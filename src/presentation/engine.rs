//! PresentationEngine and ColorDelayLine — core frame lifecycle internals.

use crate::point::LaserPoint;

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
#[cfg_attr(feature = "testutils", doc(hidden))]
#[cfg_attr(not(feature = "testutils"), allow(dead_code))]
pub struct PresentationEngine {
    /// The currently playing frame.
    pub(crate) current_base: Option<Frame>,
    /// The next frame to promote (latest-wins).
    pub(crate) pending_base: Option<Frame>,
    /// Working buffer: base-only frame points (FIFO) or composed frame (FrameSwap).
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
    /// True when transition_buf holds self-loop points (can be discarded if
    /// a pending frame arrives before they are fully drained).
    transition_is_self_loop: bool,
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
            transition_is_self_loop: false,
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
        self.transition_is_self_loop = false;
        self.frame_swap_transition_len = 0;
    }

    /// Returns true once a logical frame has been submitted to the engine.
    pub fn has_logical_frame(&self) -> bool {
        self.current_base.is_some()
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
    /// Traverses the base frame points cyclically. At each seam (cursor
    /// wrap), the transition function is called dynamically against the
    /// latest pending frame (if any) or the current frame (self-loop).
    /// This ensures the seam always reflects the most recent state — no
    /// stale self-loop transition is emitted before a real frame change.
    ///
    /// Returns the number of points written (always `max_points` if a
    /// frame is available, 0 if no frame has been submitted).
    pub fn fill_chunk(&mut self, buffer: &mut [LaserPoint], max_points: usize) -> usize {
        let max_points = max_points.min(buffer.len());

        // Rebuild drawable if dirty (no-op when current_base is None)
        if self.drawable_dirty {
            self.refresh_drawable();
        }

        // No frame yet or empty frame: output blanked at origin
        if self.current_base.is_none() || self.drawable.is_empty() {
            buffer[..max_points].fill(LaserPoint::blanked(0.0, 0.0));
            return max_points;
        }

        let mut written = 0;
        while written < max_points {
            // Drain pending transition points — but if they are stale
            // self-loop points and a pending frame has arrived, discard
            // them and promote immediately.
            if self.transition_cursor < self.transition_buf.len() {
                if self.transition_is_self_loop && self.pending_base.is_some() {
                    // Stale self-loop transition: discard and promote now
                    self.transition_buf.clear();
                    self.transition_cursor = 0;
                    self.transition_is_self_loop = false;
                    self.promote_pending();

                    if self.drawable.is_empty() {
                        buffer[written..max_points].fill(LaserPoint::blanked(0.0, 0.0));
                        return max_points;
                    }
                    continue;
                }

                // Batch-drain as many transition points as fit in the output.
                let src = &self.transition_buf[self.transition_cursor..];
                let n = src.len().min(max_points - written);
                buffer[written..written + n].copy_from_slice(&src[..n]);
                written += n;
                self.transition_cursor += n;
                continue;
            }

            // Batch-copy drawable points until the next seam or buffer full.
            let src = &self.drawable[self.cursor..];
            let n = src.len().min(max_points - written);
            buffer[written..written + n].copy_from_slice(&src[..n]);
            written += n;
            self.cursor += n;

            // At the seam: compute transition dynamically against pending or self
            if self.cursor >= self.drawable.len() {
                if self.pending_base.is_some() {
                    self.promote_pending();

                    if self.drawable.is_empty() {
                        buffer[written..max_points].fill(LaserPoint::blanked(0.0, 0.0));
                        return max_points;
                    }
                } else {
                    // Self-loop: compute seam dynamically using drawable first/last.
                    // drawable is non-empty here (checked above) and is always a
                    // direct copy of current_base.points(), so these unwraps are safe.
                    let last = self.drawable.last().unwrap();
                    let first = self.drawable.first().unwrap();
                    match (self.transition_fn)(last, first) {
                        TransitionPlan::Transition(points) => {
                            self.transition_buf = points;
                            self.transition_cursor = 0;
                            self.transition_is_self_loop = true;
                            self.cursor = 0;
                        }
                        TransitionPlan::Coalesce => {
                            self.cursor = if self.drawable.len() > 1 { 1 } else { 0 };
                        }
                    }
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
            let plan = match (
                self.current_base.as_ref().and_then(|c| c.last_point()),
                pending.first_point(),
            ) {
                (Some(last), Some(first)) => (self.transition_fn)(last, first),
                _ => TransitionPlan::Transition(vec![]),
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
                    // A.last ≈ B.first — skip B's first point to avoid a
                    // duplicate logical seam sample in the hardware frame.
                    let pts = pending.points();
                    self.drawable
                        .extend_from_slice(if pts.len() > 1 { &pts[1..] } else { pts });
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
        self.frame_swap_transition_len = 0;

        let Some(current) = &self.current_base else {
            return;
        };

        if current.is_empty() {
            self.drawable.push(LaserPoint::blanked(0.0, 0.0));
            return;
        }

        let points = current.points();
        self.frame_swap_transition_len =
            build_self_loop_drawable(&self.transition_fn, points, &mut self.drawable);
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

    /// Promote pending frame to current for FIFO delivery.
    ///
    /// Computes the A→B transition, promotes the pending frame, rebuilds
    /// the drawable, and sets the cursor appropriately.
    fn promote_pending(&mut self) {
        let pending = self.pending_base.take().unwrap();

        let plan = match (
            self.current_base.as_ref().and_then(|f| f.last_point()),
            pending.first_point(),
        ) {
            (Some(last), Some(first)) => (self.transition_fn)(last, first),
            _ => TransitionPlan::Transition(vec![]),
        };

        self.current_base = Some(pending);
        self.refresh_drawable();

        match plan {
            TransitionPlan::Transition(points) => {
                self.transition_buf = points;
                self.transition_cursor = 0;
                self.transition_is_self_loop = false;
                self.cursor = 0;
            }
            TransitionPlan::Coalesce => {
                self.cursor = if self.drawable.len() > 1 { 1 } else { 0 };
            }
        }
    }

    /// Rebuild the FIFO drawable from the current base frame.
    ///
    /// Contains only the base frame points. Transition points are computed
    /// dynamically at seam time in `fill_chunk`.
    fn refresh_drawable(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            return;
        };

        if current.is_empty() {
            return;
        }

        self.drawable.extend_from_slice(current.points());
    }
}

/// Build a seam-adjusted drawable for a self-loop, applying the transition
/// function to the seam between the frame's last and first points.
///
/// For `Transition(points)`: places the transition as a prefix before the
/// base frame points (used by frame-swap delivery).
///
/// For `Coalesce`: omits the last base point so the loop represents the
/// seam point once. Single-point frames are kept unchanged.
///
/// Returns the number of transition points in the drawable.
fn build_self_loop_drawable(
    transition_fn: &TransitionFn,
    base: &[LaserPoint],
    drawable: &mut Vec<LaserPoint>,
) -> usize {
    let last = base.last().unwrap();
    let first = base.first().unwrap();
    let plan = transition_fn(last, first);

    match plan {
        TransitionPlan::Transition(pts) => {
            let transition_len = pts.len();
            drawable.extend_from_slice(&pts);
            drawable.extend_from_slice(base);
            transition_len
        }
        TransitionPlan::Coalesce => {
            // Omit the last base point for len > 1 — on wrap, cursor returns to
            // first which is the same logical point.
            let end = if base.len() > 1 {
                base.len() - 1
            } else {
                base.len()
            };
            drawable.extend_from_slice(&base[..end]);
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

    /// Current delay in points.
    #[allow(dead_code)]
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// Reset the carry buffer (e.g., after reconnect).
    pub fn reset(&mut self) {
        self.carry.fill((0, 0, 0, 0));
    }

    /// Resize the delay line to a new point count.
    ///
    /// - **Grow**: pads the front of the carry buffer with black (oldest slots).
    /// - **Shrink**: keeps the most recent entries (trims from the front).
    /// - **Equal**: no-op.
    pub fn resize(&mut self, new_delay: usize) {
        if new_delay == self.delay {
            return;
        }
        if new_delay == 0 {
            self.delay = 0;
            self.carry.clear();
            return;
        }
        if new_delay > self.delay {
            // Grow: prepend black entries, keep existing carry at the end
            let extra = new_delay - self.delay;
            let mut new_carry = vec![(0, 0, 0, 0); extra];
            new_carry.extend_from_slice(&self.carry);
            self.carry = new_carry;
        } else {
            // Shrink: keep only the most recent (tail) entries
            self.carry.drain(..self.delay - new_delay);
        }
        self.delay = new_delay;
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

        // Apply delay: for the first `delay` points use carry; for the rest, scratch.
        for (i, point) in points.iter_mut().enumerate() {
            (point.r, point.g, point.b, point.intensity) = if i < self.delay {
                self.carry[i]
            } else {
                self.scratch[i - self.delay]
            };
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
