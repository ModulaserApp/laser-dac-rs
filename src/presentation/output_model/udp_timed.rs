//! UdpTimed adapter — fixed-rate metronomic pacing for UDP-based DACs (e.g. IDN).

use std::time::{Duration, Instant};

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::slice_pipeline::SlicePipeline;
use super::{LoopCtx, OutputModelAdapter, StepOutcome};

const CHUNK_SECS: f64 = 0.010;

pub(crate) struct UdpTimedAdapter {
    next_send: Instant,
    chunk_points: usize,
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
            chunk_duration: Duration::from_secs_f64(CHUNK_SECS),
            has_retain: false,
            max_points_per_chunk: max,
        }
    }

    fn recompute_chunk(&mut self, pps: u32) {
        let pps_f64 = pps as f64;
        let new_chunk = ((pps_f64 * CHUNK_SECS).ceil() as usize).min(self.max_points_per_chunk);
        if new_chunk != self.chunk_points {
            self.chunk_points = new_chunk;
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
        if self.next_send < now {
            self.next_send = now;
        }

        // pps may have changed during the sleep — re-read from control.
        let pps = ctx.control.pps();
        self.recompute_chunk(pps);
        ctx.pipeline.reserve_buf(self.chunk_points);

        if !self.has_retain {
            let n = ctx
                .pipeline
                .produce_fifo_chunk(self.chunk_points, pps, ctx.is_armed)
                .len();
            if n == 0 {
                return StepOutcome::Continue;
            }
            self.has_retain = true;
        }

        let outcome = {
            let slice = match ctx.pipeline.cached_slice() {
                Some(s) => s,
                None => {
                    self.has_retain = false;
                    return StepOutcome::Continue;
                }
            };
            ctx.backend.try_write(pps, slice)
        };

        match outcome {
            Ok(WriteOutcome::Written) => {
                ctx.metrics.mark_write_success();
                ctx.pipeline.invalidate();
                self.has_retain = false;
            }
            Ok(WriteOutcome::WouldBlock) => {
                ctx.metrics.mark_loop_activity();
                // leave cache; retry on next deadline
            }
            Err(e) if e.is_disconnected() => {
                return StepOutcome::Disconnected;
            }
            Err(_) => {}
        }
        StepOutcome::Continue
    }

    fn on_reconnect(
        &mut self,
        info: &DacInfo,
        _pipeline: &mut SlicePipeline,
        _backend: &mut BackendKind,
    ) {
        self.next_send = Instant::now();
        self.has_retain = false;
        self.max_points_per_chunk = info.caps.max_points_per_chunk;
        // Force a recompute on next step.
        self.chunk_points = 0;
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

        fn queued_points(&self) -> Option<u64> {
            Some(0)
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
            let mut ctx = LoopCtx {
                backend: &mut backend,
                pipeline: &mut pipeline,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
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
            let mut ctx = LoopCtx {
                backend: &mut backend,
                pipeline: &mut pipeline,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
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
}
