//! Per-`OutputModel` scheduler adapters.
//!
//! The shared driver in [`super::session`] runs the pre-iter bookkeeping
//! (frame intake, reconnect, control messages, shutter transitions). Each
//! adapter owns the variable parts: pacing wait, slice production, write,
//! and post-write bookkeeping.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::backend::BackendKind;
use crate::buffer_estimate::BufferEstimator;
use crate::device::DacInfo;
use crate::error::Error;
use crate::stream::{ControlMsg, StreamControl};

use super::content_source::ContentSourceKind;
use super::session::FrameSessionMetrics;

mod network_fifo;
mod udp_timed;
mod usb_frame_swap;

/// One step's outcome.
pub(crate) enum StepOutcome {
    Continue,
    Stopped,
    Disconnected,
}

/// Shared context borrowed by the adapter for one `step`.
pub(crate) struct LoopCtx<'a> {
    pub backend: &'a mut BackendKind,
    pub source: ContentSourceKind<'a>,
    pub control: &'a StreamControl,
    pub control_rx: &'a mpsc::Receiver<ControlMsg>,
    pub metrics: &'a FrameSessionMetrics,
    pub shutter_open: &'a mut bool,
    pub error_sink: &'a mut dyn FnMut(Error),
    pub target_buffer: Duration,
    pub pps: u32,
    pub is_armed: bool,
}

impl<'a> LoopCtx<'a> {
    /// Sleep with control-message processing and shutter handling.
    /// Returns `StepOutcome::Stopped` if a stop is requested mid-sleep
    /// (either via the atomic flag or a `ControlMsg::Stop` on the channel).
    pub fn sleep_with_control_check(&mut self, duration: Duration) -> Result<(), StepOutcome> {
        const SLICE: Duration = Duration::from_millis(2);
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            let slice = remaining.min(SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
            self.metrics.mark_loop_activity();
            if self.control.is_stop_requested() {
                return Err(StepOutcome::Stopped);
            }
            if process_control_messages(self.control_rx, self.shutter_open, self.backend) {
                return Err(StepOutcome::Stopped);
            }
        }
        Ok(())
    }

    /// High-precision sleep until `deadline` (UDP-timed pacing).
    pub fn sleep_until_precise(&mut self, deadline: Instant) -> Result<(), StepOutcome> {
        const BUSY_WAIT_THRESHOLD: Duration = Duration::from_micros(500);
        const SLEEP_SLICE: Duration = Duration::from_millis(1);

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }

            let remaining = deadline.duration_since(now);
            if remaining > BUSY_WAIT_THRESHOLD {
                let slice = remaining
                    .saturating_sub(BUSY_WAIT_THRESHOLD)
                    .min(SLEEP_SLICE);
                std::thread::sleep(slice);
            } else {
                std::thread::yield_now();
            }
            self.metrics.mark_loop_activity();

            if self.control.is_stop_requested() {
                return Err(StepOutcome::Stopped);
            }
            if process_control_messages(self.control_rx, self.shutter_open, self.backend) {
                return Err(StepOutcome::Stopped);
            }
        }
    }

    pub fn sleep_and_mark_activity(&self, duration: Duration) {
        std::thread::sleep(duration);
        self.metrics.mark_loop_activity();
    }
}

/// Drain the control-message channel, applying arm/disarm shutter side effects.
/// Returns `true` if a stop was requested.
pub(crate) fn process_control_messages(
    control_rx: &mpsc::Receiver<ControlMsg>,
    shutter_open: &mut bool,
    backend: &mut BackendKind,
) -> bool {
    use std::sync::mpsc::TryRecvError;
    loop {
        match control_rx.try_recv() {
            Ok(ControlMsg::Arm) => {
                if !*shutter_open {
                    let _ = backend.set_shutter(true);
                    *shutter_open = true;
                }
            }
            Ok(ControlMsg::Disarm) => {
                if *shutter_open {
                    let _ = backend.set_shutter(false);
                    *shutter_open = false;
                }
            }
            Ok(ControlMsg::Stop) => {
                return true;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    false
}

/// One per [`OutputModel`](crate::device::OutputModel) variant.
pub(crate) trait OutputModelAdapter: Send {
    /// Run one step of the variable scheduler work.
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome;

    /// Called by the driver after a successful reconnect. Adapters update
    /// their own pacing state and may reach into the backend; source-side
    /// reset (re-pending the last frame, resetting buffers, frame capacity)
    /// is performed by the driver against the `ContentSource` separately.
    fn on_reconnect(&mut self, info: &DacInfo, backend: &mut BackendKind);

    /// Wait for queued points to drain, then output a small blank chunk and
    /// close the shutter. Called by the driver on graceful end-of-stream.
    /// Default is a best-effort blank-and-close — FIFO models override to
    /// poll the estimator until empty or `timeout` elapses; UsbFrameSwap
    /// uses the default (no queue to drain).
    fn drain_and_blank(&mut self, ctx: &mut LoopCtx<'_>, _timeout: Duration) {
        blank_and_close_shutter(ctx);
    }
}

/// Output a small blank chunk and close the shutter. Best-effort safety
/// shutdown shared between drain paths.
pub(crate) fn blank_and_close_shutter(ctx: &mut LoopCtx<'_>) {
    use crate::point::LaserPoint;
    let _ = ctx.backend.set_shutter(false);
    *ctx.shutter_open = false;
    let blank = [LaserPoint::blanked(0.0, 0.0); 16];
    let _ = ctx.backend.try_write(ctx.pps, &blank);
}

/// Query an optional [`BufferEstimator`] for queued points, skipping the
/// `Instant::now()` clock query when the estimator ignores it
/// ([`RuntimeAuthorityEstimator`] for AVB/Oscilloscope). The sentinel is a
/// thread-local cached `Instant` — safe because estimators that ignore `now`
/// never read it.
pub(crate) fn estimator_fullness(estimator: Option<&dyn BufferEstimator>, pps: u32) -> u64 {
    estimator.map_or(0, |e| {
        let now = if e.needs_clock() {
            Instant::now()
        } else {
            sentinel_instant()
        };
        e.estimated_fullness(now, pps)
    })
}

fn sentinel_instant() -> Instant {
    thread_local! {
        static SENTINEL: Instant = Instant::now();
    }
    SENTINEL.with(|t| *t)
}

/// Poll the backend's [`BufferEstimator`] until it reports an empty queue or
/// the timeout elapses. Used by FIFO/UdpTimed `drain_and_blank`.
pub(crate) fn drain_via_estimator(ctx: &mut LoopCtx<'_>, timeout: Duration) {
    if timeout.is_zero() {
        return;
    }
    let start = Instant::now();
    let deadline = start + timeout;
    const POLL: Duration = Duration::from_millis(5);
    let mut now = start;
    while now < deadline {
        let buffered = estimator_fullness(ctx.backend.estimator(), ctx.pps);
        if buffered == 0 {
            break;
        }
        if ctx.control.is_stop_requested() {
            break;
        }
        if process_control_messages(ctx.control_rx, ctx.shutter_open, ctx.backend) {
            break;
        }
        std::thread::sleep(POLL);
        ctx.metrics.mark_loop_activity();
        now = Instant::now();
    }
}

/// Construct the adapter for a backend's output model.
pub(crate) fn for_backend(backend: &BackendKind) -> Box<dyn OutputModelAdapter> {
    use crate::device::OutputModel;
    match backend.caps().output_model {
        OutputModel::UsbFrameSwap => Box::new(usb_frame_swap::UsbFrameSwapAdapter::new(backend)),
        OutputModel::NetworkFifo => Box::new(network_fifo::NetworkFifoAdapter::new(backend)),
        OutputModel::UdpTimed => Box::new(udp_timed::UdpTimedAdapter::new(backend)),
    }
}
