//! Per-`OutputModel` scheduler adapters.
//!
//! The shared driver in [`super::session`] runs the pre-iter bookkeeping
//! (frame intake, reconnect, control messages, shutter transitions). Each
//! adapter owns the variable parts: pacing wait, slice production, write,
//! and post-write bookkeeping.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::backend::BackendKind;
use crate::device::DacInfo;
use crate::stream::{ControlMsg, StreamControl};

use super::session::FrameSessionMetrics;
use super::slice_pipeline::SlicePipeline;

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
    pub pipeline: &'a mut SlicePipeline,
    pub control: &'a StreamControl,
    pub control_rx: &'a mpsc::Receiver<ControlMsg>,
    pub metrics: &'a FrameSessionMetrics,
    pub shutter_open: &'a mut bool,
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

    /// Called by the driver after a successful reconnect.
    fn on_reconnect(
        &mut self,
        info: &DacInfo,
        pipeline: &mut SlicePipeline,
        backend: &mut BackendKind,
    );
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
