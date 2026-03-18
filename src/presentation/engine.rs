//! PresentationEngine and ColorDelayLine — core frame lifecycle internals.

use crate::types::LaserPoint;

use super::{Frame, TransitionFn, TransitionPlan};

// =============================================================================
// PresentationEngine
// =============================================================================

/// Core frame lifecycle manager.
///
/// Manages the current and pending frames, cursor position, and transition
/// point insertion. Provides two delivery modes:
///
/// - [`fill_chunk`](Self::fill_chunk): FIFO delivery for queue-based DACs.
///   Traverses the drawable, inserting transition points at frame boundaries.
/// - [`compose_hardware_frame`](Self::compose_hardware_frame): Frame-swap
///   delivery. Returns a complete composed frame with transition points.
pub(crate) struct PresentationEngine {
    /// The currently playing frame.
    pub(crate) current_base: Option<Frame>,
    /// The next frame to promote (latest-wins).
    pub(crate) pending_base: Option<Frame>,
    /// Current frame's points (no transition — just the raw frame).
    drawable: Vec<LaserPoint>,
    /// Whether the drawable needs to be rebuilt from current_base.
    drawable_dirty: bool,
    /// Current read cursor within `drawable`.
    cursor: usize,
    /// Transition function for generating blanking between frames.
    transition_fn: TransitionFn,
    /// Buffer for transition points injected between frames.
    transition_buf: Vec<LaserPoint>,
    /// Read cursor within transition_buf.
    transition_cursor: usize,
    /// Length of the transition prefix in the last composed frame-swap drawable.
    frame_swap_transition_len: usize,
    /// Maximum hardware frame capacity (frame-swap only). When set, composed
    /// frames are clamped by truncating the transition prefix.
    frame_capacity: Option<usize>,
}

impl PresentationEngine {
    /// Create a new engine with the given transition function.
    pub fn new(transition_fn: TransitionFn) -> Self {
        Self {
            current_base: None,
            pending_base: None,
            drawable: Vec::new(),
            drawable_dirty: true,
            cursor: 0,
            transition_fn,
            transition_buf: Vec::new(),
            transition_cursor: 0,
            frame_swap_transition_len: 0,
            frame_capacity: None,
        }
    }

    /// Set the maximum hardware frame capacity for frame-swap clamping.
    pub fn set_frame_capacity(&mut self, cap: Option<usize>) {
        self.frame_capacity = cap;
    }

    /// Reset all runtime state. Preserves the transition_fn and frame_capacity.
    pub fn reset(&mut self) {
        self.current_base = None;
        self.pending_base = None;
        self.drawable.clear();
        self.drawable_dirty = true;
        self.cursor = 0;
        self.transition_buf.clear();
        self.transition_cursor = 0;
        self.frame_swap_transition_len = 0;
    }

    /// Submit a new frame. Latest-wins: multiple calls before consumption
    /// keep only the most recent frame.
    ///
    /// If no current frame exists, the pending is immediately promoted.
    pub fn set_pending(&mut self, frame: Frame) {
        if self.current_base.is_none() {
            self.current_base = Some(frame);
            self.drawable_dirty = true;
            self.cursor = 0;
        } else {
            self.pending_base = Some(frame);
            // Don't mark dirty yet — we compose on promotion
        }
    }

    /// FIFO delivery: fill `buffer[..max_points]` from the current frame.
    ///
    /// Traverses the drawable cyclically. The drawable includes self-loop
    /// transition points as a suffix, so every seam (including A→A) runs
    /// through the transition function.
    ///
    /// When a pending frame is promoted, transition points are injected
    /// between the old frame's last point and the new frame's first point.
    ///
    /// Returns the number of points written (always `max_points` if a
    /// frame is available, 0 if no frame has been submitted).
    pub fn fill_chunk(&mut self, buffer: &mut [LaserPoint], max_points: usize) -> usize {
        let max_points = max_points.min(buffer.len());

        // Before first frame: output blanked at origin
        if self.current_base.is_none() {
            for p in &mut buffer[..max_points] {
                *p = LaserPoint::blanked(0.0, 0.0);
            }
            return max_points;
        }

        // Rebuild drawable if dirty
        if self.drawable_dirty {
            self.refresh_drawable();
        }

        if self.drawable.is_empty() {
            for p in &mut buffer[..max_points] {
                *p = LaserPoint::blanked(0.0, 0.0);
            }
            return max_points;
        }

        let mut written = 0;
        while written < max_points {
            // Drain any pending transition points first
            if self.transition_cursor < self.transition_buf.len() {
                buffer[written] = self.transition_buf[self.transition_cursor];
                self.transition_cursor += 1;
                written += 1;
                continue;
            }

            // Output current frame point
            buffer[written] = self.drawable[self.cursor];
            written += 1;
            self.cursor += 1;

            // Check if we've completed the drawable
            if self.cursor >= self.drawable.len() {
                if let Some(pending) = self.pending_base.take() {
                    // Frame change: compute transition from current base's
                    // real last point to the new frame's first point.
                    let outgoing_last = self
                        .current_base
                        .as_ref()
                        .and_then(|f| f.last_point())
                        .copied();
                    let new_frame = pending;
                    let mut coalesce = false;

                    if let (Some(last), Some(first)) =
                        (outgoing_last.as_ref(), new_frame.first_point())
                    {
                        match (self.transition_fn)(last, first) {
                            TransitionPlan::Transition(points) => {
                                self.transition_buf = points;
                                self.transition_cursor = 0;
                            }
                            TransitionPlan::Coalesce => {
                                self.transition_buf.clear();
                                coalesce = true;
                            }
                        }
                    }

                    self.current_base = Some(new_frame);
                    self.refresh_drawable();

                    if coalesce {
                        // Skip the incoming first point (it's the same
                        // logical point as the outgoing last). Bound by
                        // drawable length in case self-loop also coalesced.
                        self.cursor = if self.drawable.len() > 1 { 1 } else { 0 };
                    } else {
                        self.cursor = 0;
                    }
                } else {
                    // Self-loop: the drawable already contains the seam
                    // (transition suffix or coalesced base), just wrap.
                    self.cursor = 0;
                }
            }
        }

        written
    }

    /// Frame-swap delivery: compose and return a complete hardware frame.
    ///
    /// On frame change (A→B): computes `transition_fn(A.last, B.first)`,
    /// composes `[transition | B_points]`, then promotes B to current.
    /// The next call without a pending frame will recompute the self-loop.
    ///
    /// On self-loop (no pending): computes `transition_fn(A.last, A.first)`,
    /// composes `[transition | A_points]` (or coalesced).
    pub fn compose_hardware_frame(&mut self) -> &[LaserPoint] {
        if let Some(pending) = self.pending_base.take() {
            // Frame change: A→B transition.
            // Compute transition from current (A) to pending (B) BEFORE promoting.
            let plan = match &self.current_base {
                Some(current) => match (current.last_point(), pending.first_point()) {
                    (Some(last), Some(first)) => (self.transition_fn)(last, first),
                    _ => TransitionPlan::Transition(vec![]),
                },
                None => TransitionPlan::Transition(vec![]),
            };

            self.drawable.clear();
            match plan {
                TransitionPlan::Transition(transition) => {
                    self.frame_swap_transition_len = transition.len();
                    self.drawable.extend_from_slice(&transition);
                    self.drawable.extend_from_slice(pending.points());
                }
                TransitionPlan::Coalesce => {
                    self.frame_swap_transition_len = 0;
                    self.drawable.extend_from_slice(pending.points());
                }
            }

            // Empty frame submitted: send a blanked point to clear the display
            if self.drawable.is_empty() {
                self.drawable.push(LaserPoint::blanked(0.0, 0.0));
            }

            self.clamp_to_capacity();

            // Promote B to current. Mark dirty so next call builds self-loop.
            self.current_base = Some(pending);
            self.drawable_dirty = true;

            return &self.drawable;
        }

        // No pending: self-loop for current frame.
        if self.drawable_dirty {
            self.refresh_drawable_for_frame_swap();
        }

        &self.drawable
    }

    /// Rebuild drawable for frame-swap: includes self-loop transition.
    ///
    /// Frame-swap DACs send the entire drawable as one atomic frame, so the
    /// transition from last→first point is included for clean looping.
    /// For nearly-closed shapes (circles), `Coalesce` omits the last base
    /// point so the frame loops seamlessly without a duplicate seam point.
    fn refresh_drawable_for_frame_swap(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            self.frame_swap_transition_len = 0;
            return;
        };

        if current.is_empty() {
            self.frame_swap_transition_len = 0;
            self.drawable.push(LaserPoint::blanked(0.0, 0.0));
            return;
        }

        let points = current.points();
        self.frame_swap_transition_len =
            build_self_loop_drawable(&self.transition_fn, points, &mut self.drawable, true);
        self.clamp_to_capacity();
    }

    /// Truncate the transition prefix so the drawable fits within frame_capacity.
    /// Never removes authored frame points — only the leading transition.
    fn clamp_to_capacity(&mut self) {
        if let Some(cap) = self.frame_capacity {
            if self.drawable.len() > cap {
                let excess = self.drawable.len() - cap;
                let trim = excess.min(self.frame_swap_transition_len);
                self.drawable.drain(..trim);
                self.frame_swap_transition_len -= trim;
            }
        }
    }

    /// Rebuild the FIFO drawable from the current base frame.
    ///
    /// The drawable is `[base (coalesce-adjusted) | transition_suffix]`,
    /// so self-loops traverse transition points on every cycle.
    fn refresh_drawable(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            return;
        };

        if current.is_empty() {
            return;
        }

        let points = current.points();
        build_self_loop_drawable(&self.transition_fn, points, &mut self.drawable, false);
    }
}

/// Build a seam-adjusted drawable for a self-loop, applying the transition
/// function to the seam between the frame's last and first points.
///
/// For `Transition(points)`: places transition as prefix (frame-swap) or
/// suffix (FIFO) depending on `prefix_mode`.
///
/// For `Coalesce`: omits the last base point so the loop represents the
/// seam point once. Single-point frames are kept unchanged.
///
/// Returns the number of transition points in the drawable.
fn build_self_loop_drawable(
    transition_fn: &TransitionFn,
    base: &[LaserPoint],
    drawable: &mut Vec<LaserPoint>,
    prefix_mode: bool,
) -> usize {
    let last = base.last().unwrap();
    let first = base.first().unwrap();
    let plan = transition_fn(last, first);

    match plan {
        TransitionPlan::Transition(pts) => {
            let transition_len = pts.len();
            if prefix_mode {
                drawable.extend_from_slice(&pts);
                drawable.extend_from_slice(base);
            } else {
                drawable.extend_from_slice(base);
                drawable.extend_from_slice(&pts);
            }
            transition_len
        }
        TransitionPlan::Coalesce => {
            if base.len() > 1 {
                // Omit the last base point — on wrap, cursor returns to
                // first which is the same logical point.
                drawable.extend_from_slice(&base[..base.len() - 1]);
            } else {
                drawable.extend_from_slice(base);
            }
            0
        }
    }
}

// =============================================================================
// ColorDelayLine
// =============================================================================

/// Stateful color delay that carries across chunk boundaries.
///
/// For FIFO DACs, color delay is applied per-chunk. Without carry-over state,
/// the first `delay` points of every chunk get blanked, causing periodic
/// micro-brightness drops at chunk boundaries. This struct maintains a ring
/// buffer of the last `delay` color values so they carry into the next chunk.
pub(crate) struct ColorDelayLine {
    delay: usize,
    /// Ring buffer of the last `delay` colors from the previous chunk.
    carry: Vec<(u16, u16, u16, u16)>,
    /// Pre-allocated buffer for current chunk colors (avoids per-chunk allocation).
    scratch: Vec<(u16, u16, u16, u16)>,
}

impl ColorDelayLine {
    pub fn new(delay: usize) -> Self {
        Self {
            delay,
            carry: vec![(0, 0, 0, 0); delay],
            scratch: Vec::new(),
        }
    }

    /// Reset the carry buffer (e.g., after reconnect).
    pub fn reset(&mut self) {
        self.carry.fill((0, 0, 0, 0));
    }

    /// Apply color delay to a chunk, using carried state from the previous chunk.
    pub fn apply(&mut self, points: &mut [LaserPoint]) {
        if self.delay == 0 || points.is_empty() {
            return;
        }

        // Collect current colors into pre-allocated scratch buffer
        self.scratch.clear();
        self.scratch
            .extend(points.iter().map(|p| (p.r, p.g, p.b, p.intensity)));

        // Apply delay: for the first `delay` points, use the carry buffer;
        // for the rest, use colors from earlier in this chunk.
        for (i, point) in points.iter_mut().enumerate() {
            let (r, g, b, intensity) = if i < self.delay {
                // Use carried colors from previous chunk
                let carry_idx = self.carry.len() - self.delay + i;
                self.carry[carry_idx]
            } else {
                self.scratch[i - self.delay]
            };
            point.r = r;
            point.g = g;
            point.b = b;
            point.intensity = intensity;
        }

        // Update carry buffer: keep the last `delay` colors from this chunk
        let n = self.scratch.len();
        if n >= self.delay {
            self.carry.clear();
            self.carry
                .extend_from_slice(&self.scratch[n - self.delay..]);
        } else {
            // Chunk smaller than delay: shift carry and append
            self.carry.drain(..n);
            self.carry.extend_from_slice(&self.scratch);
            debug_assert_eq!(self.carry.len(), self.delay);
        }
    }
}
