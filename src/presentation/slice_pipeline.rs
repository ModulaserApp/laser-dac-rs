//! SlicePipeline — owns the engine, color delay, output filter, working buffer,
//! and a slice-freshness cache.
//!
//! Concentrates the "fresh slices get filtered, retained slices don't"
//! invariant in one place so adapters with retain semantics can ask for the
//! cached, already-filtered slice on `WouldBlock`.

use std::time::Duration;

use crate::config::IdlePolicy;
use crate::device::DacInfo;
use crate::point::LaserPoint;

use super::content_source::{FifoContentSource, FrameContentSource};
use super::engine::{ColorDelayLine, PresentationEngine};
use super::{Frame, OutputFilter, OutputFilterContext, OutputResetReason, PresentedSliceKind};

pub(crate) struct SlicePipeline {
    engine: PresentationEngine,
    color_delay: ColorDelayLine,
    output_filter: Option<Box<dyn OutputFilter>>,
    idle_policy: IdlePolicy,
    /// Working buffer. Reused across iterations; resized on demand.
    buf: Vec<LaserPoint>,
    /// Logical length of the cached slice in `buf`.
    cached_len: usize,
    /// True when `buf[..cached_len]` is a previously-filtered slice that
    /// has not yet been written.
    cached: bool,
    startup_blank_remaining: usize,
    /// Configured startup-blank duration; converted to points on each arm.
    startup_blank: Duration,
    /// Most recently submitted frame, retained so `on_reconnect` can re-prime
    /// the engine after the driver replaces the backend.
    last_pending_frame: Option<Frame>,
}

impl SlicePipeline {
    #[cfg(test)]
    pub fn new(
        engine: PresentationEngine,
        color_delay_points: usize,
        output_filter: Option<Box<dyn OutputFilter>>,
        idle_policy: IdlePolicy,
        initial_buf_capacity: usize,
    ) -> Self {
        Self::with_startup_blank(
            engine,
            color_delay_points,
            output_filter,
            idle_policy,
            initial_buf_capacity,
            Duration::ZERO,
        )
    }

    pub fn with_startup_blank(
        mut engine: PresentationEngine,
        color_delay_points: usize,
        output_filter: Option<Box<dyn OutputFilter>>,
        idle_policy: IdlePolicy,
        initial_buf_capacity: usize,
        startup_blank: Duration,
    ) -> Self {
        // Under `Park`, hold the configured position whenever the engine has no
        // drawable content (no frame *or* an empty "clear the display" frame),
        // including mid-chunk promotion of an empty pending frame — otherwise the
        // engine would snap those to the origin.
        if let IdlePolicy::Park { x, y } = idle_policy {
            engine.set_idle_blank_point(LaserPoint::blanked(x, y));
        }
        Self {
            engine,
            color_delay: ColorDelayLine::new(color_delay_points),
            output_filter,
            idle_policy,
            buf: vec![LaserPoint::default(); initial_buf_capacity],
            cached_len: 0,
            cached: false,
            startup_blank_remaining: 0,
            startup_blank,
            last_pending_frame: None,
        }
    }

    // === pass-throughs ===

    pub fn set_pending(&mut self, frame: Frame) {
        self.last_pending_frame = Some(frame.clone());
        self.engine.set_pending(frame);
    }

    #[allow(dead_code)]
    pub fn has_logical_frame(&self) -> bool {
        self.engine.has_logical_frame()
    }

    pub fn reset_engine(&mut self) {
        self.engine.reset();
        self.invalidate();
    }

    pub fn set_frame_capacity(&mut self, cap: Option<usize>) {
        self.engine.set_frame_capacity(cap);
    }

    pub fn resize_color_delay(&mut self, n: usize) {
        self.color_delay.resize(n);
    }

    pub fn reset_color_delay(&mut self) {
        self.color_delay.reset();
    }

    pub fn reset_output_filter(&mut self, reason: OutputResetReason) {
        if let Some(f) = self.output_filter.as_deref_mut() {
            f.reset(reason);
        }
    }

    pub fn arm_startup_blank(&mut self, points: usize) {
        self.startup_blank_remaining = points;
    }

    /// Ensure the working buffer can hold at least `n` points without realloc.
    pub fn reserve_buf(&mut self, n: usize) {
        if self.buf.len() < n {
            self.buf.resize(n, LaserPoint::default());
        }
    }

    // === slice production ===

    /// Produce a fresh FIFO chunk. Returns `&[]` if no logical frame and no
    /// startup-blank work is needed. Otherwise returns the filtered slice.
    /// Sets the cache.
    pub fn produce_fifo_chunk(
        &mut self,
        target_points: usize,
        pps: u32,
        is_armed: bool,
    ) -> &[LaserPoint] {
        self.reserve_buf(target_points);
        let n = self.engine.fill_chunk(&mut self.buf, target_points);
        if n == 0 {
            self.invalidate();
            return &[];
        }

        // When the engine has no drawable content it fills the configured
        // idle-blank point (park position under `IdlePolicy::Park`, origin
        // otherwise) — see `PresentationEngine::idle_blank`. No extra handling
        // needed here.
        apply_blanking(
            is_armed,
            &mut self.startup_blank_remaining,
            &mut self.buf[..n],
            &self.idle_policy,
        );

        self.color_delay.apply(&mut self.buf[..n]);

        if self.engine.has_logical_frame() {
            if let Some(f) = self.output_filter.as_deref_mut() {
                f.filter(
                    &mut self.buf[..n],
                    &OutputFilterContext {
                        pps,
                        kind: PresentedSliceKind::FifoChunk,
                        is_cyclic: false,
                    },
                );
            }
        }

        self.cached_len = n;
        self.cached = true;
        &self.buf[..n]
    }

    /// Produce a fresh frame-swap frame. Returns `&[]` if no logical frame.
    /// Sets the cache.
    pub fn produce_frame_swap(&mut self, pps: u32, is_armed: bool) -> &[LaserPoint] {
        let n = {
            let composed = self.engine.compose_hardware_frame();
            if composed.is_empty() {
                self.invalidate();
                return &[];
            }
            let n = composed.len();
            // `reserve_buf` would reborrow `self` and invalidate `composed`;
            // inline the resize here to keep the borrow live for the copy below.
            if self.buf.len() < n {
                self.buf.resize(n, LaserPoint::default());
            }
            self.buf[..n].copy_from_slice(composed);
            n
        };

        apply_blanking(
            is_armed,
            &mut self.startup_blank_remaining,
            &mut self.buf[..n],
            &self.idle_policy,
        );

        self.color_delay.apply(&mut self.buf[..n]);

        if let Some(f) = self.output_filter.as_deref_mut() {
            f.filter(
                &mut self.buf[..n],
                &OutputFilterContext {
                    pps,
                    kind: PresentedSliceKind::FrameSwapFrame,
                    is_cyclic: true,
                },
            );
        }

        self.cached_len = n;
        self.cached = true;
        &self.buf[..n]
    }

    /// Return the previously-cached filtered slice, if one is set.
    /// Adapters MUST handle `None` rather than `unwrap()`.
    pub fn cached_slice(&self) -> Option<&[LaserPoint]> {
        if self.cached {
            Some(&self.buf[..self.cached_len])
        } else {
            None
        }
    }

    /// Drop the cache. Adapters call this after a successful `Written`.
    pub fn invalidate(&mut self) {
        self.cached = false;
        self.cached_len = 0;
    }

    /// Shared body for `on_reconnect` across both [`FifoContentSource`] and
    /// [`FrameContentSource`] impls: reset engine + color delay + cache,
    /// re-pend the most recent frame, fire the filter `Reconnect` reset.
    fn replay_after_reconnect(&mut self) {
        self.reset_engine();
        self.reset_color_delay();
        if let Some(frame) = self.last_pending_frame.clone() {
            self.engine.set_pending(frame);
        }
        self.reset_output_filter(OutputResetReason::Reconnect);
    }
}

impl FifoContentSource for SlicePipeline {
    fn produce_chunk(&mut self, target_points: usize, pps: u32, is_armed: bool) -> &[LaserPoint] {
        self.produce_fifo_chunk(target_points, pps, is_armed)
    }

    fn cached_slice(&self) -> Option<&[LaserPoint]> {
        SlicePipeline::cached_slice(self)
    }

    fn commit_written(&mut self, _n: usize, _is_armed: bool) {
        // Frame composition has no per-write derived state to advance for
        // SlicePipeline; clearing the cache is sufficient.
        self.invalidate();
    }

    fn discard_cached(&mut self) {
        self.invalidate();
    }

    fn reserve_buf(&mut self, n: usize) {
        SlicePipeline::reserve_buf(self, n);
    }

    fn on_reconnect(&mut self, _info: &DacInfo) {
        self.replay_after_reconnect();
    }

    fn is_ended(&self) -> bool {
        // Frame intake never ends from the source side.
        false
    }

    fn submit_frame(&mut self, frame: Frame) {
        self.set_pending(frame);
    }

    fn arm_startup_blank(&mut self, pps: u32) {
        let points = duration_to_points(self.startup_blank, pps);
        SlicePipeline::arm_startup_blank(self, points);
    }

    fn reset_output_filter(&mut self, reason: OutputResetReason) {
        SlicePipeline::reset_output_filter(self, reason);
    }

    fn resize_color_delay_micros(&mut self, micros: u64, pps: u32) {
        let points = duration_micros_to_points(micros, pps);
        self.resize_color_delay(points);
    }
}

impl FrameContentSource for SlicePipeline {
    fn produce_frame(&mut self, pps: u32, is_armed: bool) -> &[LaserPoint] {
        self.produce_frame_swap(pps, is_armed)
    }

    fn cached_slice(&self) -> Option<&[LaserPoint]> {
        SlicePipeline::cached_slice(self)
    }

    fn commit_written(&mut self, _n: usize, _is_armed: bool) {
        self.invalidate();
    }

    fn discard_cached(&mut self) {
        self.invalidate();
    }

    fn on_reconnect(&mut self, _info: &DacInfo) {
        self.replay_after_reconnect();
    }

    fn set_frame_capacity(&mut self, cap: Option<usize>) {
        SlicePipeline::set_frame_capacity(self, cap);
    }

    fn submit_frame(&mut self, frame: Frame) {
        self.set_pending(frame);
    }

    fn arm_startup_blank(&mut self, pps: u32) {
        let points = duration_to_points(self.startup_blank, pps);
        SlicePipeline::arm_startup_blank(self, points);
    }

    fn reset_output_filter(&mut self, reason: OutputResetReason) {
        SlicePipeline::reset_output_filter(self, reason);
    }

    fn resize_color_delay_micros(&mut self, micros: u64, pps: u32) {
        let points = duration_micros_to_points(micros, pps);
        self.resize_color_delay(points);
    }
}

fn duration_to_points(d: Duration, pps: u32) -> usize {
    if d.is_zero() || pps == 0 {
        0
    } else {
        (d.as_secs_f64() * pps as f64).ceil() as usize
    }
}

fn duration_micros_to_points(micros: u64, pps: u32) -> usize {
    if micros == 0 || pps == 0 {
        0
    } else {
        (micros as f64 * pps as f64 / 1_000_000.0).ceil() as usize
    }
}

/// Apply disarm blanking and startup blanking to a buffer.
fn apply_blanking(
    is_armed: bool,
    startup_blank_remaining: &mut usize,
    buffer: &mut [LaserPoint],
    idle_policy: &IdlePolicy,
) {
    if !is_armed {
        let park = match idle_policy {
            IdlePolicy::Park { x, y } => LaserPoint::blanked(*x, *y),
            _ => LaserPoint::blanked(0.0, 0.0),
        };
        buffer.fill(park);
    } else if *startup_blank_remaining > 0 {
        let blank_count = buffer.len().min(*startup_blank_remaining);
        for p in &mut buffer[..blank_count] {
            p.r = 0;
            p.g = 0;
            p.b = 0;
            p.intensity = 0;
        }
        *startup_blank_remaining -= blank_count;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::presentation::engine::PresentationEngine;
    use crate::presentation::{Frame, TransitionPlan};

    fn lit_point(x: f32) -> LaserPoint {
        LaserPoint::new(x, 0.0, 1000, 1000, 1000, 1000)
    }

    fn make_engine() -> PresentationEngine {
        PresentationEngine::new(Box::new(|_, _| TransitionPlan::Transition(Vec::new())))
    }

    fn make_pipeline(initial_cap: usize) -> SlicePipeline {
        SlicePipeline::new(make_engine(), 0, None, IdlePolicy::Blank, initial_cap)
    }

    #[test]
    fn armed_no_frame_honors_idle_policy_park() {
        // Armed but no frame submitted yet: output should hold the configured
        // park position, not blank at the origin.
        let mut pipeline = SlicePipeline::new(
            make_engine(),
            0,
            None,
            IdlePolicy::Park { x: 0.25, y: -0.5 },
            0,
        );
        let chunk: Vec<LaserPoint> = pipeline.produce_fifo_chunk(4, 30_000, true).to_vec();
        assert_eq!(chunk.len(), 4);
        let park = LaserPoint::blanked(0.25, -0.5);
        for p in &chunk {
            assert_eq!(p.x, park.x);
            assert_eq!(p.y, park.y);
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }
    }

    #[test]
    fn armed_empty_frame_honors_idle_policy_park() {
        // An empty ("clear the display") frame reports `has_logical_frame() ==
        // true` but has no drawable content, so the engine fills the idle-blank
        // point. Under `Park` that must be the configured position, not (0,0).
        let mut pipeline = SlicePipeline::new(
            make_engine(),
            0,
            None,
            IdlePolicy::Park { x: 0.25, y: -0.5 },
            0,
        );
        pipeline.set_pending(Frame::new(Vec::new()));
        let chunk: Vec<LaserPoint> = pipeline.produce_fifo_chunk(4, 30_000, true).to_vec();
        assert_eq!(chunk.len(), 4);
        let park = LaserPoint::blanked(0.25, -0.5);
        for p in &chunk {
            assert_eq!(p.x, park.x);
            assert_eq!(p.y, park.y);
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }
    }

    #[test]
    fn produce_frame_swap_no_frame_returns_empty_and_no_cache() {
        // Frame-swap before any frame: empty composed → empty slice and no cache.
        let mut pipeline = make_pipeline(8);
        let slice = pipeline.produce_frame_swap(30_000, true);
        assert!(slice.is_empty());
        assert!(pipeline.cached_slice().is_none());
    }

    #[test]
    fn produce_fifo_then_cached_returns_same_bytes() {
        let mut pipeline = make_pipeline(0);
        pipeline.set_pending(Frame::new(vec![lit_point(0.0), lit_point(0.5)]));
        let produced: Vec<LaserPoint> = pipeline.produce_fifo_chunk(8, 30_000, true).to_vec();
        assert!(!produced.is_empty());
        let cached = pipeline.cached_slice().expect("cache set after produce");
        assert_eq!(cached, produced.as_slice());
    }

    #[test]
    fn invalidate_clears_cache() {
        let mut pipeline = make_pipeline(0);
        pipeline.set_pending(Frame::new(vec![lit_point(0.0)]));
        let _ = pipeline.produce_fifo_chunk(4, 30_000, true);
        assert!(pipeline.cached_slice().is_some());
        pipeline.invalidate();
        assert!(pipeline.cached_slice().is_none());
    }

    #[test]
    fn arm_startup_blank_decrements_across_calls() {
        // Use idle_policy::Blank and an empty filter; arm 6 startup-blank points
        // and produce two 4-point chunks. The first must come back fully blanked
        // (lit input → zeroed colors), the second only the first 2 points blanked.
        let frame: Vec<LaserPoint> = (0..8).map(|i| lit_point(i as f32 * 0.1)).collect();
        let mut pipeline = make_pipeline(0);
        pipeline.set_pending(Frame::new(frame));
        pipeline.arm_startup_blank(6);

        let chunk1: Vec<LaserPoint> = pipeline.produce_fifo_chunk(4, 30_000, true).to_vec();
        assert_eq!(chunk1.len(), 4);
        for p in &chunk1 {
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }

        let chunk2: Vec<LaserPoint> = pipeline.produce_fifo_chunk(4, 30_000, true).to_vec();
        assert_eq!(chunk2.len(), 4);
        // First 2 points consume the remaining startup-blank budget.
        for p in &chunk2[..2] {
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }
        // Remaining points retain their lit colors.
        for p in &chunk2[2..] {
            assert!(p.intensity > 0);
        }
    }

    #[test]
    fn reset_engine_invalidates_cache() {
        let mut pipeline = make_pipeline(0);
        pipeline.set_pending(Frame::new(vec![lit_point(0.0)]));
        let _ = pipeline.produce_fifo_chunk(4, 30_000, true);
        assert!(pipeline.cached_slice().is_some());
        pipeline.reset_engine();
        assert!(pipeline.cached_slice().is_none());
    }

    /// Output filter that records reset reasons.
    struct RecordingFilter {
        resets: Arc<Mutex<Vec<OutputResetReason>>>,
    }
    impl OutputFilter for RecordingFilter {
        fn reset(&mut self, reason: OutputResetReason) {
            self.resets.lock().unwrap().push(reason);
        }
        fn filter(&mut self, _points: &mut [LaserPoint], _ctx: &OutputFilterContext) {}
    }

    #[test]
    fn reset_output_filter_is_wired() {
        let resets = Arc::new(Mutex::new(Vec::new()));
        let filter = Box::new(RecordingFilter {
            resets: Arc::clone(&resets),
        });
        let mut pipeline = SlicePipeline::new(make_engine(), 0, Some(filter), IdlePolicy::Blank, 0);
        pipeline.reset_output_filter(OutputResetReason::SessionStart);
        pipeline.reset_output_filter(OutputResetReason::Reconnect);
        assert_eq!(
            *resets.lock().unwrap(),
            vec![
                OutputResetReason::SessionStart,
                OutputResetReason::Reconnect
            ]
        );
    }
}
