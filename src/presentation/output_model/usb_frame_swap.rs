//! UsbFrameSwap adapter — atomic frame upload to double-buffered DACs (Helios).

use std::time::Duration;

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::slice_pipeline::SlicePipeline;
use super::{LoopCtx, OutputModelAdapter, StepOutcome};

pub(crate) struct UsbFrameSwapAdapter {
    write_pending: bool,
}

impl UsbFrameSwapAdapter {
    pub fn new(_backend: &BackendKind) -> Self {
        Self {
            write_pending: false,
        }
    }
}

impl OutputModelAdapter for UsbFrameSwapAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        if !self.write_pending && !ctx.backend.is_ready_for_frame() {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        if !self.write_pending {
            let n = ctx.pipeline.produce_frame_swap(ctx.pps, ctx.is_armed).len();
            if n == 0 {
                ctx.sleep_and_mark_activity(Duration::from_millis(1));
                return StepOutcome::Continue;
            }
            self.write_pending = true;
        }

        let outcome = {
            let slice = match ctx.pipeline.cached_slice() {
                Some(s) => s,
                None => {
                    self.write_pending = false;
                    return StepOutcome::Continue;
                }
            };
            ctx.backend.try_write(ctx.pps, slice)
        };

        match outcome {
            Ok(WriteOutcome::Written) => {
                ctx.metrics.mark_write_success();
                ctx.pipeline.invalidate();
                self.write_pending = false;
            }
            Ok(WriteOutcome::WouldBlock) => {
                ctx.metrics.mark_loop_activity();
                ctx.sleep_and_mark_activity(Duration::from_millis(1));
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
        _info: &DacInfo,
        pipeline: &mut SlicePipeline,
        backend: &mut BackendKind,
    ) {
        self.write_pending = false;
        pipeline.set_frame_capacity(backend.frame_capacity());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use crate::backend::{BackendKind, DacBackend, FrameSwapBackend, WriteOutcome};
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

    use super::UsbFrameSwapAdapter;

    struct FakeFrameSwap {
        caps: DacCapabilities,
        ready: Arc<AtomicBool>,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
    }

    impl DacBackend for FakeFrameSwap {
        fn dac_type(&self) -> DacType {
            DacType::Custom("FakeFrameSwap".into())
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

    impl FrameSwapBackend for FakeFrameSwap {
        fn frame_capacity(&self) -> usize {
            self.caps.max_points_per_chunk
        }
        fn is_ready_for_frame(&mut self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
        fn write_frame(&mut self, _pps: u32, points: &[LaserPoint]) -> DacResult<WriteOutcome> {
            self.writes.lock().unwrap().push(points.to_vec());
            Ok(WriteOutcome::Written)
        }
    }

    #[test]
    fn ready_check_transition_false_to_true_produces_a_frame() {
        let ready = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFrameSwap {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 4_095,
                output_model: OutputModel::UsbFrameSwap,
            },
            ready: Arc::clone(&ready),
            writes: Arc::clone(&writes),
        };
        let mut backend = BackendKind::FrameSwap(Box::new(backend));

        let mut engine =
            PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())));
        engine.set_frame_capacity(Some(4_095));
        let pts: Vec<LaserPoint> = (0..16)
            .map(|i| LaserPoint::new(i as f32 * 0.01, 0.0, 1000, 1000, 1000, 1000))
            .collect();
        engine.set_pending(Frame::new(pts));
        let mut pipeline = SlicePipeline::new(engine, 0, None, IdlePolicy::Blank, 0);

        let mut adapter = UsbFrameSwapAdapter::new(&backend);

        let (tx, rx) = mpsc::channel::<ControlMsg>();
        let control = StreamControl::new(tx, std::time::Duration::ZERO, 30_000);
        let metrics = FrameSessionMetrics::new(true);
        let mut shutter = false;

        // Step 1: not ready → 1ms sleep, no produce/write.
        {
            let mut ctx = LoopCtx {
                backend: &mut backend,
                pipeline: &mut pipeline,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                pps: 30_000,
                is_armed: true,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }
        assert_eq!(writes.lock().unwrap().len(), 0);
        assert!(pipeline.cached_slice().is_none());

        // Flip ready true. Step 2: produces and writes.
        ready.store(true, Ordering::SeqCst);
        {
            let mut ctx = LoopCtx {
                backend: &mut backend,
                pipeline: &mut pipeline,
                control: &control,
                control_rx: &rx,
                metrics: &metrics,
                shutter_open: &mut shutter,
                pps: 30_000,
                is_armed: true,
            };
            assert!(matches!(adapter.step(&mut ctx), StepOutcome::Continue));
        }

        let writes_v = writes.lock().unwrap();
        assert_eq!(writes_v.len(), 1);
        assert!(!writes_v[0].is_empty());
        // Cache cleared after successful Written.
        assert!(pipeline.cached_slice().is_none());
    }
}
