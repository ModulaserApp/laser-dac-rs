//! NetworkFifo adapter — buffer-estimation pacing for FIFO-style DACs
//! (Ether Dream, IDN, LaserCube, AVB).

use std::time::Duration;

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;

use super::super::content_source::{ContentSourceKind, FifoContentSource};
use super::{
    blank_and_close_shutter, estimator_fullness, LoopCtx, OutputModelAdapter, StepOutcome,
};

/// Re-borrow the loop-context source as a `FifoContentSource`. NetworkFifo is
/// only ever paired with a Fifo source by the driver; mismatch is a programmer
/// error.
fn fifo_source<'a>(source: &'a mut ContentSourceKind<'_>) -> &'a mut dyn FifoContentSource {
    match source {
        ContentSourceKind::Fifo(s) => &mut **s,
        ContentSourceKind::Frame(_) => {
            unreachable!("NetworkFifoAdapter requires a Fifo content source")
        }
    }
}

pub(crate) struct NetworkFifoAdapter {
    max_points: usize,
}

impl NetworkFifoAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        Self {
            max_points: backend.caps().max_points_per_chunk,
        }
    }
}

impl OutputModelAdapter for NetworkFifoAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        let pps = ctx.pps;
        let pps_f64 = pps as f64;
        let target_buffer_secs = ctx.target_buffer.as_secs_f64();
        let target_buffer_points = (target_buffer_secs * pps_f64) as u64;

        let buffered = estimator_fullness(ctx.backend.estimator(), pps);

        if buffered > target_buffer_points {
            let excess = buffered - target_buffer_points;
            let sleep_time = Duration::from_secs_f64(excess as f64 / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(sleep_time) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        let deficit = (target_buffer_secs - buffered as f64 / pps_f64.max(1.0)).max(0.0);
        let target_points = ((deficit * pps_f64).ceil() as usize).min(self.max_points);
        if target_points == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        let n = fifo_source(&mut ctx.source)
            .produce_chunk(target_points, pps, ctx.is_armed)
            .len();
        if n == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        // Inner WouldBlock spin: ~100µs hardware drain assumption.
        loop {
            let outcome = match fifo_source(&mut ctx.source).cached_slice() {
                Some(slice) => ctx.backend.try_write(pps, slice),
                None => return StepOutcome::Continue,
            };
            match outcome {
                Ok(WriteOutcome::Written) => {
                    ctx.metrics.mark_write_success();
                    fifo_source(&mut ctx.source).commit_written(n, ctx.is_armed);
                    break;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    ctx.metrics.mark_loop_activity();
                    std::thread::yield_now();
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
        }
        StepOutcome::Continue
    }

    fn on_reconnect(&mut self, info: &DacInfo, _backend: &mut BackendKind) {
        self.max_points = info.caps.max_points_per_chunk;
    }

    fn drain_and_blank(&mut self, ctx: &mut LoopCtx<'_>, timeout: Duration) {
        super::drain_via_estimator(ctx, timeout);
        blank_and_close_shutter(ctx);
    }
}
