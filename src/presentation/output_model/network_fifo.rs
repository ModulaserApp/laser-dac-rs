//! NetworkFifo adapter — buffer-estimation pacing for FIFO-style DACs
//! (Ether Dream, LaserCube, AVB, oscilloscope).

use std::time::{Duration, Instant};

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;
use crate::error::Error;

use super::super::content_source::{ContentSourceKind, FifoContentSource};
use super::{
    blank_and_close_shutter, estimator_fullness, process_control_messages, LoopCtx,
    OutputModelAdapter, StepOutcome,
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

/// Re-borrow the loop-context source as a `FifoContentSource`. NetworkFifo is
/// only ever paired with a Fifo source by the driver; mismatch is a programmer
/// error.
fn fifo_source<'a>(source: &'a mut ContentSourceKind<'_>) -> &'a mut dyn FifoContentSource {
    match source {
        ContentSourceKind::Fifo(s) => &mut **s,
        ContentSourceKind::Frame(_) => {
            unreachable!("NetworkFifoAdapter requires a Fifo content source")
        }
    }
}

pub(crate) struct NetworkFifoAdapter {
    max_points: usize,
}

impl NetworkFifoAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        Self {
            max_points: backend.caps().max_points_per_chunk,
        }
    }
}

impl OutputModelAdapter for NetworkFifoAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        let pps = ctx.pps;
        let pps_f64 = pps as f64;
        let target_buffer_secs = ctx.target_buffer.as_secs_f64();
        let target_buffer_points = (target_buffer_secs * pps_f64) as u64;

        let buffered = estimator_fullness(ctx.backend.estimator(), pps);

        if buffered > target_buffer_points {
            let excess = buffered - target_buffer_points;
            let sleep_time = Duration::from_secs_f64(excess as f64 / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(sleep_time) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        let deficit = (target_buffer_secs - buffered as f64 / pps_f64.max(1.0)).max(0.0);
        let target_points = ((deficit * pps_f64).ceil() as usize).min(self.max_points);
        if target_points == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        // Minimum write quantum: if the deficit is below the quantum, sleep until
        // enough points have drained to justify a full-sized write instead of
        // producing a tiny chunk now. Capped to the target buffer itself (the
        // largest deficit ever possible) so the gate never wedges when the whole
        // target is smaller than the quantum (very low pps), and to `max_points`.
        let quantum = ((MIN_WRITE_QUANTUM_SECS * pps_f64).ceil() as usize)
            .max(MIN_WRITE_QUANTUM_FLOOR)
            .min(self.max_points)
            .min(target_buffer_points as usize);
        if target_points < quantum {
            let shortfall = (quantum - target_points) as f64;
            let wait = Duration::from_secs_f64(shortfall / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(wait) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        let n = fifo_source(&mut ctx.source)
            .produce_chunk(target_points, pps, ctx.is_armed)
            .len();
        if n == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        // Inner WouldBlock spin: ~100µs hardware drain assumption.
        let spin_start = Instant::now();
        loop {
            let outcome = match fifo_source(&mut ctx.source).cached_slice() {
                Some(slice) => ctx.backend.try_write(pps, slice),
                None => return StepOutcome::Continue,
            };
            match outcome {
                Ok(WriteOutcome::Written) => {
                    ctx.metrics.mark_write_success();
                    fifo_source(&mut ctx.source).commit_written(n, ctx.is_armed);
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
                    if process_control_messages(ctx.control_rx, ctx.shutter_open, ctx.backend) {
                        return StepOutcome::Stopped;
                    }
                    // Bounded staleness: a device wedged in continuous WouldBlock
                    // is treated as disconnected so reconnect can engage.
                    if spin_start.elapsed() >= WOULDBLOCK_STALL_TIMEOUT {
                        log::warn!("write stalled (WouldBlock > {WOULDBLOCK_STALL_TIMEOUT:?}), treating as disconnect");
                        let _ = ctx.backend.disconnect();
                        (ctx.error_sink)(Error::disconnected(
                            "write stalled: continuous WouldBlock",
                        ));
                        return StepOutcome::Disconnected;
                    }
                    ctx.sleep_and_mark_activity(Duration::from_micros(100));
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
    use crate::stream::{ControlMsg, StreamControl};

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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;
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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;
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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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

        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true; // armed/open
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
        assert!(!shutter, "shutter must be closed by the in-spin Disarm");
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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;
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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;
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
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(2_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

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
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;
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
        assert!(!shutter, "drain_and_blank must close the shutter");
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
        StreamControl,
        mpsc::Receiver<ControlMsg>,
        FrameSessionMetrics,
    ) {
        const PPS: u32 = 30_000;
        let engine =
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        let pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);
        let backend = BackendKind::Fifo(Box::new(FakeFifo {
            caps: caps(4_096),
            always_block: false,
            writes: Arc::new(Mutex::new(Vec::new())),
            shutter_calls: Arc::new(Mutex::new(Vec::new())),
            estimator: SoftwareDecayEstimator::new(),
        }));
        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = StreamControl::new(tx, Duration::ZERO, PPS);
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
        let mut shutter = true;

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
        let mut shutter = true;
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
}
