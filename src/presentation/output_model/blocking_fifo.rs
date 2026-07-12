//! BlockingFifo adapter — device-paced blocking writes for USB DACs (LaserDock).
//!
//! Unlike [`NetworkFifoAdapter`](super::network_fifo::NetworkFifoAdapter), which
//! meters writes off a software buffer estimator, this adapter writes large
//! fixed-size chunks straight into a *blocking* bulk endpoint and lets the
//! device's own backpressure set the pace. The LaserDock ring is small (768
//! samples ≈ 25ms at 30kpps) and only drains while output is enabled, so the
//! estimator-driven deficit trickle degenerates into starving sub-packet writes
//! (~1–4 samples each) that cap effective throughput well below the commanded
//! rate. See the `usb_probe` example for the measurement that motivated this.
//!
//! Two invariants keep the blocking model safe:
//!
//! 1. **Fixed-size chunks.** Every write is [`CHUNK_POINTS`] points (a multiple
//!    of the device's 64-sample bulk packet), large enough to amortise the
//!    ~85µs per-transfer overhead. The blocking endpoint stalls each write until
//!    the ring has room, which is exactly the pacing signal we want.
//!
//! 2. **No writes while disarmed.** With output disabled the device consumes
//!    nothing, so a blocking write would wedge forever once the ring fills. The
//!    adapter issues no writes while disarmed and idles instead; on re-arm it
//!    clears the ring (via [`BackendKind::reset_device_buffer`]) so stale points
//!    queued before the gap never replay.

use std::time::Duration;

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::content_source::{ContentSourceKind, FifoContentSource};
use super::{LoopCtx, OutputModelAdapter, StepOutcome};

/// Points per blocking write. 512 = 8 × the LaserDock bulk packet (64) and
/// ≈17ms at 30kpps — comfortably above the ring drain rate (no underruns) while
/// bounding arm/disarm/stop latency to one chunk period. Clamped to the
/// device's `max_points_per_chunk`.
const CHUNK_POINTS: usize = 512;

/// Idle poll interval while disarmed (device consumes nothing, so there is
/// nothing to write). Short enough that re-arm and stop are handled promptly.
const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Re-borrow the loop-context source as a `FifoContentSource`. BlockingFifo is
/// only ever paired with a Fifo source by the driver; mismatch is a programmer
/// error.
fn fifo_source<'a>(source: &'a mut ContentSourceKind<'_>) -> &'a mut dyn FifoContentSource {
    match source {
        ContentSourceKind::Fifo(s) => &mut **s,
        ContentSourceKind::Frame(_) => {
            unreachable!("BlockingFifoAdapter requires a Fifo content source")
        }
    }
}

pub(crate) struct BlockingFifoAdapter {
    chunk_points: usize,
    /// True while a produced-but-unwritten slice is held in the source cache
    /// (retain-on-`WouldBlock`).
    has_retain: bool,
    /// Tracks the armed edge so the ring can be cleared exactly once on re-arm.
    was_armed: bool,
}

impl BlockingFifoAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        Self {
            chunk_points: CHUNK_POINTS.min(backend.caps().max_points_per_chunk),
            has_retain: false,
            was_armed: false,
        }
    }
}

impl OutputModelAdapter for BlockingFifoAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        // Disarmed: output is disabled and the device drains nothing, so a
        // blocking write would wedge. Idle (processing control messages) until
        // re-armed.
        if !ctx.is_armed {
            self.was_armed = false;
            if let Err(stopped) = ctx.sleep_with_control_check(IDLE_SLEEP) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        // Rising arm edge: the ring still holds whatever was queued before the
        // disarm (the device never consumed it). Clear it so stale points don't
        // replay, and drop any slice retained from before the gap.
        if !self.was_armed {
            self.was_armed = true;
            if let Err(e) = ctx.backend.reset_device_buffer() {
                log::debug!("reset_device_buffer on re-arm failed (non-fatal): {e}");
            }
            fifo_source(&mut ctx.source).discard_cached();
            self.has_retain = false;
        }

        let pps = ctx.pps;
        let chunk = self.chunk_points;
        fifo_source(&mut ctx.source).reserve_buf(chunk);

        if !self.has_retain {
            if fifo_source(&mut ctx.source)
                .produce_chunk(chunk, pps, ctx.is_armed)
                .is_empty()
            {
                // No content available right now (frame-mode with no frame yet,
                // or the producer ended). Idle briefly and let the driver's
                // end-of-stream check run.
                ctx.sleep_and_mark_activity(Duration::from_millis(1));
                return StepOutcome::Continue;
            }
            self.has_retain = true;
        }

        // Blocking write: `try_write` stalls until the device ring accepts the
        // whole chunk, which is the pacing we rely on.
        let (n, outcome) = match fifo_source(&mut ctx.source).cached_slice() {
            Some(slice) => (slice.len(), ctx.backend.try_write(pps, slice)),
            None => {
                self.has_retain = false;
                return StepOutcome::Continue;
            }
        };

        match outcome {
            Ok(WriteOutcome::Written) => {
                ctx.metrics.mark_write_success();
                fifo_source(&mut ctx.source).commit_written(n, ctx.is_armed);
                self.has_retain = false;
            }
            Ok(WriteOutcome::WouldBlock) => {
                // The LaserDock data endpoint blocks rather than returning
                // WouldBlock, but a future BlockingFifo backend might not. Keep
                // the retain and retry on the next step, backing off briefly so a
                // non-blocking endpoint doesn't spin the CPU at 100%.
                if ctx.control.is_stop_requested() {
                    return StepOutcome::Stopped;
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
        StepOutcome::Continue
    }

    fn on_reconnect(&mut self, info: &DacInfo, _backend: &mut BackendKind) {
        self.chunk_points = CHUNK_POINTS.min(info.caps.max_points_per_chunk);
        self.has_retain = false;
        // Force a ring clear on the first armed step after reconnect.
        self.was_armed = false;
    }

    fn drain_and_blank(&mut self, ctx: &mut LoopCtx<'_>, timeout: Duration) {
        // While output is still enabled the device keeps draining, so the
        // estimator poll lets the final chunk play out before we blank + close.
        super::drain_via_estimator(ctx, timeout);
        // `blank_and_close_shutter` disables output *before* its (blocking) write.
        // On a device that only drains while enabled, a still-full ring would
        // wedge that write forever — which happens whenever the drain above was
        // capped short (`drain_timeout == 0`, as for every FrameSession, or the
        // timeout elapsing mid-playout) and the blocking writer left the ring
        // near-full. Clear the ring first so the trailing blank always has room
        // and returns promptly.
        let _ = ctx.backend.reset_device_buffer();
        super::blank_and_close_shutter(ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
    use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
    use crate::config::IdlePolicy;
    use crate::device::{DacCapabilities, DacType, OutputModel};
    use crate::error::Result as DacResult;
    use crate::point::LaserPoint;
    use crate::presentation::content_source::{ContentSourceKind, FifoContentSource};
    use crate::presentation::engine::PresentationEngine;
    use crate::presentation::output_model::{
        drain_via_estimator, LoopCtx, OutputModelAdapter, StepOutcome, SystemClock,
    };
    use crate::presentation::session::FrameSessionMetrics;
    use crate::presentation::slice_pipeline::SlicePipeline;
    use crate::presentation::{Frame, TransitionPlan};
    use crate::stream::{ControlMsg, StreamControl};

    use super::{BlockingFifoAdapter, CHUNK_POINTS};

    /// Fake BlockingFifo backend that records every write and every ring clear.
    struct FakeBlockingFifo {
        caps: DacCapabilities,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
        resets: Arc<AtomicUsize>,
        estimator: Box<dyn BufferEstimator>,
    }

    impl DacBackend for FakeBlockingFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("FakeBlockingFifo".into())
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

    impl FifoBackend for FakeBlockingFifo {
        fn try_write_points(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            self.writes.lock().unwrap().push(points.to_vec());
            Ok(WriteOutcome::Written)
        }
        fn estimator(&self) -> &dyn BufferEstimator {
            &*self.estimator
        }
        fn reset_device_buffer(&mut self) -> DacResult<()> {
            self.resets.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn caps() -> DacCapabilities {
        DacCapabilities {
            pps_min: 1,
            pps_max: 35_000,
            max_points_per_chunk: 4096,
            output_model: OutputModel::BlockingFifo,
        }
    }

    fn frame_with_points(n: usize) -> Frame {
        let pts: Vec<LaserPoint> = (0..n)
            .map(|i| LaserPoint::new(i as f32 * 0.0001, 0.0, 1000, 1000, 1000, 1000))
            .collect();
        Frame::new(pts)
    }

    struct Harness {
        backend: BackendKind,
        adapter: BlockingFifoAdapter,
        pipeline: SlicePipeline,
        control: StreamControl,
        rx: mpsc::Receiver<ControlMsg>,
        metrics: FrameSessionMetrics,
        shutter: bool,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
        resets: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new() -> Self {
            let writes = Arc::new(Mutex::new(Vec::new()));
            let resets = Arc::new(AtomicUsize::new(0));
            let backend = FakeBlockingFifo {
                caps: caps(),
                writes: Arc::clone(&writes),
                resets: Arc::clone(&resets),
                estimator: Box::new(SoftwareDecayEstimator::new()),
            };
            let backend = BackendKind::Fifo(Box::new(backend));
            let adapter = BlockingFifoAdapter::new(&backend);

            let mut engine =
                PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
            engine.set_pending(frame_with_points(474));
            let pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

            let (tx, rx) = mpsc::channel::<ControlMsg>();
            let control = StreamControl::new(tx, std::time::Duration::ZERO, 30_000);
            let metrics = FrameSessionMetrics::new(true);
            Self {
                backend,
                adapter,
                pipeline,
                control,
                rx,
                metrics,
                shutter: false,
                writes,
                resets,
            }
        }

        fn step(&mut self, is_armed: bool) -> StepOutcome {
            let source = ContentSourceKind::Fifo(&mut self.pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut self.backend,
                source,
                control: &self.control,
                control_rx: &self.rx,
                metrics: &self.metrics,
                shutter_open: &mut self.shutter,
                error_sink: &mut |_| {},
                target_buffer: std::time::Duration::from_millis(20),
                pps: 30_000,
                is_armed,
                clock: &SystemClock,
            };
            self.adapter.step(&mut ctx)
        }

        fn drain_and_blank(&mut self, timeout: std::time::Duration) {
            let source = ContentSourceKind::Fifo(&mut self.pipeline as &mut dyn FifoContentSource);
            let mut ctx = LoopCtx {
                backend: &mut self.backend,
                source,
                control: &self.control,
                control_rx: &self.rx,
                metrics: &self.metrics,
                shutter_open: &mut self.shutter,
                error_sink: &mut |_| {},
                target_buffer: std::time::Duration::from_millis(20),
                pps: 30_000,
                is_armed: true,
                clock: &SystemClock,
            };
            self.adapter.drain_and_blank(&mut ctx, timeout);
        }
    }

    #[test]
    fn armed_step_writes_fixed_chunk_size() {
        let mut h = Harness::new();
        assert!(matches!(h.step(true), StepOutcome::Continue));
        let writes = h.writes.lock().unwrap();
        assert_eq!(writes.len(), 1, "one blocking write per armed step");
        assert_eq!(
            writes[0].len(),
            CHUNK_POINTS,
            "chunk should be the fixed CHUNK_POINTS, not an estimator-derived trickle"
        );
    }

    #[test]
    fn disarmed_step_issues_no_write() {
        let mut h = Harness::new();
        assert!(matches!(h.step(false), StepOutcome::Continue));
        assert_eq!(
            h.writes.lock().unwrap().len(),
            0,
            "no blocking write while disarmed (would wedge on a disabled device)"
        );
    }

    #[test]
    fn rearm_clears_ring_exactly_once_per_edge() {
        let mut h = Harness::new();
        // First armed step: rising edge from the initial disarmed state → clear.
        h.step(true);
        assert_eq!(h.resets.load(Ordering::SeqCst), 1);
        // Staying armed does not re-clear.
        h.step(true);
        assert_eq!(h.resets.load(Ordering::SeqCst), 1);
        // Disarm, then re-arm: a second clear on the new rising edge.
        h.step(false);
        h.step(true);
        assert_eq!(
            h.resets.load(Ordering::SeqCst),
            2,
            "ring should be cleared once per re-arm, not per armed step"
        );
    }

    #[test]
    fn drain_and_blank_clears_ring_before_trailing_blank() {
        // Regression: `blank_and_close_shutter` disables output and then issues a
        // blocking write; on a still-full ring (drain capped at timeout 0) that
        // write would wedge the thread forever. `drain_and_blank` must clear the
        // ring first, guaranteeing room for the trailing blank.
        let mut h = Harness::new();
        h.step(true); // arm + one write, leaving the ring non-empty
        let before = h.resets.load(Ordering::SeqCst);
        h.drain_and_blank(std::time::Duration::ZERO);
        assert!(
            h.resets.load(Ordering::SeqCst) > before,
            "ring must be cleared before the trailing blank write so it can't wedge"
        );
    }

    /// Estimator that records the `pps` it is polled at and always reports the
    /// queue empty, so the drain loop polls exactly once and exits immediately.
    struct CapturingEstimator {
        queried_pps: Arc<Mutex<Vec<u32>>>,
    }

    impl BufferEstimator for CapturingEstimator {
        fn estimated_fullness(&self, _now: std::time::Instant, pps: u32) -> u64 {
            self.queried_pps.lock().unwrap().push(pps);
            0
        }
    }

    /// Regression (audit finding #2): when the configured `pps` exceeds the
    /// connected device's real max, `write_frame` clamps the programmed rate and
    /// `record_send` anchors the estimator at that *effective* (clamped) rate.
    /// The drain poll must read the estimator at the SAME clamped rate — reading
    /// at the unclamped configured `pps` decays the software model faster than
    /// the hardware and blanks the tail of the final frame early.
    #[test]
    fn drain_via_estimator_polls_at_device_clamped_rate() {
        const DEVICE_MAX: u32 = 30_000; // connected device's real max_dac_rate
        const CONFIGURED_PPS: u32 = 33_000; // survived pre-connect validation

        let queried_pps = Arc::new(Mutex::new(Vec::new()));
        let mut device_caps = caps();
        device_caps.pps_max = DEVICE_MAX;
        let backend = FakeBlockingFifo {
            caps: device_caps,
            writes: Arc::new(Mutex::new(Vec::new())),
            resets: Arc::new(AtomicUsize::new(0)),
            estimator: Box::new(CapturingEstimator {
                queried_pps: Arc::clone(&queried_pps),
            }),
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));

        let mut engine =
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(64));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = StreamControl::new(tx, std::time::Duration::ZERO, CONFIGURED_PPS);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = true;

        let source = ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource);
        let mut ctx = LoopCtx {
            backend: &mut backend,
            source,
            control: &control,
            control_rx: &rx,
            metrics: &metrics,
            shutter_open: &mut shutter,
            error_sink: &mut |_| {},
            target_buffer: std::time::Duration::from_millis(20),
            pps: CONFIGURED_PPS,
            is_armed: true,
            clock: &SystemClock,
        };
        // Non-zero timeout so the drain loop actually polls the estimator.
        drain_via_estimator(&mut ctx, std::time::Duration::from_secs(1));

        let polled = queried_pps.lock().unwrap();
        assert!(
            !polled.is_empty(),
            "drain must poll the estimator at least once"
        );
        assert!(
            polled.iter().all(|&p| p == DEVICE_MAX),
            "drain must poll at the device-clamped rate {DEVICE_MAX}, not the \
             configured {CONFIGURED_PPS}; got {polled:?}"
        );
    }
}
