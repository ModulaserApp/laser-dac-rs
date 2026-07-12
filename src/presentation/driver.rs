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
use crate::stream::{ControlMsg, RunExit, StreamControl};

use super::content_source::{ContentSourceKind, FifoContentSource, FrameContentSource};
use super::output_model::{
    self, process_control_messages, Clock, LoopCtx, OutputModelAdapter, StepOutcome, SystemClock,
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

    fn resize_color_delay_micros(&mut self, micros: u64, pps: u32) {
        match self {
            SourceOwned::Fifo(s) => s.resize_color_delay_micros(micros, pps),
            SourceOwned::Frame(s) => s.resize_color_delay_micros(micros, pps),
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
/// `Err(RunExit::...)` rejects with the given exit reason.
pub(crate) type ReconnectValidator =
    Box<dyn Fn(&DacInfo, &BackendKind, u32) -> std::result::Result<(), RunExit> + Send>;

/// Sink for non-fatal write errors. Stream-mode threads the user's
/// `on_error`; frame-mode passes a no-op.
pub(crate) type ErrorSink = Box<dyn FnMut(Error) + Send>;

pub(crate) struct DriverInputs {
    pub backend: BackendKind,
    pub source: SourceOwned,
    pub control: StreamControl,
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
pub(crate) fn run(mut inputs: DriverInputs) -> Result<RunExit> {
    let expected_frame_swap = inputs.source.is_frame();
    let mut adapter = output_model::for_backend(&inputs.backend);

    let mut shutter_open = false;
    let mut last_armed = false;
    let mut error_sink = inputs.error_sink;
    // Instant the last reconnect completed; gates the flapping-device backoff
    // floor in `reconnect_backend_with_retry`.
    let mut last_reconnect_at: Option<Instant> = None;

    loop {
        inputs.metrics.mark_loop_activity();
        if inputs.control.is_stop_requested() {
            return Ok(stop_and_close_shutter(
                &mut inputs.backend,
                &mut shutter_open,
            ));
        }

        let pps = inputs.control.pps();
        let color_delay_micros = inputs.control.color_delay().as_micros() as u64;
        inputs
            .source
            .resize_color_delay_micros(color_delay_micros, pps);
        if let Some(slot) = inputs.pending_frame.as_ref() {
            if let Some(frame) = slot.lock().unwrap().take() {
                inputs.source.submit_frame(frame);
            }
        }

        if !inputs.backend.is_connected() {
            match reconnect(
                &mut inputs.backend,
                inputs.reconnect_policy.as_ref(),
                &inputs.validator,
                &mut inputs.source,
                expected_frame_swap,
                &inputs.control,
                &mut shutter_open,
                &mut last_armed,
                &inputs.metrics,
                &mut *adapter,
                &mut last_reconnect_at,
            ) {
                Ok(()) => continue,
                Err(exit) => return Ok(exit),
            }
        }

        if process_control_messages(&inputs.control_rx, &mut shutter_open, &mut inputs.backend) {
            return Ok(stop_and_close_shutter(
                &mut inputs.backend,
                &mut shutter_open,
            ));
        }
        let is_armed = inputs.control.is_armed();
        handle_shutter_transition(
            is_armed,
            &mut last_armed,
            &mut shutter_open,
            &mut inputs.backend,
            &mut inputs.source,
            pps,
        );

        let outcome = {
            let source = inputs.source.as_kind();
            let mut ctx = LoopCtx {
                backend: &mut inputs.backend,
                source,
                control: &inputs.control,
                control_rx: &inputs.control_rx,
                metrics: &inputs.metrics,
                shutter_open: &mut shutter_open,
                error_sink: &mut *error_sink,
                target_buffer: inputs.target_buffer,
                pps,
                is_armed,
                clock: &*inputs.clock,
            };
            adapter.step(&mut ctx)
        };

        match outcome {
            StepOutcome::Continue => {}
            StepOutcome::Stopped => {
                return Ok(stop_and_close_shutter(
                    &mut inputs.backend,
                    &mut shutter_open,
                ))
            }
            StepOutcome::Disconnected => {
                match reconnect(
                    &mut inputs.backend,
                    inputs.reconnect_policy.as_ref(),
                    &inputs.validator,
                    &mut inputs.source,
                    expected_frame_swap,
                    &inputs.control,
                    &mut shutter_open,
                    &mut last_armed,
                    &inputs.metrics,
                    &mut *adapter,
                    &mut last_reconnect_at,
                ) {
                    Ok(()) => continue,
                    Err(exit) => return Ok(exit),
                }
            }
        }

        if inputs.source.is_ended() {
            if let Some(err) = inputs.source.take_stop_error() {
                return Err(err);
            }
            let mut ctx = LoopCtx {
                backend: &mut inputs.backend,
                source: inputs.source.as_kind(),
                control: &inputs.control,
                control_rx: &inputs.control_rx,
                metrics: &inputs.metrics,
                shutter_open: &mut shutter_open,
                error_sink: &mut *error_sink,
                target_buffer: inputs.target_buffer,
                pps,
                is_armed,
                clock: &*inputs.clock,
            };
            adapter.drain_and_blank(&mut ctx, inputs.drain_timeout);
            return Ok(RunExit::ProducerEnded);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconnect(
    backend: &mut BackendKind,
    policy: Option<&ReconnectPolicy>,
    validator: &ReconnectValidator,
    source: &mut SourceOwned,
    expected_frame_swap: bool,
    control: &StreamControl,
    shutter_open: &mut bool,
    last_armed: &mut bool,
    metrics: &FrameSessionMetrics,
    adapter: &mut dyn OutputModelAdapter,
    last_reconnect_at: &mut Option<Instant>,
) -> std::result::Result<(), RunExit> {
    let Some(policy) = policy else {
        return Err(RunExit::Disconnected);
    };
    metrics.set_connected(false);
    let (info, new_backend) = reconnect_backend_with_retry(
        policy,
        *last_reconnect_at,
        || control.is_stop_requested(),
        |info, new_backend| {
            if new_backend.is_frame_swap() != expected_frame_swap {
                log::error!(
                    "'{}' reconnected device has incompatible backend type",
                    policy.target.device_id
                );
                return Err(RunExit::Disconnected);
            }
            validator(info, new_backend, control.pps())
        },
        || metrics.mark_loop_activity(),
    )?;

    *backend = new_backend;
    *shutter_open = false;
    *last_armed = false;
    *last_reconnect_at = Some(Instant::now());
    metrics.set_connected(true);

    source.on_reconnect(&info);
    if expected_frame_swap {
        source.set_frame_capacity_if_supported(backend.frame_capacity());
    }
    adapter.on_reconnect(&info, backend);

    if let Some(cb) = policy.on_reconnect.lock().unwrap().as_mut() {
        cb(&info);
    }

    Ok(())
}

/// Handle arm/disarm shutter transitions, mutating source-side state via the
/// `ContentSource` lifecycle methods.
fn handle_shutter_transition(
    is_armed: bool,
    last_armed: &mut bool,
    shutter_open: &mut bool,
    backend: &mut BackendKind,
    source: &mut SourceOwned,
    pps: u32,
) {
    if !*last_armed && is_armed {
        source.arm_startup_blank(pps);
        source.reset_output_filter(OutputResetReason::Arm);
        if !*shutter_open {
            let _ = backend.set_shutter(true);
            *shutter_open = true;
        }
    } else if *last_armed && !is_armed {
        source.on_disarm();
        source.reset_output_filter(OutputResetReason::Disarm);
        if *shutter_open {
            let _ = backend.set_shutter(false);
            *shutter_open = false;
        }
    }
    *last_armed = is_armed;
}

/// Close the shutter (best-effort) and return [`RunExit::Stopped`].
///
/// A `Stop` request must never leave the shutter open — otherwise the beam
/// freezes on the last bright point ("freeze on last bright point" hazard).
/// The graceful end-of-stream path closes the shutter via `drain_and_blank`;
/// this is the equivalent for every abrupt-stop exit. It also removes a race:
/// `is_stop_requested()` is checked before the control channel is drained, so a
/// `Disarm` queued just before `Stop` could otherwise be dropped, leaving the
/// shutter open.
fn stop_and_close_shutter(backend: &mut BackendKind, shutter_open: &mut bool) -> RunExit {
    if *shutter_open {
        let _ = backend.set_shutter(false);
        *shutter_open = false;
    }
    RunExit::Stopped
}
