//! FrameSession and FrameSessionConfig — public frame-mode API.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::BackendKind;
use crate::device::{DacInfo, OutputModel};
use crate::error::{Error, Result};
use crate::reconnect::{reconnect_backend_with_retry, ReconnectPolicy};
use crate::stream::{ControlMsg, RunExit, StreamControl};

use super::content_source::{ContentSourceKind, FifoContentSource, FrameContentSource};
use super::engine::PresentationEngine;
use super::output_model::{self, LoopCtx, StepOutcome};
use super::slice_pipeline::SlicePipeline;
use super::{default_transition, Frame, OutputResetReason, TransitionFn};

/// Convert a duration to a point count at the given PPS rate.
fn duration_to_points(d: Duration, pps: u32) -> usize {
    if d.is_zero() {
        0
    } else {
        (d.as_secs_f64() * pps as f64).ceil() as usize
    }
}

// =============================================================================
// FrameSessionConfig
// =============================================================================

/// Configuration for a frame-mode streaming session.
pub struct FrameSessionConfig {
    /// Points per second output rate.
    pub pps: u32,
    /// Transition function for blanking between frames.
    pub transition_fn: TransitionFn,
    /// Duration of forced blanking after arming (default: 1ms).
    pub startup_blank: std::time::Duration,
    /// Number of points to shift RGB relative to XY (0 = disabled).
    ///
    /// Delays color channels relative to XY coordinates, compensating for
    /// the difference in galvo mirror and laser modulation response times.
    /// Applied at composition time. Set to 0 to disable.
    pub color_delay_points: usize,
    /// Reconnection configuration (default: disabled).
    ///
    /// Set via [`with_reconnect`](Self::with_reconnect) to enable automatic
    /// reconnection when the device disconnects.
    pub reconnect: Option<crate::config::ReconnectConfig>,
    /// Policy for what to output when the stream is idle (disarmed).
    ///
    /// Controls scanner behavior when disarmed. Default: [`Blank`](crate::config::IdlePolicy::Blank)
    /// (park at origin with laser off). Use [`Park`](crate::config::IdlePolicy::Park) to park at a
    /// specific position.
    pub idle_policy: crate::config::IdlePolicy,
    /// Optional hook for processing the final presented output.
    pub output_filter: Option<Box<dyn super::OutputFilter>>,
}

impl FrameSessionConfig {
    const DEFAULT_COLOR_DELAY: std::time::Duration = std::time::Duration::from_micros(150);

    /// Create a new config with the given PPS and default transition.
    pub fn new(pps: u32) -> Self {
        let color_delay_points =
            (Self::DEFAULT_COLOR_DELAY.as_secs_f64() * pps as f64).ceil() as usize;
        Self {
            pps,
            transition_fn: default_transition(pps),
            startup_blank: std::time::Duration::from_millis(1),
            color_delay_points,
            idle_policy: crate::config::IdlePolicy::default(),
            reconnect: None,
            output_filter: None,
        }
    }

    /// Set the transition function (builder pattern).
    pub fn with_transition_fn(mut self, f: TransitionFn) -> Self {
        self.transition_fn = f;
        self
    }

    /// Set the startup blank duration (builder pattern).
    pub fn with_startup_blank(mut self, duration: std::time::Duration) -> Self {
        self.startup_blank = duration;
        self
    }

    /// Set the color delay in points (builder pattern).
    pub fn with_color_delay_points(mut self, n: usize) -> Self {
        self.color_delay_points = n;
        self
    }

    /// Enable automatic reconnection (builder pattern).
    ///
    /// Requires the device to have been opened via [`open_device`](crate::open_device).
    pub fn with_reconnect(mut self, config: crate::config::ReconnectConfig) -> Self {
        self.reconnect = Some(config);
        self
    }

    /// Set the idle policy (builder pattern).
    ///
    /// Controls scanner behavior when disarmed. See [`crate::config::IdlePolicy`].
    pub fn with_idle_policy(mut self, policy: crate::config::IdlePolicy) -> Self {
        self.idle_policy = policy;
        self
    }

    /// Install a final-output filter (builder pattern).
    pub fn with_output_filter(mut self, filter: Box<dyn super::OutputFilter>) -> Self {
        self.output_filter = Some(filter);
        self
    }
}

// =============================================================================
// FrameSession
// =============================================================================

/// Read-only liveness and connectivity metrics for a [`FrameSession`].
#[derive(Clone)]
pub struct FrameSessionMetrics {
    inner: Arc<FrameSessionMetricsInner>,
}

struct FrameSessionMetricsInner {
    connected: AtomicBool,
    origin: Instant,
    last_loop_activity_nanos: AtomicU64,
    last_write_success_nanos: AtomicU64,
}

impl FrameSessionMetrics {
    pub(crate) fn new(connected: bool) -> Self {
        let metrics = Self {
            inner: Arc::new(FrameSessionMetricsInner {
                connected: AtomicBool::new(connected),
                origin: Instant::now(),
                last_loop_activity_nanos: AtomicU64::new(0),
                last_write_success_nanos: AtomicU64::new(0),
            }),
        };
        metrics.mark_loop_activity();
        metrics
    }

    /// Returns whether the session currently has a connected backend.
    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    /// Returns the last time the scheduler thread showed progress.
    pub fn last_loop_activity(&self) -> Option<Instant> {
        self.instant_from_nanos(self.inner.last_loop_activity_nanos.load(Ordering::SeqCst))
    }

    /// Returns the last time a backend write completed successfully.
    pub fn last_write_success(&self) -> Option<Instant> {
        self.instant_from_nanos(self.inner.last_write_success_nanos.load(Ordering::SeqCst))
    }

    fn instant_from_nanos(&self, nanos: u64) -> Option<Instant> {
        if nanos == 0 {
            None
        } else {
            self.inner.origin.checked_add(Duration::from_nanos(nanos))
        }
    }

    fn now_nanos(&self) -> u64 {
        (self.inner.origin.elapsed().as_nanos().min(u64::MAX as u128) as u64).max(1)
    }

    pub(super) fn mark_loop_activity(&self) {
        self.inner
            .last_loop_activity_nanos
            .store(self.now_nanos(), Ordering::SeqCst);
    }

    pub(super) fn mark_write_success(&self) {
        let now = self.now_nanos();
        self.inner
            .last_loop_activity_nanos
            .store(now, Ordering::SeqCst);
        self.inner
            .last_write_success_nanos
            .store(now, Ordering::SeqCst);
    }

    pub(super) fn set_connected(&self, connected: bool) {
        self.inner.connected.store(connected, Ordering::SeqCst);
        self.mark_loop_activity();
    }
}

struct MetricsDisconnectGuard(FrameSessionMetrics);

impl Drop for MetricsDisconnectGuard {
    fn drop(&mut self) {
        self.0.set_connected(false);
    }
}

/// A frame-mode streaming session.
///
/// Owns a scheduler thread that reads frames from a channel and writes them
/// to the DAC backend using the appropriate strategy (FIFO or frame-swap).
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device, FrameSessionConfig, Frame, LaserPoint};
///
/// let device = open_device("my-device")?;
/// let config = FrameSessionConfig::new(30_000);
/// let (session, _info) = device.start_frame_session(config)?;
///
/// session.control().arm()?;
/// session.send_frame(Frame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
/// ]));
/// ```
pub struct FrameSession {
    control: StreamControl,
    thread: Option<JoinHandle<Result<RunExit>>>,
    frame_slot: Arc<Mutex<Option<Frame>>>,
    metrics: FrameSessionMetrics,
}

impl FrameSession {
    /// Start a frame session on the given backend.
    pub(crate) fn start(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<Self> {
        if !backend.is_connected() {
            backend.connect()?;
        }

        let (control_tx, control_rx) = mpsc::channel();
        let initial_color_delay = if config.color_delay_points > 0 {
            Duration::from_secs_f64(config.color_delay_points as f64 / config.pps as f64)
        } else {
            Duration::ZERO
        };
        let control = StreamControl::new(control_tx, initial_color_delay, config.pps);
        let frame_slot: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
        let metrics = FrameSessionMetrics::new(backend.is_connected());

        let control_clone = control.clone();
        let slot_clone = frame_slot.clone();
        let metrics_clone = metrics.clone();

        let thread = std::thread::spawn(move || {
            let _disconnect_guard = MetricsDisconnectGuard(metrics_clone.clone());
            Self::run_loop(
                backend,
                config,
                control_clone,
                control_rx,
                slot_clone,
                metrics_clone,
                reconnect_policy,
            )
        });

        Ok(Self {
            control,
            thread: Some(thread),
            frame_slot,
            metrics,
        })
    }

    /// Returns a control handle for arm/disarm/stop.
    pub fn control(&self) -> StreamControl {
        self.control.clone()
    }

    /// Submit a frame for display. Latest-wins: overwrites any unconsumed
    /// pending frame immediately, with no buffering or memory growth.
    pub fn send_frame(&self, frame: Frame) {
        *self.frame_slot.lock().unwrap() = Some(frame);
    }

    /// Returns true if the scheduler thread has finished.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Returns a metrics handle for observing scheduler liveness.
    pub fn metrics(&self) -> FrameSessionMetrics {
        self.metrics.clone()
    }

    /// Wait for the session thread to finish and return the exit reason.
    pub fn join(mut self) -> Result<RunExit> {
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .unwrap_or(Err(Error::disconnected("thread panicked")))
        } else {
            Ok(RunExit::Stopped)
        }
    }

    // =========================================================================
    // Driver loop
    // =========================================================================

    fn run_loop(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<ControlMsg>,
        frame_slot: Arc<Mutex<Option<Frame>>>,
        metrics: FrameSessionMetrics,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<RunExit> {
        let FrameSessionConfig {
            pps: _,
            transition_fn,
            startup_blank,
            color_delay_points,
            idle_policy,
            output_filter,
            reconnect: _,
        } = config;

        let mut engine = PresentationEngine::new(transition_fn);
        if backend.is_frame_swap() {
            engine.set_frame_capacity(backend.frame_capacity());
        }

        // Per-OutputModel initial buffer sizing: FIFO bounds by max_points_per_chunk;
        // frame-swap bounds by frame_capacity (max_points_per_chunk is meaningless there).
        let initial_buf_capacity = match backend.caps().output_model {
            OutputModel::UsbFrameSwap => backend.frame_capacity().unwrap_or(0),
            OutputModel::NetworkFifo | OutputModel::UdpTimed => backend.caps().max_points_per_chunk,
        };
        let mut pipeline = SlicePipeline::new(
            engine,
            color_delay_points,
            output_filter,
            idle_policy,
            initial_buf_capacity,
        );
        pipeline.reset_output_filter(OutputResetReason::SessionStart);

        let mut adapter = output_model::for_backend(&backend);

        let mut shutter_open = false;
        let mut last_armed = false;
        let expected_frame_swap = backend.is_frame_swap();

        loop {
            metrics.mark_loop_activity();
            if control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            let pps = control.pps();
            let startup_blank_points = duration_to_points(startup_blank, pps);
            pipeline.resize_color_delay(duration_to_points(control.color_delay(), pps));

            if let Some(frame) = frame_slot.lock().unwrap().take() {
                pipeline.set_pending(frame);
            }

            if !backend.is_connected() {
                match Self::try_reconnect(
                    &mut backend,
                    reconnect_policy.as_ref(),
                    &control,
                    &mut shutter_open,
                    &mut last_armed,
                    &mut pipeline,
                    expected_frame_swap,
                    &metrics,
                ) {
                    Ok(info) => {
                        adapter.on_reconnect(&info, &mut pipeline, &mut backend);
                        continue;
                    }
                    Err(exit) => return Ok(exit),
                }
            }

            if output_model::process_control_messages(&control_rx, &mut shutter_open, &mut backend)
            {
                return Ok(RunExit::Stopped);
            }
            let is_armed = control.is_armed();
            if let Some(reason) = Self::handle_shutter_transition(
                is_armed,
                &mut last_armed,
                &mut shutter_open,
                &mut pipeline,
                startup_blank_points,
                &mut backend,
            ) {
                pipeline.reset_output_filter(reason);
            }

            let outcome = {
                let source = if expected_frame_swap {
                    ContentSourceKind::Frame(&mut pipeline as &mut dyn FrameContentSource)
                } else {
                    ContentSourceKind::Fifo(&mut pipeline as &mut dyn FifoContentSource)
                };
                let mut ctx = LoopCtx {
                    backend: &mut backend,
                    source,
                    control: &control,
                    control_rx: &control_rx,
                    metrics: &metrics,
                    shutter_open: &mut shutter_open,
                    pps,
                    is_armed,
                };
                adapter.step(&mut ctx)
            };

            match outcome {
                StepOutcome::Continue => {}
                StepOutcome::Stopped => return Ok(RunExit::Stopped),
                StepOutcome::Disconnected => match Self::try_reconnect(
                    &mut backend,
                    reconnect_policy.as_ref(),
                    &control,
                    &mut shutter_open,
                    &mut last_armed,
                    &mut pipeline,
                    expected_frame_swap,
                    &metrics,
                ) {
                    Ok(info) => {
                        adapter.on_reconnect(&info, &mut pipeline, &mut backend);
                        continue;
                    }
                    Err(exit) => return Ok(exit),
                },
            }
        }
    }

    // =========================================================================
    // Shared helpers
    // =========================================================================

    /// Attempt to reconnect the backend using the reconnection policy.
    ///
    /// On success, replaces `backend` and asks the source (the pipeline) to
    /// replay its derived state (engine, color delay, cached pending frame,
    /// output-filter `Reconnect` reset). Returns the new `DacInfo`. If no
    /// policy is set, returns `Err(RunExit::Disconnected)`.
    #[allow(clippy::too_many_arguments)]
    fn try_reconnect(
        backend: &mut BackendKind,
        policy: Option<&ReconnectPolicy>,
        control: &StreamControl,
        shutter_open: &mut bool,
        last_armed: &mut bool,
        pipeline: &mut SlicePipeline,
        expected_frame_swap: bool,
        metrics: &FrameSessionMetrics,
    ) -> std::result::Result<DacInfo, RunExit> {
        let Some(policy) = policy else {
            return Err(RunExit::Disconnected);
        };
        metrics.set_connected(false);
        let current_pps = control.pps();
        let (info, new_backend) = reconnect_backend_with_retry(
            policy,
            || control.is_stop_requested(),
            |info, new_backend| {
                if current_pps < info.caps.pps_min || current_pps > info.caps.pps_max {
                    log::error!(
                        "'{}' PPS {} outside new device range [{}, {}]",
                        policy.target.device_id,
                        current_pps,
                        info.caps.pps_min,
                        info.caps.pps_max
                    );
                    return Err(RunExit::Disconnected);
                }

                if new_backend.is_frame_swap() != expected_frame_swap {
                    log::error!(
                        "'{}' reconnected device has incompatible backend type",
                        policy.target.device_id
                    );
                    return Err(RunExit::Disconnected);
                }

                Ok(())
            },
            || metrics.mark_loop_activity(),
        )?;

        *backend = new_backend;
        *shutter_open = false;
        *last_armed = false;
        metrics.set_connected(true);

        // Source-level reconnect: resets engine + color delay, replays the most
        // recent frame, fires `OutputResetReason::Reconnect` on the output
        // filter. Adapter-side reconnect (capacity sizing, etc.) is invoked
        // separately by the driver.
        if expected_frame_swap {
            <SlicePipeline as FrameContentSource>::on_reconnect(pipeline, &info);
        } else {
            <SlicePipeline as FifoContentSource>::on_reconnect(pipeline, &info);
        }

        if let Some(cb) = policy.on_reconnect.lock().unwrap().as_mut() {
            cb(&info);
        }

        Ok(info)
    }

    /// Handle arm/disarm shutter transitions.
    fn handle_shutter_transition(
        is_armed: bool,
        last_armed: &mut bool,
        shutter_open: &mut bool,
        pipeline: &mut SlicePipeline,
        startup_blank_points: usize,
        backend: &mut BackendKind,
    ) -> Option<OutputResetReason> {
        if !*last_armed && is_armed {
            pipeline.arm_startup_blank(startup_blank_points);
            if !*shutter_open {
                let _ = backend.set_shutter(true);
                *shutter_open = true;
            }
            *last_armed = is_armed;
            return Some(OutputResetReason::Arm);
        } else if *last_armed && !is_armed {
            if *shutter_open {
                let _ = backend.set_shutter(false);
                *shutter_open = false;
            }
            *last_armed = is_armed;
            return Some(OutputResetReason::Disarm);
        }
        *last_armed = is_armed;
        None
    }
}

impl Drop for FrameSession {
    fn drop(&mut self) {
        let _ = self.control.stop();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}
