//! Unified driver loop shared by [`super::session::FrameSession`] and
//! [`crate::stream::Stream`].
//!
//! Concentrates the cross-mode invariants in one place: control-message
//! drain, shutter transitions, reconnect with retry, end-of-stream drain,
//! and step dispatch. The two callers differ only in the [`ContentSource`]
//! they hand in and a small set of mode-specific knobs (reconnect validator,
//! pre-step hook, error sink).
//!
//! [`ContentSource`]: super::content_source::FifoContentSource

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::BackendKind;
use crate::device::DacInfo;
use crate::error::{Error, Result};
use crate::reconnect::{reconnect_backend_with_retry, ReconnectPolicy};
use crate::session::{ControlMsg, SessionControl, SessionExit};

use super::content_source::{ContentSourceKind, FifoContentSource, FrameContentSource};
use super::output_model::{
    self, process_control_messages, Clock, ControlAction, LoopCtx, OutputModelAdapter,
    ShutterState, StepOutcome, SystemClock,
};
use super::session::FrameSessionMetrics;
use super::OutputResetReason;

/// Either kind of content source, owned by the driver for the duration of `run`.
pub(crate) enum SourceOwned {
    Fifo(Box<dyn FifoContentSource>),
    Frame(Box<dyn FrameContentSource>),
}

impl SourceOwned {
    fn is_frame(&self) -> bool {
        matches!(self, SourceOwned::Frame(_))
    }

    fn as_kind(&mut self) -> ContentSourceKind<'_> {
        match self {
            SourceOwned::Fifo(s) => ContentSourceKind::Fifo(s.as_mut()),
            SourceOwned::Frame(s) => ContentSourceKind::Frame(s.as_mut()),
        }
    }

    fn on_reconnect(&mut self, info: &DacInfo) {
        match self {
            SourceOwned::Fifo(s) => s.on_reconnect(info),
            SourceOwned::Frame(s) => s.on_reconnect(info),
        }
    }

    fn set_frame_capacity_if_supported(&mut self, cap: Option<usize>) {
        if let SourceOwned::Frame(s) = self {
            s.set_frame_capacity(cap);
        }
    }

    fn is_ended(&self) -> bool {
        match self {
            SourceOwned::Fifo(s) => s.is_ended(),
            SourceOwned::Frame(_) => false,
        }
    }

    fn submit_frame(&mut self, frame: super::Frame) {
        match self {
            SourceOwned::Fifo(s) => s.submit_frame(frame),
            SourceOwned::Frame(s) => s.submit_frame(frame),
        }
    }

    fn arm_startup_blank(&mut self, pps: u32) {
        match self {
            SourceOwned::Fifo(s) => s.arm_startup_blank(pps),
            SourceOwned::Frame(s) => s.arm_startup_blank(pps),
        }
    }

    fn discard_cached(&mut self) {
        match self {
            SourceOwned::Fifo(s) => s.discard_cached(),
            SourceOwned::Frame(s) => s.discard_cached(),
        }
    }

    fn on_disarm(&mut self) {
        match self {
            SourceOwned::Fifo(s) => s.on_disarm(),
            SourceOwned::Frame(s) => s.on_disarm(),
        }
    }

    fn reset_output_filter(&mut self, reason: OutputResetReason) {
        match self {
            SourceOwned::Fifo(s) => s.reset_output_filter(reason),
            SourceOwned::Frame(s) => s.reset_output_filter(reason),
        }
    }

    fn set_color_delay(&mut self, delay: Duration) {
        match self {
            SourceOwned::Fifo(s) => s.set_color_delay(delay),
            SourceOwned::Frame(s) => s.set_color_delay(delay),
        }
    }

    fn take_stop_error(&mut self) -> Option<Error> {
        match self {
            SourceOwned::Fifo(s) => s.take_stop_error(),
            SourceOwned::Frame(_) => None,
        }
    }
}

/// Latest-wins frame slot the driver polls each iteration.
pub(crate) type PendingFrame = Arc<Mutex<Option<super::Frame>>>;

/// Result of the reconnect-validator closure: `Ok(())` accepts the swap,
/// `Err(SessionExit::...)` rejects with the given exit reason.
pub(crate) type ReconnectValidator =
    Box<dyn Fn(&DacInfo, &BackendKind, u32) -> std::result::Result<(), SessionExit> + Send>;

/// Sink for non-fatal write errors. Stream-mode threads the user's
/// `on_error`; frame-mode passes a no-op.
pub(crate) type ErrorSink = Box<dyn FnMut(Error) + Send>;

pub(crate) struct DriverInputs {
    pub backend: BackendKind,
    pub source: SourceOwned,
    pub control: SessionControl,
    pub control_rx: Receiver<ControlMsg>,
    pub metrics: FrameSessionMetrics,
    pub reconnect_policy: Option<ReconnectPolicy>,
    pub validator: ReconnectValidator,
    pub error_sink: ErrorSink,
    pub target_buffer: Duration,
    pub drain_timeout: Duration,
    /// Latest-wins frame slot. Frame-mode passes the shared `Arc` so
    /// `FrameSession::send_frame` can write into it; stream-mode passes
    /// `None` (no frame intake).
    pub pending_frame: Option<PendingFrame>,
    /// Time source for the loop's pacing sleeps. Production passes
    /// [`SystemClock`]; tests can inject a virtual clock for deterministic
    /// pacing. Defaulted via [`DriverInputs::system_clock`].
    pub clock: Box<dyn Clock>,
}

impl DriverInputs {
    /// The production clock (real wall-clock). Constructors that don't inject a
    /// custom clock use this.
    pub(crate) fn system_clock() -> Box<dyn Clock> {
        Box::new(SystemClock)
    }
}

/// The unified driver loop. Lifted from the old `FrameSession::run_loop`,
/// generalised to either content source.
pub(crate) fn run(mut inputs: DriverInputs) -> Result<SessionExit> {
    let result = run_loop(&mut inputs);
    end_session(&mut inputs.backend);
    result
}

/// Stop the device and drop the connection when the session ends.
///
/// The driver owns the backend and drops it on return, so nothing else will
/// ever stop it. Every exit path has already closed the shutter (or drained
/// and blanked); without an explicit stop, protocols such as Ether Dream are
/// left holding stale points and a playback state that only a later stop
/// resets. This matches `Stream::stop()`: stop output, then disconnect.
fn end_session(backend: &mut BackendKind) {
    if !backend.is_connected() {
        return;
    }
    if let Err(e) = backend.stop() {
        log::debug!("backend stop at session end failed: {e}");
    }
    if let Err(e) = backend.disconnect() {
        log::debug!("backend disconnect at session end failed: {e}");
    }
}

fn run_loop(inputs: &mut DriverInputs) -> Result<SessionExit> {
    let expected_frame_swap = inputs.source.is_frame();
    let mut adapter = match output_model::for_backend(&inputs.backend, expected_frame_swap) {
        Ok(adapter) => adapter,
        Err(err) => {
            inputs.backend.close_and_disconnect();
            return Err(err);
        }
    };
    let mut state = LoopState::default();

    loop {
        inputs.metrics.mark_loop_activity();
        if inputs.control.is_stop_requested() {
            return Ok(stop_and_close_shutter(
                &mut inputs.backend,
                &mut state.shutter_state,
            ));
        }

        let pps = inputs.control.pps();
        inputs.source.set_color_delay(inputs.control.color_delay());
        if let Some(slot) = inputs.pending_frame.as_ref() {
            // Poisoning policy: a frame slot is latest-wins state; recover via
            // `into_inner` instead of propagating a scheduler panic to callers.
            if let Some(frame) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                inputs.source.submit_frame(frame);
            }
        }

        if !inputs.backend.is_connected() {
            match reconnect(inputs, &mut *adapter, &mut state) {
                Ok(()) => continue,
                Err(exit) => return Ok(exit),
            }
        }

        if matches!(
            process_control_messages(&inputs.control_rx),
            ControlAction::Stop
        ) {
            return Ok(stop_and_close_shutter(
                &mut inputs.backend,
                &mut state.shutter_state,
            ));
        }
        if matches!(
            handle_shutter_transition(
                &inputs.control,
                &mut state.handled_state,
                &mut state.shutter_state,
                &mut inputs.backend,
                &mut inputs.source,
                &mut *adapter,
                pps,
            ),
            TransitionOutcome::Disconnected
        ) {
            match reconnect(inputs, &mut *adapter, &mut state) {
                Ok(()) => continue,
                Err(exit) => return Ok(exit),
            }
        }
        let is_armed = inputs.control.is_armed();

        let outcome = {
            let source = inputs.source.as_kind();
            let mut ctx = LoopCtx {
                backend: &mut inputs.backend,
                source,
                control: &inputs.control,
                control_rx: &inputs.control_rx,
                metrics: &inputs.metrics,
                shutter_open: &mut state.shutter_state,
                error_sink: &mut inputs.error_sink,
                target_buffer: inputs.target_buffer,
                pps,
                is_armed,
                clock: &*inputs.clock,
            };
            adapter.step(&mut ctx)
        };

        match outcome {
            StepOutcome::Continue | StepOutcome::StateChanged => {}
            StepOutcome::Stopped => {
                return Ok(stop_and_close_shutter(
                    &mut inputs.backend,
                    &mut state.shutter_state,
                ))
            }
            StepOutcome::Disconnected => match reconnect(inputs, &mut *adapter, &mut state) {
                Ok(()) => continue,
                Err(exit) => return Ok(exit),
            },
        }

        if inputs.source.is_ended() {
            if let Some(err) = inputs.source.take_stop_error() {
                // IdlePolicy::Stop is an abrupt error exit, so it must use the
                // same explicit ShutterState safety path as a control stop.
                command_close(&mut inputs.backend, &mut state.shutter_state, true);
                return Err(err);
            }
            let mut ctx = LoopCtx {
                backend: &mut inputs.backend,
                source: inputs.source.as_kind(),
                control: &inputs.control,
                control_rx: &inputs.control_rx,
                metrics: &inputs.metrics,
                shutter_open: &mut state.shutter_state,
                error_sink: &mut inputs.error_sink,
                target_buffer: inputs.target_buffer,
                pps,
                is_armed,
                clock: &*inputs.clock,
            };
            adapter.drain_and_blank(&mut ctx, inputs.drain_timeout);
            return Ok(SessionExit::ProducerEnded);
        }
    }
}

/// Loop-local mutable state threaded through the driver's reconnect path.
struct LoopState {
    /// Confirmed state of the hardware shutter.
    shutter_state: ShutterState,
    /// Last arm/disarm generation fully handled by the hardware lifecycle.
    handled_state: crate::session::DesiredState,
    /// Instant the last reconnect completed; gates the flapping-device backoff
    /// floor in `reconnect_backend_with_retry`.
    last_reconnect_at: Option<Instant>,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            shutter_state: ShutterState::Closed,
            handled_state: crate::session::DesiredState::initial(),
            last_reconnect_at: None,
        }
    }
}

fn reconnect(
    inputs: &mut DriverInputs,
    adapter: &mut dyn OutputModelAdapter,
    state: &mut LoopState,
) -> std::result::Result<(), SessionExit> {
    let expected_frame_swap = inputs.source.is_frame();
    let DriverInputs {
        backend,
        source,
        control,
        metrics,
        reconnect_policy,
        validator,
        ..
    } = inputs;
    let Some(policy) = reconnect_policy else {
        return Err(SessionExit::Disconnected);
    };
    metrics.set_connected(false);
    let (info, mut new_backend) = reconnect_backend_with_retry(
        policy,
        state.last_reconnect_at,
        || control.is_stop_requested(),
        |info, new_backend| {
            if new_backend.is_frame_swap() != expected_frame_swap {
                log::error!(
                    "'{}' reconnected device has incompatible backend type",
                    policy.target.device_id
                );
                return Err(SessionExit::IncompatibleDevice);
            }
            validator(info, new_backend, control.pps())
        },
        || metrics.mark_loop_activity(),
    )?;

    if control
        .update_pps_bounds(new_backend.caps().pps_min, new_backend.caps().pps_max)
        .is_err()
    {
        // The validator's PPS snapshot can become stale if a control handle
        // changes PPS concurrently. Reject the new connection explicitly safe.
        new_backend.close_and_disconnect();
        return Err(SessionExit::IncompatibleDevice);
    }
    *backend = new_backend;
    state.shutter_state = ShutterState::Closed;
    // Reconnect starts a fresh confirmed-disarmed hardware lifecycle. Preserve
    // generation zero as the handled baseline so an already-requested arm must
    // still run full rising preparation.
    state.handled_state = crate::session::DesiredState::initial();
    state.last_reconnect_at = Some(Instant::now());
    metrics.set_connected(true);

    source.on_reconnect(&info);
    if expected_frame_swap {
        source.set_frame_capacity_if_supported(backend.frame_capacity());
    }
    adapter.on_reconnect(&info, backend);

    // Poisoning policy: user callbacks are invoked without holding any lock;
    // the callback slot recovers from poisoning via `into_inner`.
    let mut cb = policy
        .on_reconnect
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(cb) = cb.as_mut() {
        cb(&info);
    }
    if cb.is_some() {
        *policy
            .on_reconnect
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = cb;
    }

    Ok(())
}

/// Handle arm/disarm shutter transitions, mutating source-side state via the
/// `ContentSource` lifecycle methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionOutcome {
    Continue,
    Disconnected,
}

fn command_close(backend: &mut BackendKind, shutter_state: &mut ShutterState, force: bool) {
    if force || *shutter_state != ShutterState::Closed {
        *shutter_state = match backend.set_shutter(false) {
            Ok(()) => ShutterState::Closed,
            Err(_) => ShutterState::Unknown,
        };
    }
}

fn handle_shutter_transition(
    control: &SessionControl,
    handled_state: &mut crate::session::DesiredState,
    shutter_state: &mut ShutterState,
    backend: &mut BackendKind,
    source: &mut SourceOwned,
    adapter: &mut dyn OutputModelAdapter,
    pps: u32,
) -> TransitionOutcome {
    let desired = control.desired_state();
    if desired != *handled_state {
        // Any generation change ending disarmed, or any change from an armed
        // lifecycle, requires falling cleanup. This catches arm->disarm and
        // disarm->arm pairs that occur entirely between scheduler passes.
        if handled_state.is_armed() || !desired.is_armed() {
            command_close(backend, shutter_state, false);
            source.discard_cached();
            adapter.on_state_change(false);
            source.on_disarm();
            source.reset_output_filter(OutputResetReason::Disarm);
            *handled_state = desired;

            // A concurrent edge during falling cleanup must be handled by a
            // fresh pass; never reconcile it directly into an open command.
            if control.desired_state() != desired {
                return TransitionOutcome::Continue;
            }
        }

        if desired.is_armed() {
            source.discard_cached();
            adapter.on_state_change(true);
            source.arm_startup_blank(pps);
            source.reset_output_filter(OutputResetReason::Arm);
            if let Err(e) = backend.reset_device_buffer() {
                log::error!("reset_device_buffer on arm failed; disconnecting safely: {e}");
                command_close(backend, shutter_state, true);
                let _ = backend.disconnect();
                return TransitionOutcome::Disconnected;
            }
            *handled_state = desired;
        }
    }

    // Opening is permitted only for the exact generation whose rising
    // preparation completed. An error leaves hardware state unknown.
    if desired.is_armed() && control.desired_state() == desired {
        if *shutter_state != ShutterState::Open {
            *shutter_state = match backend.set_shutter(true) {
                Ok(()) => ShutterState::Open,
                Err(_) => ShutterState::Unknown,
            };
        }
        // Close a concurrent disarm even if the open command reported failure:
        // failure cannot prove that hardware stayed closed.
        if control.desired_state() != desired {
            command_close(backend, shutter_state, true);
        }
    } else if !desired.is_armed() {
        command_close(backend, shutter_state, false);
    }

    TransitionOutcome::Continue
}

/// Close the shutter (best-effort) and return [`SessionExit::Stopped`].
///
/// A `Stop` request must never leave the shutter open — otherwise the beam
/// freezes on the last bright point ("freeze on last bright point" hazard).
/// The graceful end-of-stream path closes the shutter via `drain_and_blank`;
/// this is the equivalent for every abrupt-stop exit. It also removes a race:
/// `is_stop_requested()` is checked before the control channel is drained, so a
/// `Disarm` queued just before `Stop` could otherwise be dropped, leaving the
/// shutter open.
fn stop_and_close_shutter(
    backend: &mut BackendKind,
    shutter_state: &mut ShutterState,
) -> SessionExit {
    // Stop always issues closure: cached state is not proof that connect or an
    // earlier command did not enable output behind our back.
    command_close(backend, shutter_state, true);
    SessionExit::Stopped
}

#[cfg(test)]
mod lifecycle_tests {
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
    use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
    use crate::config::ReconnectConfig;
    use crate::device::{DacCapabilities, DacInfo, DacType, EnabledDacTypes, OutputModel};
    use crate::discovery::{DacDiscovery, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
    use crate::error::{Error, Result};
    use crate::point::LaserPoint;
    use crate::presentation::content_source::FifoContentSource;
    use crate::presentation::output_model::{OutputModelAdapter, ShutterState, StepOutcome};
    use crate::presentation::OutputResetReason;
    use crate::reconnect::{ReconnectPolicy, ReconnectTarget};
    use crate::session::{DesiredState, SessionControl, SessionExit};

    use super::{
        end_session, handle_shutter_transition, reconnect, stop_and_close_shutter, DriverInputs,
        LoopState, SourceOwned,
    };

    struct LifecycleBackend {
        events: Arc<Mutex<Vec<&'static str>>>,
        shutter_results: VecDeque<Result<()>>,
        reset_results: VecDeque<Result<()>>,
        estimator: SoftwareDecayEstimator,
        caps: DacCapabilities,
    }

    impl DacBackend for LifecycleBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("lifecycle".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_shutter(&mut self, open: bool) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(if open { "open" } else { "close" });
            self.shutter_results.pop_front().unwrap_or(Ok(()))
        }
    }

    impl FifoBackend for LifecycleBackend {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }
        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
        fn reset_device_buffer(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("device-reset");
            self.reset_results.pop_front().unwrap_or(Ok(()))
        }
    }

    struct Source {
        events: Arc<Mutex<Vec<&'static str>>>,
        arm_during_disarm: Option<SessionControl>,
    }
    impl FifoContentSource for Source {
        fn produce_chunk(&mut self, _: usize, _: u32, _: bool) -> &[LaserPoint] {
            &[]
        }
        fn cached_slice(&self) -> Option<&[LaserPoint]> {
            None
        }
        fn commit_written(&mut self, _: usize, _: bool) {}
        fn discard_cached(&mut self) {
            self.events.lock().unwrap().push("discard");
        }
        fn reserve_buf(&mut self, _: usize) {}
        fn on_reconnect(&mut self, _: &DacInfo) {}
        fn is_ended(&self) -> bool {
            false
        }
        fn arm_startup_blank(&mut self, _: u32) {
            self.events.lock().unwrap().push("startup-blank");
        }
        fn on_disarm(&mut self) {
            self.events.lock().unwrap().push("source-disarm");
            if let Some(control) = self.arm_during_disarm.take() {
                control.arm().unwrap();
            }
        }
        fn reset_output_filter(&mut self, reason: OutputResetReason) {
            self.events.lock().unwrap().push(match reason {
                OutputResetReason::Arm => "filter-arm",
                OutputResetReason::Disarm => "filter-disarm",
                _ => "filter-other",
            });
        }
    }

    struct Adapter;
    impl OutputModelAdapter for Adapter {
        fn step(&mut self, _: &mut crate::presentation::output_model::LoopCtx<'_>) -> StepOutcome {
            StepOutcome::Continue
        }
        fn on_reconnect(&mut self, _: &DacInfo, _: &mut BackendKind) {}
    }

    fn harness_with(
        shutter_results: Vec<Result<()>>,
        reset_results: Vec<Result<()>>,
        arm_during_disarm: bool,
    ) -> (
        BackendKind,
        SourceOwned,
        SessionControl,
        Arc<Mutex<Vec<&'static str>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = LifecycleBackend {
            events: Arc::clone(&events),
            shutter_results: shutter_results.into(),
            reset_results: reset_results.into(),
            estimator: SoftwareDecayEstimator::new(),
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 100,
                output_model: OutputModel::NetworkFifo,
            },
        };
        let (tx, rx) = mpsc::channel();
        // The lifecycle unit harness invokes the handler directly rather than
        // running a scheduler, so retain a live notification receiver.
        std::mem::forget(rx);
        let control =
            SessionControl::new_with_pps_bounds(tx, Duration::ZERO, 30_000, 1_000, 100_000);
        let source = SourceOwned::Fifo(Box::new(Source {
            events: Arc::clone(&events),
            arm_during_disarm: arm_during_disarm.then(|| control.clone()),
        }));
        (
            BackendKind::Fifo(Box::new(backend)),
            source,
            control,
            events,
        )
    }

    fn harness(
        results: Vec<Result<()>>,
    ) -> (
        BackendKind,
        SourceOwned,
        SessionControl,
        Arc<Mutex<Vec<&'static str>>>,
    ) {
        harness_with(results, vec![], false)
    }

    struct ReconnectCleanupBackend {
        connected: bool,
        events: Arc<Mutex<Vec<&'static str>>>,
        estimator: SoftwareDecayEstimator,
        caps: DacCapabilities,
    }

    impl DacBackend for ReconnectCleanupBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("driver-reconnect-cleanup".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            self.events.lock().unwrap().push("disconnect");
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn stop(&mut self) -> Result<()> {
            self.events.lock().unwrap().push("stop");
            Ok(())
        }
        fn set_shutter(&mut self, open: bool) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(if open { "open" } else { "close" });
            Ok(())
        }
    }

    impl FifoBackend for ReconnectCleanupBackend {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }
        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    struct ReconnectCleanupDiscoverer {
        events: Arc<Mutex<Vec<&'static str>>>,
        caps: DacCapabilities,
    }

    impl Discoverer for ReconnectCleanupDiscoverer {
        fn dac_type(&self) -> DacType {
            DacType::Custom("driver-reconnect-cleanup".into())
        }
        fn prefix(&self) -> &str {
            "driver-reconnect-cleanup"
        }
        fn scan(&mut self) -> Vec<DiscoveredDevice> {
            vec![DiscoveredDevice::new(
                DiscoveredDeviceInfo::new(
                    self.dac_type(),
                    "driver-reconnect-cleanup:1",
                    "Driver reconnect cleanup",
                ),
                Box::new(()),
            )
            .with_caps(self.caps.clone())]
        }
        fn connect(&mut self, _opaque: Box<dyn std::any::Any + Send>) -> Result<BackendKind> {
            Ok(BackendKind::Fifo(Box::new(ReconnectCleanupBackend {
                connected: false,
                events: self.events.clone(),
                estimator: SoftwareDecayEstimator::new(),
                caps: self.caps.clone(),
            })))
        }
    }

    fn cleanup_backend(connected: bool) -> (BackendKind, Arc<Mutex<Vec<&'static str>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = ReconnectCleanupBackend {
            connected,
            events: Arc::clone(&events),
            estimator: SoftwareDecayEstimator::new(),
            caps: DacCapabilities {
                pps_min: 1_000,
                pps_max: 100_000,
                max_points_per_chunk: 100,
                output_model: OutputModel::NetworkFifo,
            },
        };
        (BackendKind::Fifo(Box::new(backend)), events)
    }

    #[test]
    fn end_session_stops_then_disconnects_a_connected_backend() {
        let (mut backend, events) = cleanup_backend(true);
        end_session(&mut backend);
        assert_eq!(*events.lock().unwrap(), vec!["stop", "disconnect"]);
        assert!(!backend.is_connected());
    }

    #[test]
    fn end_session_skips_a_disconnected_backend() {
        let (mut backend, events) = cleanup_backend(false);
        end_session(&mut backend);
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn reconnect_pps_bounds_race_closes_and_disconnects_rejected_backend() {
        let new_backend_events = Arc::new(Mutex::new(Vec::new()));
        let factory_events = new_backend_events.clone();
        let reconnect_caps = DacCapabilities {
            pps_min: 1_000,
            pps_max: 40_000,
            max_points_per_chunk: 100,
            output_model: OutputModel::NetworkFifo,
        };
        let factory_caps = reconnect_caps.clone();
        let target = ReconnectTarget {
            device_id: "driver-reconnect-cleanup:1".into(),
            discovery_factory: Some(Box::new(move || {
                let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
                discovery.register(Box::new(ReconnectCleanupDiscoverer {
                    events: factory_events.clone(),
                    caps: factory_caps.clone(),
                }));
                discovery
            })),
        };
        let policy = ReconnectPolicy::new(ReconnectConfig::new().max_retries(1), target);

        let (backend, source, control, _) = harness(vec![]);
        let validator_control = control.clone();
        let validator: super::ReconnectValidator =
            Box::new(move |_info: &DacInfo, backend: &BackendKind, pps: u32| {
                assert_eq!(pps, 30_000);
                if backend.is_connected() {
                    validator_control.set_pps(50_000).unwrap();
                }
                Ok(())
            });
        let metrics = crate::presentation::FrameSessionMetrics::new(true);
        let (_control_tx, control_rx) = mpsc::channel();
        let mut inputs = DriverInputs {
            backend,
            source,
            control,
            control_rx,
            metrics,
            reconnect_policy: Some(policy),
            validator,
            error_sink: Box::new(|_| {}),
            target_buffer: Duration::ZERO,
            drain_timeout: Duration::ZERO,
            pending_frame: None,
            clock: DriverInputs::system_clock(),
        };
        let mut adapter = Adapter;
        let mut state = LoopState::default();

        let result = reconnect(&mut inputs, &mut adapter, &mut state);

        assert_eq!(result, Err(SessionExit::IncompatibleDevice));
        assert_eq!(inputs.control.pps(), 50_000);
        assert_eq!(
            *new_backend_events.lock().unwrap(),
            vec!["close", "close", "disconnect"]
        );
    }

    #[test]
    fn startup_preparation_precedes_open_and_open_failure_is_retried() {
        let (mut backend, mut source, control, events) =
            harness(vec![Err(Error::invalid_config("first open fails")), Ok(())]);
        let mut adapter = Adapter;
        let mut handled = DesiredState::initial();
        let mut shutter = ShutterState::Closed;
        control.arm().unwrap();

        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Unknown);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "discard",
                "startup-blank",
                "filter-arm",
                "device-reset",
                "open"
            ]
        );

        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Open);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "open")
                .count(),
            2
        );
    }

    #[test]
    fn rapid_disarm_arm_final_armed_runs_falling_then_rising_before_open() {
        let (mut backend, mut source, control, events) = harness(vec![]);
        let mut adapter = Adapter;
        let mut handled = DesiredState::initial();
        let mut shutter = ShutterState::Closed;

        control.arm().unwrap();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        events.lock().unwrap().clear();

        control.disarm().unwrap();
        control.arm().unwrap();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );

        assert_eq!(shutter, ShutterState::Open);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "close",
                "discard",
                "source-disarm",
                "filter-disarm",
                "discard",
                "startup-blank",
                "filter-arm",
                "device-reset",
                "open"
            ]
        );
    }

    #[test]
    fn arm_arriving_mid_falling_handler_defers_open_until_next_prepared_pass() {
        let (mut backend, mut source, control, events) = harness_with(vec![], vec![], true);
        let mut adapter = Adapter;
        let mut shutter = ShutterState::Open;

        control.arm().unwrap();
        let mut handled = control.desired_state();
        control.disarm().unwrap();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Closed);
        assert!(!events.lock().unwrap().contains(&"open"));

        events.lock().unwrap().clear();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Open);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "discard",
                "startup-blank",
                "filter-arm",
                "device-reset",
                "open"
            ]
        );
    }

    #[test]
    fn failed_open_is_closed_on_disarm_and_stop_always_commands_close() {
        let (mut backend, mut source, control, events) = harness(vec![
            Err(Error::backend(std::io::Error::other("open uncertain"))),
            Ok(()),
            Ok(()),
        ]);
        let mut adapter = Adapter;
        let mut handled = DesiredState::initial();
        let mut shutter = ShutterState::Closed;

        control.arm().unwrap();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Unknown);
        control.disarm().unwrap();
        handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(shutter, ShutterState::Closed);
        stop_and_close_shutter(&mut backend, &mut shutter);
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "close")
                .count(),
            2
        );
    }

    #[test]
    fn reset_device_buffer_failure_prevents_open() {
        let (mut backend, mut source, control, events) = harness_with(
            vec![Ok(())],
            vec![Err(Error::backend(std::io::Error::other("reset failed")))],
            false,
        );
        let mut adapter = Adapter;
        let mut handled = DesiredState::initial();
        let mut shutter = ShutterState::Closed;
        control.arm().unwrap();

        let outcome = handle_shutter_transition(
            &control,
            &mut handled,
            &mut shutter,
            &mut backend,
            &mut source,
            &mut adapter,
            30_000,
        );
        assert_eq!(outcome, super::TransitionOutcome::Disconnected);
        assert!(!events.lock().unwrap().contains(&"open"));
        assert_eq!(shutter, ShutterState::Closed);
    }
}
