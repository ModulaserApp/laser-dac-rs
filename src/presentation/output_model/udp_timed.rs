//! UdpTimed adapter — fixed-rate metronomic pacing for UDP-based DACs (e.g. IDN).

use std::time::{Duration, Instant};

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::content_source::ContentSourceKind;
use super::{
    blank_and_close_shutter, drain_via_estimator, LoopCtx, OutputModelAdapter, StepOutcome,
};

const CHUNK_SECS: f64 = 0.010;

/// After a stall, allow `next_send` to lag `now` by up to this many chunk
/// durations so lost time is made up gradually instead of discarded.
const CATCH_UP_CHUNKS: u32 = 3;

pub(crate) struct UdpTimedAdapter {
    next_send: Instant,
    chunk_points: usize,
    /// pps used to compute the current `chunk_duration`; a change here (even
    /// with the chunk size pinned at the clamp) requires a duration recompute.
    chunk_pps: u32,
    chunk_duration: Duration,
    /// True while a previously-filtered slice is held in `pipeline.cached_slice()`.
    has_retain: bool,
    max_points_per_chunk: usize,
}

impl UdpTimedAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        let max = backend.caps().max_points_per_chunk;
        Self {
            next_send: Instant::now(),
            chunk_points: 0,
            chunk_pps: 0,
            chunk_duration: Duration::from_secs_f64(CHUNK_SECS),
            has_retain: false,
            max_points_per_chunk: max,
        }
    }

    fn recompute_chunk(&mut self, pps: u32) {
        let pps_f64 = pps as f64;
        let new_chunk = ((pps_f64 * CHUNK_SECS).ceil() as usize).min(self.max_points_per_chunk);
        // Recompute the send period whenever the chunk size OR pps changes. In
        // the clamped regime (chunk pinned at `max_points_per_chunk`, e.g. IDN's
        // 179 points at pps ≥ 17.9k) a pps change leaves the chunk size the same
        // but still shifts the per-chunk duration; guarding on chunk size alone
        // would keep a stale, too-slow send period and cause continuous underrun.
        if new_chunk != self.chunk_points || pps != self.chunk_pps {
            self.chunk_points = new_chunk;
            self.chunk_pps = pps;
            self.chunk_duration = Duration::from_secs_f64(new_chunk as f64 / pps_f64.max(1.0));
        }
    }
}

impl OutputModelAdapter for UdpTimedAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        if let Err(stopped) = ctx.sleep_until_precise(self.next_send) {
            return stopped;
        }
        self.next_send += self.chunk_duration;
        let now = Instant::now();
        // Bounded catch-up: after a stall (long sleep, backpressure) `next_send`
        // may be far behind `now`. Rather than hard-clamping to `now` — which
        // discards the lost time and permanently drains the device buffer — let
        // `next_send` lag `now` by up to `CATCH_UP_CHUNKS` chunk durations so the
        // backlog is worked off over the next few sends. Clamp only beyond that
        // window to avoid an unbounded send burst.
        let max_lag = self.chunk_duration.saturating_mul(CATCH_UP_CHUNKS);
        let min_next = now.checked_sub(max_lag).unwrap_or(now);
        if self.next_send < min_next {
            self.next_send = min_next;
        }

        // pps may have changed during the sleep — re-read from control.
        let pps = ctx.control.pps();
        self.recompute_chunk(pps);

        let ContentSourceKind::Fifo(source) = &mut ctx.source else {
            unreachable!("UdpTimedAdapter requires a Fifo content source");
        };
        source.reserve_buf(self.chunk_points);

        if !self.has_retain {
            if source
                .produce_chunk(self.chunk_points, pps, ctx.is_armed)
                .is_empty()
            {
                return StepOutcome::Continue;
            }
            self.has_retain = true;
        }

        let (n, outcome) = match source.cached_slice() {
            Some(slice) => (slice.len(), ctx.backend.try_write(pps, slice)),
            None => {
                self.has_retain = false;
                return StepOutcome::Continue;
            }
        };

        match outcome {
            Ok(WriteOutcome::Written) => {
                ctx.metrics.mark_write_success();
                source.commit_written(n, ctx.is_armed);
                self.has_retain = false;
            }
            Ok(WriteOutcome::WouldBlock) => {
                ctx.metrics.mark_loop_activity();
                // leave cache; retry on next deadline
            }
            Err(e) if e.is_stopped() => {
                return StepOutcome::Stopped;
            }
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
        self.next_send = Instant::now();
        self.has_retain = false;
        self.max_points_per_chunk = info.caps.max_points_per_chunk;
        // Force a recompute on next step.
        self.chunk_points = 0;
    }

    fn drain_and_blank(&mut self, ctx: &mut LoopCtx<'_>, timeout: Duration) {
        drain_via_estimator(ctx, timeout);
        blank_and_close_shutter(ctx);
    }
}

#[cfg(test)]
mod tests {
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
    use crate::presentation::output_model::{LoopCtx, OutputModelAdapter, StepOutcome};
    use crate::presentation::session::FrameSessionMetrics;
    use crate::presentation::slice_pipeline::SlicePipeline;
    use crate::presentation::{Frame, TransitionPlan};
    use crate::stream::{ControlMsg, StreamControl};

    use super::UdpTimedAdapter;

    struct FakeFifo {
        caps: DacCapabilities,
        next_outcomes: Arc<Mutex<Vec<WriteOutcome>>>,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
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
        fn set_shutter(&mut self, _open: bool) -> DacResult<()> {
            Ok(())
        }
    }

    impl FifoBackend for FakeFifo {
        fn try_write_points(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            let outcome = {
                let mut q = self.next_outcomes.lock().unwrap();
                if q.is_empty() {
                    WriteOutcome::Written
                } else {
                    q.remove(0)
                }
            };
            if outcome == WriteOutcome::Written {
                self.writes.lock().unwrap().push(points.to_vec());
            }
            Ok(outcome)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    fn frame_with_points(n: usize) -> Frame {
        let pts: Vec<LaserPoint> = (0..n)
            .map(|i| LaserPoint::new(i as f32 * 0.001, 0.0, 1000, 1000, 1000, 1000))
            .collect();
        Frame::new(pts)
    }

    #[test]
    fn pps_change_mid_retain_writes_original_chunk_size() {
        const PPS_INITIAL: u32 = 20_000;
        const PPS_AFTER: u32 = 5_000;
        // chunk = ceil(pps * 0.010): 200 then 50.
        let expected_initial_chunk = 200;

        let mut engine =
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(400));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

        let outcomes = Arc::new(Mutex::new(vec![WriteOutcome::WouldBlock]));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFifo {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 1_000,
                output_model: OutputModel::UdpTimed,
            },
            next_outcomes: Arc::clone(&outcomes),
            writes: Arc::clone(&writes),
            estimator: SoftwareDecayEstimator::new(),
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = UdpTimedAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = StreamControl::new(tx, std::time::Duration::ZERO, PPS_INITIAL);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = false;

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
                target_buffer: std::time::Duration::from_millis(20),
                pps: PPS_INITIAL,
                is_armed: true,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        // WouldBlock: nothing was written yet, but the cache is retained.
        assert_eq!(writes.lock().unwrap().len(), 0);
        assert!(pipeline.cached_slice().is_some());
        assert_eq!(
            pipeline.cached_slice().unwrap().len(),
            expected_initial_chunk
        );

        // Drop pps so a fresh produce would yield a smaller chunk; queue Written next.
        control.set_pps(PPS_AFTER);
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
                target_buffer: std::time::Duration::from_millis(20),
                pps: PPS_AFTER,
                is_armed: true,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }

        // The cached buffer (sized for the original pps) is what gets written.
        let writes_v = writes.lock().unwrap();
        assert_eq!(writes_v.len(), 1);
        assert_eq!(writes_v[0].len(), expected_initial_chunk);
    }

    /// Regression for the clamped-regime pps change: when the chunk size stays
    /// pinned at `max_points_per_chunk` (IDN's 179 points at pps ≥ 17.9k), a pps
    /// change must still recompute `chunk_duration` — otherwise the send period
    /// is stale and the device is fed at the wrong rate.
    #[test]
    fn pps_change_in_clamped_band_changes_chunk_duration() {
        const MAX: usize = 179; // IDN clamp
        let caps = DacCapabilities {
            pps_min: 1_000,
            pps_max: 100_000,
            max_points_per_chunk: MAX,
            output_model: OutputModel::UdpTimed,
        };
        let backend = FakeFifo {
            caps,
            next_outcomes: Arc::new(Mutex::new(Vec::new())),
            writes: Arc::new(Mutex::new(Vec::new())),
            estimator: SoftwareDecayEstimator::new(),
        };
        let backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = UdpTimedAdapter::new(&backend);

        // Both pps values are in the clamped band (ceil(pps*0.01) ≥ 179).
        adapter.recompute_chunk(18_000);
        let d1 = adapter.chunk_duration;
        assert_eq!(adapter.chunk_points, MAX);

        adapter.recompute_chunk(30_000);
        let d2 = adapter.chunk_duration;
        assert_eq!(adapter.chunk_points, MAX, "still clamped");

        assert_ne!(
            d1, d2,
            "chunk_duration must change with pps even while chunk size is clamped"
        );
        // 179 / 30000 < 179 / 18000
        assert!(d2 < d1);
    }

    /// After a long stall, `next_send` must be allowed to lag `now` (bounded
    /// catch-up) rather than being hard-clamped to `now`, so lost time is made
    /// up over the next few sends instead of permanently draining the buffer.
    #[test]
    fn stall_allows_bounded_catchup() {
        let mut engine =
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_pending(frame_with_points(4_000));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

        let backend = FakeFifo {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 4_096,
                output_model: OutputModel::UdpTimed,
            },
            next_outcomes: Arc::new(Mutex::new(Vec::new())),
            writes: Arc::new(Mutex::new(Vec::new())),
            estimator: SoftwareDecayEstimator::new(),
        };
        let mut backend = BackendKind::Fifo(Box::new(backend));
        let mut adapter = UdpTimedAdapter::new(&backend);
        // chunk_duration is 10ms by default; catch-up window is 30ms.
        // Simulate a 1s stall: next_send far in the past.
        adapter.next_send = std::time::Instant::now() - std::time::Duration::from_secs(1);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = StreamControl::new(tx, std::time::Duration::ZERO, 30_000);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = false;
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
                target_buffer: std::time::Duration::from_millis(20),
                pps: 30_000,
                is_armed: true,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }

        // Hard-clamp (old behavior) would leave next_send ≈ now (gap ~0). Bounded
        // catch-up keeps it ~30ms (3 × 10ms chunks) behind, so the lag survives.
        let lag = std::time::Instant::now().saturating_duration_since(adapter.next_send);
        assert!(
            lag >= std::time::Duration::from_millis(20),
            "expected retained catch-up lag, got {lag:?}"
        );
        assert!(
            lag <= std::time::Duration::from_millis(45),
            "catch-up lag must stay bounded, got {lag:?}"
        );
    }

    /// Phase 4 lock-in: UdpTimed produces fixed 10ms slices sized
    /// `ceil(pps * 0.010)` and clamped to `max_points_per_chunk`. Asserted
    /// across both small-MTU (140-point) and large-MTU (IDN-shaped: 4096)
    /// caps so the unification is auditable for either device class.
    #[test]
    fn chunk_size_is_ten_millis_clamped_to_max() {
        for (pps, max, expected) in [
            (30_000_u32, 140_usize, 140_usize), // small-MTU shape: clamped
            (30_000, 4_096, 300),               // IDN shape: ceil(30k*0.010)
            (20_000, 4_096, 200),
            (60_000, 4_096, 600),
        ] {
            let mut engine =
                PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
            engine.set_pending(frame_with_points(8_000));
            let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

            let writes = Arc::new(Mutex::new(Vec::new()));
            let backend = FakeFifo {
                caps: DacCapabilities {
                    pps_min: 1_000,
                    pps_max: 100_000,
                    max_points_per_chunk: max,
                    output_model: OutputModel::UdpTimed,
                },
                next_outcomes: Arc::new(Mutex::new(Vec::new())),
                writes: Arc::clone(&writes),
                estimator: SoftwareDecayEstimator::new(),
            };
            let mut backend = BackendKind::Fifo(Box::new(backend));
            let mut adapter = UdpTimedAdapter::new(&backend);

            let (tx, rx) = mpsc::channel::<ControlMsg>();
            let control = StreamControl::new(tx, std::time::Duration::ZERO, pps);
            let metrics = FrameSessionMetrics::new(true);
            let mut shutter = false;

            // Two steps so the second produces a fresh chunk after the first
            // commits its retain.
            for _ in 0..2 {
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
                    pps,
                    is_armed: true,
                };
                assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
            }

            let writes_v = writes.lock().unwrap();
            assert!(!writes_v.is_empty(), "no writes for pps={pps} max={max}");
            for chunk in writes_v.iter() {
                assert_eq!(
                    chunk.len(),
                    expected,
                    "pps={pps} max={max}: expected {expected}-point slice"
                );
            }
        }
    }
}
