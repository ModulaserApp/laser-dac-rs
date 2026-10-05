//! NetworkFifo adapter — buffer-estimation pacing for FIFO-style DACs
//! (Ether Dream, LaserCube, AVB, oscilloscope).

use std::time::Duration;

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;
use crate::error::Error;

use super::super::content_source::ContentSourceKind;
use super::{
    blank_and_close_shutter, close_if_disarmed, estimator_fullness, process_control_messages,
    ControlAction, LoopCtx, OutputModelAdapter, StepOutcome,
};

/// Minimum write quantum: don't dribble out tiny chunks. When the buffer
/// deficit is below `max(this many seconds of points, MIN_WRITE_QUANTUM_FLOOR)`
/// we wait for it to grow instead of issuing a few-point write — otherwise the
/// steady state degenerates to ~1000 tiny writes/sec (pure overhead, and an
/// amplifier for any estimator rounding bias).
const MIN_WRITE_QUANTUM_SECS: f64 = 0.005;
const MIN_WRITE_QUANTUM_FLOOR: usize = 16;

/// Continuous `WouldBlock` for longer than this is treated as a stalled/dead
/// link so reconnect engages instead of spinning forever.
const WOULDBLOCK_STALL_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct NetworkFifoAdapter {
    max_points: usize,
    /// Rate of the last accepted write. Network DACs learn a new rate from the
    /// next data write, so until then the buffer keeps draining at this one.
    written_pps: Option<u32>,
}

impl NetworkFifoAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        Self {
            max_points: backend.caps().max_points_per_chunk,
            written_pps: None,
        }
    }
}

impl OutputModelAdapter for NetworkFifoAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        let pps = ctx.pps;
        let pps_f64 = pps as f64;
        // The requested target, clamped to the device's ceiling. A target
        // above the ring (50 ms at 100 kpps is 5000 points, the ED2 ring holds
        // 3899) can never be reached, so the deficit would never close and
        // every chunk would be too large to fit. Read every step: the ceiling
        // can change when the backend reconnects.
        let requested_points = ctx.target_buffer.as_secs_f64() * pps_f64;
        let target_buffer_f64 = match ctx.backend.target_buffer_ceiling() {
            // At least one point, so a zero ceiling cannot stall the adapter.
            Some(ceiling) => requested_points.min(ceiling.max(1) as f64),
            None => requested_points,
        };
        let target_buffer_points = target_buffer_f64 as u64;

        // Minimum write quantum: if the deficit is below the quantum, sleep until
        // enough points have drained to justify a full-sized write instead of
        // producing a tiny chunk now. Capped to the target buffer itself (the
        // largest deficit ever possible) so the gate never wedges when the whole
        // target is smaller than the quantum (very low pps), and to `max_points`.
        let quantum = ((MIN_WRITE_QUANTUM_SECS * pps_f64).ceil() as usize)
            .max(MIN_WRITE_QUANTUM_FLOOR)
            .min(self.max_points)
            .min(target_buffer_points as usize);

        // Network DACs learn a new rate from the next data write and keep
        // draining at the old one until then. Judging the buffer at the new
        // rate after a large drop sleeps for many times the real drain time
        // and underflows the device. So a changed rate is written right away
        // with one quantum of points, whatever the buffer, so the device
        // learns it promptly. After that the backend's estimator has to
        // account for buffered content queued at the old rate.
        let drain_pps = self.written_pps.unwrap_or(pps);
        let rate_pending = drain_pps != pps;
        let buffered = estimator_fullness(ctx.backend.estimator(), drain_pps);

        if buffered > target_buffer_points && !rate_pending {
            let excess = buffered - target_buffer_points;
            let sleep_time = Duration::from_secs_f64(excess as f64 / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(sleep_time) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        let deficit = (target_buffer_f64 - buffered as f64).max(0.0);
        let mut target_points = (deficit.ceil() as usize).min(self.max_points);
        if rate_pending {
            target_points = target_points.max(quantum).max(1);
        }
        if target_points == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        if target_points < quantum {
            let shortfall = (quantum - target_points) as f64;
            let wait = Duration::from_secs_f64(shortfall / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(wait) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        // The adapter/source pairing is validated once at construction
        // (`for_backend`); this guard keeps any future wiring mistake a
        // reported error instead of a panic.
        let source = match &mut ctx.source {
            ContentSourceKind::Fifo(s) => s,
            ContentSourceKind::Frame(_) => {
                return super::source_mismatch(ctx, "NetworkFifoAdapter");
            }
        };

        let n = source.produce_chunk(target_points, pps, ctx.is_armed).len();
        if n == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        // Inner WouldBlock spin: ~100µs hardware drain assumption. A
        // disarm or arm during the spin hands control back to the driver;
        // an unchanged state (including staying disarmed, e.g. while the DAC
        // sits in e-stop) keeps the backoff and the stall timeout.
        let spin_start = ctx.clock.now();
        let desired_at_start = ctx.control.desired_state();
        loop {
            let outcome = match source.cached_slice() {
                Some(slice) => ctx.backend.try_write(pps, slice),
                None => return StepOutcome::Continue,
            };
            match outcome {
                Ok(WriteOutcome::Written) => {
                    ctx.metrics.mark_write_success();
                    self.written_pps = Some(pps);
                    source.commit_written(n, ctx.is_armed);
                    break;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    ctx.metrics.mark_loop_activity();
                    std::thread::yield_now();
                    if ctx.control.is_stop_requested() {
                        return StepOutcome::Stopped;
                    }
                    // Process control messages inside the spin: a Disarm
                    // (shutter close) is safety-critical and must take effect
                    // promptly rather than waiting for the device to accept the
                    // pending write, which may never happen while stalled.
                    let action = process_control_messages(ctx.control_rx);
                    close_if_disarmed(ctx.control, ctx.shutter_open, ctx.backend);
                    match action {
                        ControlAction::Stop => return StepOutcome::Stopped,
                        ControlAction::StateChanged => return StepOutcome::StateChanged,
                        ControlAction::None
                            if ctx.control.is_armed() != ctx.is_armed
                                || ctx.control.desired_state() != desired_at_start =>
                        {
                            return StepOutcome::StateChanged;
                        }
                        ControlAction::None => {}
                    }
                    // Bounded staleness: a device wedged in continuous WouldBlock
                    // is treated as disconnected so reconnect can engage.
                    if ctx.clock.now().saturating_duration_since(spin_start)
                        >= WOULDBLOCK_STALL_TIMEOUT
                    {
                        log::warn!("write stalled (WouldBlock > {WOULDBLOCK_STALL_TIMEOUT:?}), treating as disconnect");
                        let _ = ctx.backend.disconnect();
                        (ctx.error_sink)(Error::disconnected(
                            "write stalled: continuous WouldBlock",
                        ));
                        return StepOutcome::Disconnected;
                    }
                    // Inlined `sleep_and_mark_activity`: `source` is still
                    // borrowed from `ctx.source` for the next spin iteration,
                    // so only field-disjoint accesses are allowed here.
                    ctx.clock.sleep(Duration::from_micros(100));
                    ctx.metrics.mark_loop_activity();
                }
                Err(e) if e.is_stopped() => return StepOutcome::Stopped,
                Err(e) if e.is_disconnected() => {
                    (ctx.error_sink)(e);
                    return StepOutcome::Disconnected;
                }
                Err(e) => {
                    log::warn!("write error, disconnecting backend: {e}");
                    let _ = ctx.backend.disconnect();
                    (ctx.error_sink)(e);
                    return StepOutcome::Disconnected;
                }
            }
        }
        StepOutcome::Continue
    }

    fn on_reconnect(&mut self, info: &DacInfo, _backend: &mut BackendKind) {
        self.max_points = info.caps.max_points_per_chunk;
        self.written_pps = None;
    }

    fn drain_and_blank(&mut self, ctx: &mut LoopCtx<'_>, timeout: Duration) {
        super::drain_via_estimator(ctx, timeout);
        blank_and_close_shutter(ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
    use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
    use crate::config::IdlePolicy;
    use crate::device::{DacCapabilities, DacType, OutputModel};
    use crate::error::Result as DacResult;
    use crate::point::LaserPoint;
    use crate::presentation::content_source::{ContentSourceKind, FifoContentSource};
    use crate::presentation::engine::PresentationEngine;
    use crate::presentation::output_model::{
        FakeClock, LoopCtx, OutputModelAdapter, StepOutcome, SystemClock,
    };
    use crate::presentation::session::FrameSessionMetrics;
    use crate::presentation::slice_pipeline::SlicePipeline;
    use crate::presentation::{Frame, TransitionPlan};
    use crate::session::{ControlMsg, SessionControl};

    use super::NetworkFifoAdapter;

    struct FakeFifo {
        caps: DacCapabilities,
        always_block: bool,
        writes: Arc<Mutex<Vec<usize>>>,
        shutter_calls: Arc<Mutex<Vec<bool>>>,
        estimator: SoftwareDecayEstimator,
    }

    impl DacBackend for FakeFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("FakeFifo".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn set_shutter(&mut self, open: bool) -> DacResult<()> {
            self.shutter_calls.lock().unwrap().push(open);
            Ok(())
        }
    }

    impl FifoBackend for FakeFifo {
        fn try_write_points(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            if self.always_block {
                return Ok(WriteOutcome::WouldBlock);
            }
            self.writes.lock().unwrap().push(points.len());
            Ok(WriteOutcome::Written)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    fn frame_with_points(n: usize) -> Frame {
        let pts: Vec<LaserPoint> = (0..n)
            .map(|i| LaserPoint::new(i as f32 * 0.0001, 0.0, 1000, 1000, 1000, 1000))
            .collect();
        Frame::new(pts)
    }

    fn caps(max_points: usize) -> DacCapabilities {
        DacCapabilities {
            pps_min: 1_000,
            pps_max: 100_000,
            max_points_per_chunk: max_points,
            output_model: OutputModel::NetworkFifo,
        }
    }

    /// Counts what was written and reports it as buffered, with no drain,
    /// so an adapter run is deterministic.
    struct HoldingEstimator(Arc<Mutex<Vec<usize>>>);

    impl BufferEstimator for HoldingEstimator {
        fn estimated_fullness(&self, _now: Instant, _pps: u32) -> u64 {
            self.0.lock().unwrap().iter().sum::<usize>() as u64
        }
    }

    /// FIFO backend with a configurable target ceiling whose buffer never
    /// drains.
    struct CeilingFifo {
        caps: DacCapabilities,
        ceiling: Option<usize>,
        writes: Arc<Mutex<Vec<usize>>>,
        estimator: HoldingEstimator,
    }

    impl DacBackend for CeilingFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("CeilingFifo".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _open: bool) -> DacResult<()> {
            Ok(())
        }
    }

    impl FifoBackend for CeilingFifo {
        fn try_write_points(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            self.writes.lock().unwrap().push(points.len());
            Ok(WriteOutcome::Written)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }

        fn target_buffer_ceiling(&self) -> Option<usize> {
            self.ceiling
        }
    }

    /// Run `steps` adapter steps at 100 kpps with a 50 ms target (5000
    /// points) against a 3899-point device and return the write sizes.
    fn writes_with_ceiling(ceiling: Option<usize>, steps: usize) -> Vec<usize> {
        const PPS: u32 = 100_000;
        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = CeilingFifo {
            caps: caps(3_899),
            ceiling,
            writes: Arc::clone(&writes),
            estimator: HoldingEstimator(Arc::clone(&writes)),
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);
        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        for _ in 0..steps {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(50),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        let written = writes.lock().unwrap().clone();
        written
    }

    /// Regression: the target (50 ms at 100 kpps = 5000 points) was not
    /// clamped to the device. Against a 3899-point Ether Dream 2 ring the
    /// deficit never closed and the adapter kept producing chunks that could
    /// not fit. With a ceiling it stops writing once the estimate reaches it.
    #[test]
    fn target_is_clamped_to_the_backend_ceiling() {
        let w = writes_with_ceiling(Some(3_119), 4);
        assert_eq!(w, vec![3_119], "one write up to the ceiling, then none");
    }

    /// Without a ceiling the adapter aims for the full requested target.
    #[test]
    fn no_ceiling_keeps_the_requested_target() {
        let w = writes_with_ceiling(None, 4);
        assert_eq!(w, vec![3_899, 1_101], "chunk-capped, then up to 5000");
    }

    /// A deficit below the minimum write quantum must NOT produce a tiny write —
    /// the adapter waits for the buffer to drain enough for a full-sized chunk.
    #[test]
    fn deficit_below_quantum_is_not_written() {
        const PPS: u32 = 30_000;
        // 20ms target = 600 points; quantum = ceil(5ms * 30k) = 150 points.
        // Pre-fill to 595 → deficit ~5 points, well below the quantum.
        // Anchor the send one second in the future so the estimator does not
        // decay during the (real-time) construction between here and the read
        // inside `step` — otherwise a slow/loaded runner could drain enough
        // points to push the deficit across the quantum and flake the test.
        let mut estimator = SoftwareDecayEstimator::new();
        estimator.record_send(Instant::now() + Duration::from_secs(1), 595, PPS);

        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator,
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        assert!(
            writes.lock().unwrap().is_empty(),
            "sub-quantum deficit must not be written"
        );
    }

    /// A deficit at/above the quantum is written normally.
    #[test]
    fn deficit_above_quantum_is_written() {
        const PPS: u32 = 30_000;
        let mut estimator = SoftwareDecayEstimator::new();
        estimator.record_send(Instant::now(), 100, PPS); // deficit ~500 points

        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator,
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1, "above-quantum deficit should be written");
        assert!(w[0] >= 150, "written chunk should be at least the quantum");
    }

    /// A Disarm arriving while the write is spinning on `WouldBlock` must be
    /// processed inside the spin (shutter closes promptly) — a safety concern.
    #[test]
    fn disarm_during_wouldblock_spin_closes_shutter() {
        const PPS: u32 = 30_000;
        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let shutter_calls = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: true, // never accepts the write → perpetual spin
            writes: Arc::new(Mutex::new(Vec::new())),
            shutter_calls: Arc::clone(&shutter_calls),
            estimator: SoftwareDecayEstimator::new(), // empty → deficit above quantum
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        // Queue a Disarm (should close the shutter) followed by Stop (exits the
        // spin). Both are drained in one `process_control_messages` call.
        tx.send(ControlMsg::Disarm).unwrap();
        tx.send(ControlMsg::Stop).unwrap();

        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open; // armed/open
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Stopped));
        }
        assert_eq!(
            shutter,
            crate::presentation::output_model::ShutterState::Closed,
            "shutter must be closed by the in-spin Disarm"
        );
        assert_eq!(
            shutter_calls.lock().unwrap().as_slice(),
            &[false],
            "exactly one set_shutter(false) from the Disarm"
        );
    }

    /// `on_reconnect` must adopt the reconnected device's `max_points_per_chunk`
    /// so the next write is clamped to the new (smaller) capacity.
    #[test]
    fn on_reconnect_updates_max_points() {
        use crate::device::{DacInfo, DacType};
        const PPS: u32 = 30_000;

        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator: SoftwareDecayEstimator::new(), // empty → large deficit
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend); // max_points = 4096

        // Reconnect to a device advertising a much smaller chunk capacity.
        let info = DacInfo {
            id: "fakefifo:reconnect".to_string(),
            name: "FakeFifo".to_string(),
            kind: DacType::Custom("FakeFifo".to_string()),
            caps: caps(3),
        };
        adapter.on_reconnect(&info, &mut backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1, "one write after reconnect");
        assert_eq!(
            w[0], 3,
            "write must be clamped to the reconnected device's max_points_per_chunk"
        );
    }

    /// When the estimator reports more buffered than the target, the adapter
    /// sleeps to let the queue drain and issues no write this step.
    #[test]
    fn buffered_above_target_sleeps_without_writing() {
        const PPS: u32 = 30_000;
        // 20ms target = 600 points. Anchor the send in the future so the
        // estimator does not decay below the target during construction, then
        // prefill to 800 → buffered above target.
        let mut estimator = SoftwareDecayEstimator::new();
        estimator.record_send(Instant::now() + Duration::from_secs(1), 800, PPS);

        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator,
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        assert!(
            writes.lock().unwrap().is_empty(),
            "a buffer above target must not be topped up with a write"
        );
    }

    /// `drain_and_blank` closes the shutter and emits a trailing blank chunk.
    #[test]
    fn drain_and_blank_closes_shutter_and_writes_blank() {
        const PPS: u32 = 30_000;
        let mut engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );

        let writes = Arc::new(Mutex::new(Vec::new()));
        let shutter_calls = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::clone(&shutter_calls),
            estimator: SoftwareDecayEstimator::new(),
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = NetworkFifoAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: PPS,
                is_armed: true,
                clock: &SystemClock,
            };
            // Zero timeout → drain is a no-op; only the blank-and-close runs.
            adapter.drain_and_blank(&mut ctx, Duration::ZERO);
        }
        assert_eq!(
            shutter,
            crate::presentation::output_model::ShutterState::Closed,
            "drain_and_blank must close the shutter"
        );
        assert_eq!(
            shutter_calls.lock().unwrap().as_slice(),
            &[false],
            "exactly one set_shutter(false) from drain_and_blank"
        );
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1, "one trailing blank write");
        assert_eq!(w[0], 16, "the trailing blank chunk is 16 points");
    }

    /// Build a minimal `LoopCtx`-ready scaffold (backend/source/control/metrics)
    /// for exercising the pacing-sleep seam. Returns everything by value so the
    /// caller can borrow it into a `LoopCtx` with whatever clock it wants.
    fn sleep_scaffold() -> (
        BackendKind,
        SlicePipeline,
        SessionControl,
        mpsc::Receiver<ControlMsg>,
        FrameSessionMetrics,
    ) {
        const PPS: u32 = 30_000;
        let engine =
            PresentationEngine::new(Box::new(|_, _, _| TransitionPlan::Transition(Vec::new())));
        let pipeline = SlicePipeline::new(
            engine,
            std::time::Duration::ZERO,
            None,
            IdlePolicy::Blank,
            0,
        );
        let backend = BackendKind::Fifo(Box::new(FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::new(Mutex::new(Vec::new())),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator: SoftwareDecayEstimator::new(),
        }));
        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = SessionControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        (backend, pipeline, control, rx, metrics)
    }

    /// The pacing sleep runs entirely on the injected clock: a 30s wait
    /// completes with no wall-clock delay while virtual time advances the full
    /// duration. This is the determinism the clock seam exists to enable.
    #[test]
    fn pacing_sleep_uses_injected_clock_not_wall_clock() {
        let (mut backend, mut pipeline, control, rx, metrics) = sleep_scaffold();
        let clock = FakeClock::new();
        let mut shutter = crate::presentation::output_model::ShutterState::Open;

        let wall_start = Instant::now();
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: 30_000,
                is_armed: true,
                clock: &clock,
            };
            assert!(ctx
                .sleep_with_control_check(Duration::from_secs(30))
                .is_ok());
        }

        // No real time was spent (generous bound to avoid CI flakes)...
        assert!(
            wall_start.elapsed() < Duration::from_secs(1),
            "sleep must not block on the wall clock"
        );
        // ...but virtual time advanced the full requested duration.
        assert_eq!(clock.total_slept(), Duration::from_secs(30));
    }

    /// A stop requested before the wait returns `Stopped` promptly through the
    /// injected clock, without real sleeping.
    #[test]
    fn pacing_sleep_returns_stopped_on_stop_request() {
        let (mut backend, mut pipeline, control, rx, metrics) = sleep_scaffold();
        let clock = FakeClock::new();
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        control.stop().unwrap();

        let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
        let mut ctx = LoopCtx {
            backend: &mut backend,
            source,
            control: &control,
            control_rx: &rx,
            metrics: &metrics,
            shutter_open: &mut shutter,
            error_sink: &mut |_| {},
            target_buffer: Duration::from_millis(20),
            pps: 30_000,
            is_armed: true,
            clock: &clock,
        };
        assert!(matches!(
            ctx.sleep_with_control_check(Duration::from_secs(30)),
            Err(StepOutcome::Stopped)
        ));
        // Returned on the first slice, not after the full 30s of virtual time.
        assert!(clock.total_slept() < Duration::from_secs(1));
    }

    /// Always answers WouldBlock and counts how often it was asked. Optionally
    /// arms the session after a number of calls.
    struct CountingBlocker {
        caps: DacCapabilities,
        calls: Arc<Mutex<usize>>,
        arm_after: Option<(usize, SessionControl)>,
        estimator: SoftwareDecayEstimator,
    }

    impl DacBackend for CountingBlocker {
        fn dac_type(&self) -> DacType {
            DacType::Custom("CountingBlocker".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _open: bool) -> DacResult<()> {
            Ok(())
        }
    }

    impl FifoBackend for CountingBlocker {
        fn try_write_points(
            &mut self,
            _pps: u32,
            _points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if let Some((n, control)) = &self.arm_after {
                if *calls == *n {
                    control.arm().unwrap();
                }
            }
            Ok(WriteOutcome::WouldBlock)
        }
        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    /// Run one step against a backend that always blocks, with the session
    /// disarmed and a fake clock. Returns the outcome, the try_write count and
    /// the virtual time spent.
    fn step_disarmed_against_blocker(arm_after: Option<usize>) -> (StepOutcome, usize, Duration) {
        let (_, mut pipeline, control, rx, metrics) = sleep_scaffold();
        assert!(!control.is_armed());
        let calls = Arc::new(Mutex::new(0));
        let mut backend = BackendKind::Fifo(Box::new(CountingBlocker {
            caps: caps(4_096),
            calls: Arc::clone(&calls),
            arm_after: arm_after.map(|n| (n, control.clone())),
            estimator: SoftwareDecayEstimator::new(),
        }));
        let mut adapter = NetworkFifoAdapter::new(&backend);
        let clock = FakeClock::new();
        let mut shutter = crate::presentation::output_model::ShutterState::Closed;
        let outcome = {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(20),
                pps: 30_000,
                is_armed: false,
                clock: &clock,
            };
            adapter.step(&mut ctx)
        };
        let calls = *calls.lock().unwrap();
        (outcome, calls, clock.total_slept())
    }

    /// Regression: while disarmed, a WouldBlock returned to the driver at
    /// once, skipping the backoff sleep and the stall timeout. During an
    /// e-stop that meant hundreds of thousands of writes per second.
    #[test]
    fn disarmed_wouldblock_backs_off_and_hits_the_stall_timeout() {
        let (outcome, calls, slept) = step_disarmed_against_blocker(None);
        assert!(matches!(outcome, StepOutcome::Disconnected));
        assert!(slept >= super::WOULDBLOCK_STALL_TIMEOUT, "slept {slept:?}");
        // One attempt per 100 µs backoff, over the 2 s stall timeout.
        let bound = (super::WOULDBLOCK_STALL_TIMEOUT.as_micros() / 100) as usize + 2;
        assert!(calls <= bound, "{calls} writes in {slept:?}");
    }

    /// Arming while the disarmed spin waits still hands control back to the
    /// driver, so it can open the shutter.
    #[test]
    fn arming_during_disarmed_wouldblock_returns_state_changed() {
        let (outcome, calls, _) = step_disarmed_against_blocker(Some(5));
        assert!(matches!(outcome, StepOutcome::StateChanged));
        assert_eq!(calls, 5);
    }

    /// Regression: after lowering the rate, the buffer was judged at the new
    /// rate and the adapter slept for about a second. The device keeps
    /// draining at the old rate until a write carries the change, so it
    /// underflowed. The new rate must be written at once instead.
    #[test]
    fn lowered_rate_is_written_without_waiting() {
        let (_, mut pipeline, control, rx, metrics) = sleep_scaffold();
        // 1000 buffered points, anchored in the future so no real time decays
        // them during the test.
        let mut estimator = SoftwareDecayEstimator::new();
        estimator.record_send(Instant::now() + Duration::from_secs(5), 1_000, 20_000);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut backend = BackendKind::Fifo(Box::new(FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::clone(&writes),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator,
        }));
        let mut adapter = NetworkFifoAdapter::new(&backend);
        adapter.written_pps = Some(20_000);
        let clock = FakeClock::new();
        let mut shutter = crate::presentation::output_model::ShutterState::Open;
        {
            let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut backend,
                source,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                error_sink: &mut |_| {},
                target_buffer: Duration::from_millis(50),
                pps: 1_000,
                is_armed: true,
                clock: &clock,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        assert_eq!(clock.total_slept(), Duration::ZERO);
        // One quantum: 5 ms at 1 kpps, raised to the 16-point floor.
        assert_eq!(writes.lock().unwrap().as_slice(), &[16]);
        assert_eq!(adapter.written_pps, Some(1_000));
    }
}
