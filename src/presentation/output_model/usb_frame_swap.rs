//! UsbFrameSwap adapter — atomic frame upload to double-buffered DACs (Helios).

use std::time::Duration;

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::content_source::ContentSourceKind;
use super::{LoopCtx, OutputModelAdapter, StepOutcome};

/// Number of consecutive transient (non-disconnect) write errors tolerated by
/// retrying the cached frame before escalating to a full disconnect. A single
/// bulk-write hiccup (e.g. one rusb `Timeout`) should not tear down the whole
/// connection for ≥150 ms.
const MAX_TRANSIENT_WRITE_RETRIES: u32 = 3;

pub(crate) struct UsbFrameSwapAdapter {
    write_pending: bool,
    /// Consecutive transient write errors on the current cached frame.
    transient_retries: u32,
}

impl UsbFrameSwapAdapter {
    pub fn new(_backend: &BackendKind) -> Self {
        Self {
            write_pending: false,
            transient_retries: 0,
        }
    }
}

impl OutputModelAdapter for UsbFrameSwapAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        if !ctx.backend.is_ready_for_frame() {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        let ContentSourceKind::Frame(source) = &mut ctx.source else {
            unreachable!("UsbFrameSwapAdapter requires a Frame content source");
        };

        if !self.write_pending {
            if source.produce_frame(ctx.pps, ctx.is_armed).is_empty() {
                ctx.sleep_and_mark_activity(Duration::from_millis(1));
                return StepOutcome::Continue;
            }
            self.write_pending = true;
        }

        let (n, outcome) = match source.cached_slice() {
            Some(slice) => (
                slice.len(),
                ctx.backend.try_write_frame_ready(ctx.pps, slice),
            ),
            None => {
                self.write_pending = false;
                return StepOutcome::Continue;
            }
        };

        // Expected play time of the frame just handled; used to back off status
        // polling after a successful write (the device won't be ready to swap
        // until near the end of the frame).
        let mut post_write_frame_secs: Option<f64> = None;

        match outcome {
            Ok(WriteOutcome::Written) => {
                ctx.metrics.mark_write_success();
                source.commit_written(n, ctx.is_armed);
                self.write_pending = false;
                self.transient_retries = 0;
                if ctx.pps > 0 && n > 0 {
                    post_write_frame_secs = Some(n as f64 / ctx.pps as f64);
                }
            }
            Ok(WriteOutcome::WouldBlock) => {
                ctx.metrics.mark_loop_activity();
                ctx.sleep_and_mark_activity(Duration::from_millis(1));
            }
            Err(e) if e.is_stopped() => {
                return StepOutcome::Stopped;
            }
            Err(e) if e.is_disconnected() => {
                (ctx.error_sink)(e);
                return StepOutcome::Disconnected;
            }
            Err(e) => {
                // Transient (non-disconnect) write error: retry the cached frame
                // a few times before tearing down the connection. `write_pending`
                // stays true so the same cached slice is re-sent next step.
                self.transient_retries += 1;
                if self.transient_retries <= MAX_TRANSIENT_WRITE_RETRIES {
                    log::debug!(
                        "frame write error (transient, retry {}/{MAX_TRANSIENT_WRITE_RETRIES}): {e}",
                        self.transient_retries
                    );
                    ctx.metrics.mark_loop_activity();
                    ctx.sleep_and_mark_activity(Duration::from_millis(1));
                } else {
                    log::warn!(
                        "frame write error, disconnecting backend after {MAX_TRANSIENT_WRITE_RETRIES} transient retries: {e}"
                    );
                    self.transient_retries = 0;
                    self.write_pending = false;
                    let _ = ctx.backend.disconnect();
                    (ctx.error_sink)(e);
                    return StepOutcome::Disconnected;
                }
            }
        }

        // Adaptive status polling: after a successful write, sleep ~half the
        // frame's play time before resuming ~1 ms readiness polling near the
        // expected swap point. Stays stop/control responsive via the
        // control-checking sleep.
        if let Some(secs) = post_write_frame_secs {
            let half = Duration::from_secs_f64(secs / 2.0);
            if let Err(outcome) = ctx.sleep_with_control_check(half) {
                return outcome;
            }
        }

        StepOutcome::Continue
    }

    fn on_reconnect(&mut self, _info: &DacInfo, _backend: &mut BackendKind) {
        self.write_pending = false;
        self.transient_retries = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use crate::backend::{BackendKind, DacBackend, FrameSwapBackend, WriteOutcome};
    use crate::config::IdlePolicy;
    use crate::device::{DacCapabilities, DacType, OutputModel};
    use crate::error::{Error, Result as DacResult};
    use crate::point::LaserPoint;
    use crate::presentation::content_source::{ContentSourceKind, FrameContentSource};
    use crate::presentation::engine::PresentationEngine;
    use crate::presentation::output_model::{LoopCtx, OutputModelAdapter, StepOutcome};
    use crate::presentation::session::FrameSessionMetrics;
    use crate::presentation::slice_pipeline::SlicePipeline;
    use crate::presentation::{Frame, TransitionPlan};
    use crate::stream::{ControlMsg, StreamControl};

    use super::{UsbFrameSwapAdapter, MAX_TRANSIENT_WRITE_RETRIES};

    struct FakeFrameSwap {
        caps: DacCapabilities,
        ready: Arc<AtomicBool>,
        ready_checks: Arc<AtomicUsize>,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
        outcomes: Arc<Mutex<Vec<DacResult<WriteOutcome>>>>,
        disconnects: Arc<AtomicUsize>,
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
            self.disconnects.fetch_add(1, Ordering::SeqCst);
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
            self.ready_checks.fetch_add(1, Ordering::SeqCst);
            self.ready.load(Ordering::SeqCst)
        }
        fn write_frame(&mut self, _pps: u32, points: &[LaserPoint]) -> DacResult<WriteOutcome> {
            self.write_frame_ready(_pps, points)
        }
        fn write_frame_ready(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<WriteOutcome> {
            self.writes.lock().unwrap().push(points.to_vec());
            self.outcomes.lock().unwrap().remove(0)
        }
    }

    #[test]
    fn ready_check_transition_false_to_true_produces_a_frame() {
        let ready = Arc::new(AtomicBool::new(false));
        let ready_checks = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFrameSwap {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 4_095,
                output_model: OutputModel::UsbFrameSwap,
            },
            ready: Arc::clone(&ready),
            ready_checks: Arc::clone(&ready_checks),
            writes: Arc::clone(&writes),
            outcomes: Arc::new(Mutex::new(vec![Ok(WriteOutcome::Written)])),
            disconnects: Arc::new(AtomicUsize::new(0)),
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
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
        assert_eq!(writes.lock().unwrap().len(), 0);
        assert!(pipeline.cached_slice().is_none());

        // Flip ready true. Step 2: produces and writes.
        ready.store(true, Ordering::SeqCst);
        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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

        let writes_v = writes.lock().unwrap();
        assert_eq!(writes_v.len(), 1);
        assert!(!writes_v[0].is_empty());
        assert_eq!(ready_checks.load(Ordering::SeqCst), 2);
        // Cache cleared after successful Written.
        assert!(pipeline.cached_slice().is_none());
    }

    #[test]
    fn pending_write_rechecks_readiness_before_retry() {
        let ready = Arc::new(AtomicBool::new(true));
        let ready_checks = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeFrameSwap {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 4_095,
                output_model: OutputModel::UsbFrameSwap,
            },
            ready: Arc::clone(&ready),
            ready_checks: Arc::clone(&ready_checks),
            writes: Arc::clone(&writes),
            outcomes: Arc::new(Mutex::new(vec![
                Ok(WriteOutcome::WouldBlock),
                Ok(WriteOutcome::Written),
            ])),
            disconnects: Arc::new(AtomicUsize::new(0)),
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

        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
        assert_eq!(writes.lock().unwrap().len(), 1);

        ready.store(false, Ordering::SeqCst);
        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
        assert_eq!(writes.lock().unwrap().len(), 1);

        ready.store(true, Ordering::SeqCst);
        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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

        assert_eq!(writes.lock().unwrap().len(), 2);
        assert!(pipeline.cached_slice().is_none());
        assert_eq!(ready_checks.load(Ordering::SeqCst), 3);
    }

    /// Build a `FakeFrameSwap` (ready = true) with the given queued outcomes.
    fn fake_ready_backend(
        outcomes: Vec<DacResult<WriteOutcome>>,
        writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
        disconnects: Arc<AtomicUsize>,
    ) -> BackendKind {
        BackendKind::FrameSwap(Box::new(FakeFrameSwap {
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 4_095,
                output_model: OutputModel::UsbFrameSwap,
            },
            ready: Arc::new(AtomicBool::new(true)),
            ready_checks: Arc::new(AtomicUsize::new(0)),
            writes,
            outcomes: Arc::new(Mutex::new(outcomes)),
            disconnects,
        }))
    }

    #[test]
    fn transient_write_error_retries_then_succeeds_without_disconnect() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let disconnects = Arc::new(AtomicUsize::new(0));
        // One transient (non-disconnect) error, then success.
        let mut backend = fake_ready_backend(
            vec![
                Err(Error::backend(std::io::Error::other("transient timeout"))),
                Ok(WriteOutcome::Written),
            ],
            Arc::clone(&writes),
            Arc::clone(&disconnects),
        );

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

        // Step 1: transient error — retried, cached frame kept, no disconnect.
        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
        assert_eq!(writes.lock().unwrap().len(), 1);
        assert_eq!(disconnects.load(Ordering::SeqCst), 0);
        assert!(
            pipeline.cached_slice().is_some(),
            "cached frame kept for retry"
        );

        // Step 2: retry succeeds.
        {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
        assert_eq!(writes.lock().unwrap().len(), 2);
        assert_eq!(disconnects.load(Ordering::SeqCst), 0);
        assert!(pipeline.cached_slice().is_none());
    }

    #[test]
    fn persistent_transient_write_error_escalates_to_disconnect() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let disconnects = Arc::new(AtomicUsize::new(0));
        // More than MAX_TRANSIENT_WRITE_RETRIES failures in a row.
        let outcomes: Vec<DacResult<WriteOutcome>> = (0..MAX_TRANSIENT_WRITE_RETRIES + 1)
            .map(|_| Err(Error::backend(std::io::Error::other("transient timeout"))))
            .collect();
        let mut backend =
            fake_ready_backend(outcomes, Arc::clone(&writes), Arc::clone(&disconnects));

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

        let mut last = StepOutcome::Continue;
        for _ in 0..(MAX_TRANSIENT_WRITE_RETRIES + 1) {
            let source = ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource);
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
            last = adapter.step(&mut ctx);
            if matches!(last, StepOutcome::Disconnected) {
                break;
            }
        }

        assert!(matches!(last, StepOutcome::Disconnected));
        assert_eq!(disconnects.load(Ordering::SeqCst), 1);
        // Retried MAX times, then one more failing write escalated.
        assert_eq!(
            writes.lock().unwrap().len() as u32,
            MAX_TRANSIENT_WRITE_RETRIES + 1
        );
    }
}
