//! NetworkFifo adapter — buffer-estimation pacing for FIFO-style DACs
//! (Ether Dream, IDN, LaserCube, AVB).

use std::time::{Duration, Instant};

use crate::backend::{BackendKind, WriteOutcome};
use crate::device::DacInfo;
use crate::scheduler;

use super::super::slice_pipeline::SlicePipeline;
use super::{LoopCtx, OutputModelAdapter, StepOutcome};

const TARGET_BUFFER_SECS: f64 = 0.020;

pub(crate) struct NetworkFifoAdapter {
    scheduled_ahead: u64,
    fractional_consumed: f64,
    last_iteration: Instant,
    max_points: usize,
}

impl NetworkFifoAdapter {
    pub fn new(backend: &BackendKind) -> Self {
        Self {
            scheduled_ahead: 0,
            fractional_consumed: 0.0,
            last_iteration: Instant::now(),
            max_points: backend.caps().max_points_per_chunk,
        }
    }
}

impl OutputModelAdapter for NetworkFifoAdapter {
    fn step(&mut self, ctx: &mut LoopCtx<'_>) -> StepOutcome {
        let pps = ctx.pps;
        let pps_f64 = pps as f64;
        let target_buffer_points = (TARGET_BUFFER_SECS * pps_f64) as u64;

        let now = Instant::now();
        scheduler::advance_scheduled_ahead(
            &mut self.scheduled_ahead,
            &mut self.fractional_consumed,
            &mut self.last_iteration,
            now,
            pps_f64,
        );

        let buffered = scheduler::conservative_buffered_points(
            self.scheduled_ahead,
            ctx.backend.queued_points(),
        );

        if buffered > target_buffer_points {
            let excess = buffered - target_buffer_points;
            let sleep_time = Duration::from_secs_f64(excess as f64 / pps_f64.max(1.0));
            if let Err(stopped) = ctx.sleep_with_control_check(sleep_time) {
                return stopped;
            }
            return StepOutcome::Continue;
        }

        let deficit = (TARGET_BUFFER_SECS - buffered as f64 / pps_f64.max(1.0)).max(0.0);
        let target_points = ((deficit * pps_f64).ceil() as usize).min(self.max_points);
        if target_points == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        let n = ctx
            .pipeline
            .produce_fifo_chunk(target_points, pps, ctx.is_armed)
            .len();
        if n == 0 {
            ctx.sleep_and_mark_activity(Duration::from_millis(1));
            return StepOutcome::Continue;
        }

        // Inner WouldBlock spin: ~100µs hardware drain assumption.
        loop {
            let outcome = {
                let slice = match ctx.pipeline.cached_slice() {
                    Some(s) => s,
                    None => return StepOutcome::Continue,
                };
                ctx.backend.try_write(pps, slice)
            };
            match outcome {
                Ok(WriteOutcome::Written) => {
                    ctx.metrics.mark_write_success();
                    self.scheduled_ahead += n as u64;
                    ctx.pipeline.invalidate();
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
                Err(e) if e.is_disconnected() => return StepOutcome::Disconnected,
                Err(_) => break,
            }
        }
        StepOutcome::Continue
    }

    fn on_reconnect(
        &mut self,
        info: &DacInfo,
        pipeline: &mut SlicePipeline,
        _backend: &mut BackendKind,
    ) {
        self.scheduled_ahead = 0;
        self.fractional_consumed = 0.0;
        self.last_iteration = Instant::now();
        self.max_points = info.caps.max_points_per_chunk;
        pipeline.reserve_buf(self.max_points);
    }
}
