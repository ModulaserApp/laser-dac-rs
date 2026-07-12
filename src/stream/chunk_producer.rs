//! ChunkProducer — `FifoContentSource` that wraps a user fill callback.
//!
//! Owns the working buffer, RepeatLast cache, color-delay line, startup-blank
//! countdown, idle policy, and end-of-stream flag. The streaming-mode analogue
//! of [`SlicePipeline`](crate::presentation::slice_pipeline::SlicePipeline).

use std::collections::VecDeque;
use std::time::Duration;

use crate::config::IdlePolicy;
use crate::device::DacInfo;
use crate::error::Error;
use crate::point::LaserPoint;
use crate::presentation::content_source::FifoContentSource;
use crate::stream::{ChunkRequest, ChunkResult, StreamControl, StreamInstant, StreamStats};

type FillFn = Box<dyn FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static>;

/// Wraps a user-supplied chunk-fill callback as a `FifoContentSource`.
pub(crate) struct ChunkProducer {
    producer: FillFn,
    control: StreamControl,
    idle_policy: IdlePolicy,
    startup_blank: Duration,

    /// Working buffer for the producer to fill. Resized on demand.
    buf: Vec<LaserPoint>,
    /// Logical length of the cached slice in `buf`.
    cached_len: usize,
    /// True when `buf[..cached_len]` is a previously-produced slice that
    /// has not yet been written.
    cached: bool,

    /// Most recent committed RAW frame (pre-blanking, pre-color-delay), kept
    /// for `IdlePolicy::RepeatLast`. Storing raw content — rather than the
    /// already-delayed output — lets a starved repeat flow back through the
    /// continuous color-delay line exactly once, keeping colour aligned to the
    /// repeated XY positions (mirrors `ColorDelayLine`/`SlicePipeline`).
    last_chunk: Vec<LaserPoint>,
    last_chunk_len: usize,

    /// Raw (pre-blanking, pre-color-delay) snapshot of the chunk currently
    /// produced-but-not-yet-committed. Promoted into `last_chunk` on commit so
    /// only chunks actually written to the device become the repeat source.
    pending_raw: Vec<LaserPoint>,
    pending_raw_len: usize,

    color_delay_line: VecDeque<(u16, u16, u16, u16)>,
    startup_blank_remaining: usize,

    current_instant: StreamInstant,
    stats: StreamStats,
    ended: bool,
    /// Set when end-of-stream was triggered by `IdlePolicy::Stop` rather
    /// than a producer-emitted `ChunkResult::End`. Drained by the driver
    /// so the public `run()` returns `Err(Error::Stopped)`.
    stop_error: Option<Error>,
}

impl ChunkProducer {
    pub fn new<F>(
        producer: F,
        control: StreamControl,
        idle_policy: IdlePolicy,
        startup_blank: Duration,
        initial_capacity: usize,
    ) -> Self
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
    {
        Self {
            producer: Box::new(producer),
            control,
            idle_policy,
            startup_blank,
            buf: vec![LaserPoint::default(); initial_capacity],
            cached_len: 0,
            cached: false,
            last_chunk: vec![LaserPoint::default(); initial_capacity],
            last_chunk_len: 0,
            pending_raw: vec![LaserPoint::default(); initial_capacity],
            pending_raw_len: 0,
            color_delay_line: VecDeque::new(),
            startup_blank_remaining: 0,
            current_instant: StreamInstant::new(0),
            stats: StreamStats::default(),
            ended: false,
            stop_error: None,
        }
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> &StreamStats {
        &self.stats
    }

    /// Begin a startup-blank window of `points` points (called by the driver
    /// on arm transition).
    pub fn arm_startup_blank(&mut self, pps: u32) {
        self.startup_blank_remaining = duration_to_points(self.startup_blank, pps);
    }

    /// Disarm transition: clear the color delay line so stale colors don't
    /// leak past a re-arm.
    pub fn on_disarm(&mut self) {
        self.color_delay_line.clear();
    }

    fn invalidate(&mut self) {
        self.cached = false;
        self.cached_len = 0;
    }

    fn ensure_buf(&mut self, n: usize) {
        if self.buf.len() < n {
            self.buf.resize(n, LaserPoint::default());
        }
        if self.last_chunk.len() < n {
            self.last_chunk.resize(n, LaserPoint::default());
        }
        if self.pending_raw.len() < n {
            self.pending_raw.resize(n, LaserPoint::default());
        }
    }

    fn build_request(&self, target_points: usize, pps: u32) -> ChunkRequest {
        ChunkRequest {
            start: self.current_instant,
            pps,
            target_points,
        }
    }

    /// Apply disarm blanking, startup blanking, and color delay in place.
    fn apply_blanking_and_color_delay(
        &mut self,
        n: usize,
        pps: u32,
        is_armed: bool,
        delay_micros: u64,
    ) {
        if !is_armed {
            let park = match &self.idle_policy {
                IdlePolicy::Park { x, y } => LaserPoint::blanked(*x, *y),
                _ => LaserPoint::blanked(0.0, 0.0),
            };
            self.buf[..n].fill(park);
        } else if self.startup_blank_remaining > 0 {
            let blank_count = n.min(self.startup_blank_remaining);
            for p in &mut self.buf[..blank_count] {
                p.r = 0;
                p.g = 0;
                p.b = 0;
                p.intensity = 0;
            }
            self.startup_blank_remaining -= blank_count;
        }

        let color_delay_points = duration_micros_to_points(delay_micros, pps);
        if color_delay_points > 0 {
            self.color_delay_line
                .resize(color_delay_points, (0, 0, 0, 0));
            for p in &mut self.buf[..n] {
                self.color_delay_line
                    .push_back((p.r, p.g, p.b, p.intensity));
                let (r, g, b, i) = self.color_delay_line.pop_front().unwrap();
                p.r = r;
                p.g = g;
                p.b = b;
                p.intensity = i;
            }
        } else if !self.color_delay_line.is_empty() {
            self.color_delay_line.clear();
        }
    }

    /// Fill a range with the appropriate idle-policy fallback. Returns
    /// `false` if `IdlePolicy::Stop` triggered.
    fn fill_idle_range(&mut self, range: std::ops::Range<usize>, is_armed: bool) -> bool {
        if is_armed {
            match &self.idle_policy {
                IdlePolicy::Stop => {
                    self.ended = true;
                    self.stop_error = Some(Error::Stopped);
                    return false;
                }
                IdlePolicy::RepeatLast if self.last_chunk_len > 0 => {
                    for (offset, i) in range.enumerate() {
                        self.buf[i] = self.last_chunk[offset % self.last_chunk_len];
                    }
                }
                IdlePolicy::Park { x, y } => {
                    self.buf[range].fill(LaserPoint::blanked(*x, *y));
                }
                _ => {
                    self.buf[range].fill(LaserPoint::blanked(0.0, 0.0));
                }
            }
        } else {
            let park = match &self.idle_policy {
                IdlePolicy::Park { x, y } => LaserPoint::blanked(*x, *y),
                _ => LaserPoint::blanked(0.0, 0.0),
            };
            self.buf[range].fill(park);
        }
        true
    }

    fn fill_idle(&mut self, n: usize, is_armed: bool) -> bool {
        self.fill_idle_range(0..n, is_armed)
    }
}

impl FifoContentSource for ChunkProducer {
    fn produce_chunk(&mut self, target_points: usize, pps: u32, is_armed: bool) -> &[LaserPoint] {
        if self.ended {
            return &[];
        }
        if target_points == 0 {
            self.invalidate();
            return &[];
        }
        self.ensure_buf(target_points);

        let req = self.build_request(target_points, pps);
        let delay_micros = self.control.color_delay().as_micros() as u64;

        let result = match (self.producer)(&req, &mut self.buf[..target_points]) {
            ChunkResult::Filled(0) => ChunkResult::Starved,
            other => other,
        };

        let n = match result {
            ChunkResult::Filled(filled) => {
                let filled = filled.min(target_points);
                if filled == target_points {
                    filled
                } else if self.fill_idle_range(filled..target_points, is_armed) {
                    target_points
                } else if filled == 0 {
                    self.invalidate();
                    return &[];
                } else {
                    filled
                }
            }
            ChunkResult::Starved => {
                self.stats.underrun_count += 1;
                if !self.fill_idle(target_points, is_armed) {
                    self.invalidate();
                    return &[];
                }
                target_points
            }
            ChunkResult::End => {
                self.ended = true;
                self.invalidate();
                return &[];
            }
        };

        // Snapshot the RAW content (post-idle-fill, pre-blanking, pre-delay)
        // before the color-delay line mutates `buf`. On commit this becomes the
        // RepeatLast source, so a starved repeat replays raw points through the
        // continuous delay line exactly once instead of re-delaying output.
        self.pending_raw[..n].copy_from_slice(&self.buf[..n]);
        self.pending_raw_len = n;

        self.apply_blanking_and_color_delay(n, pps, is_armed, delay_micros);

        self.cached_len = n;
        self.cached = true;
        &self.buf[..n]
    }

    fn cached_slice(&self) -> Option<&[LaserPoint]> {
        if self.cached {
            Some(&self.buf[..self.cached_len])
        } else {
            None
        }
    }

    fn commit_written(&mut self, n: usize, is_armed: bool) {
        if is_armed && n > 0 {
            // Promote the RAW snapshot (not the delayed output in `buf`) so
            // RepeatLast replays raw frames through the color-delay line once.
            let copy = n.min(self.pending_raw_len);
            debug_assert!(copy <= self.last_chunk.len());
            self.last_chunk[..copy].copy_from_slice(&self.pending_raw[..copy]);
            self.last_chunk_len = copy;
        }
        self.current_instant += n as u64;
        self.stats.chunks_written += 1;
        self.stats.points_written += n as u64;
        self.invalidate();
    }

    fn discard_cached(&mut self) {
        self.invalidate();
    }

    fn reserve_buf(&mut self, n: usize) {
        self.ensure_buf(n);
    }

    fn on_reconnect(&mut self, info: &DacInfo) {
        let max = info.caps.max_points_per_chunk;
        self.ensure_buf(max);
        self.last_chunk_len = 0;
        self.pending_raw_len = 0;
        self.color_delay_line.clear();
        self.startup_blank_remaining = 0;
        self.invalidate();
        self.stats.reconnect_count += 1;
    }

    fn is_ended(&self) -> bool {
        self.ended
    }

    fn arm_startup_blank(&mut self, pps: u32) {
        ChunkProducer::arm_startup_blank(self, pps);
    }

    fn on_disarm(&mut self) {
        ChunkProducer::on_disarm(self);
    }

    fn take_stop_error(&mut self) -> Option<Error> {
        self.stop_error.take()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    fn lit_point(x: f32) -> LaserPoint {
        LaserPoint::new(x, 0.0, 1000, 1000, 1000, 1000)
    }

    fn make_producer<F>(producer: F, idle: IdlePolicy) -> (ChunkProducer, StreamControl)
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
    {
        let (tx, _rx) = mpsc::channel();
        let control = StreamControl::new(tx, Duration::ZERO, 30_000);
        let cp = ChunkProducer::new(producer, control.clone(), idle, Duration::ZERO, 16);
        (cp, control)
    }

    /// Build a producer with an explicit color-delay (seeded on the control's
    /// atomic) and startup-blank window. Capacity is large enough for the
    /// deterministic single-chunk tests below.
    fn make_producer_cfg<F>(
        producer: F,
        color_delay: Duration,
        startup_blank: Duration,
        pps: u32,
    ) -> (ChunkProducer, StreamControl)
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
    {
        let (tx, _rx) = mpsc::channel();
        let control = StreamControl::new(tx, color_delay, pps);
        let cp = ChunkProducer::new(
            producer,
            control.clone(),
            IdlePolicy::Blank,
            startup_blank,
            32,
        );
        (cp, control)
    }

    /// Fill `buf` with `n` distinct, lit points at a fixed non-zero position so
    /// tests can prove x/y are preserved while colour is shifted or blanked.
    fn fill_distinct_lit(buf: &mut [LaserPoint], n: usize) {
        for (i, p) in buf.iter_mut().take(n).enumerate() {
            let c = (i as u16 + 1) * 1000;
            *p = LaserPoint::new(0.5, -0.25, c, c / 2, c / 4, 60000);
        }
    }

    #[test]
    fn color_delay_blanks_leading_points_and_shifts_color() {
        // pps=10_000, color_delay=300µs → ceil(0.0003 * 10_000) = 3 points.
        let n = 8;
        let (mut cp, _ctrl) = make_producer_cfg(
            move |_req, buf| {
                fill_distinct_lit(buf, n);
                ChunkResult::Filled(n)
            },
            Duration::from_micros(300),
            Duration::ZERO,
            10_000,
        );

        let slice = cp.produce_chunk(n, 10_000, true).to_vec();
        assert_eq!(slice.len(), n);

        // Leading 3 points: colour/intensity blanked, but x/y PRESERVED — the
        // invariant the legacy deque-state tests never checked.
        for (i, p) in slice.iter().take(3).enumerate() {
            assert_eq!(
                (p.r, p.g, p.b, p.intensity),
                (0, 0, 0, 0),
                "point {i} colour must be blanked by the delay line"
            );
            assert_eq!(
                p.x, 0.5,
                "point {i} x must be preserved through color delay"
            );
            assert_eq!(
                p.y, -0.25,
                "point {i} y must be preserved through color delay"
            );
        }

        // Point 3 carries the colour shifted in from input point 0.
        assert_eq!(
            (slice[3].r, slice[3].g, slice[3].b, slice[3].intensity),
            (1000, 500, 250, 60000),
            "point 3 must carry input point 0's colour, delayed by 3"
        );
    }

    #[test]
    fn startup_blank_blanks_first_n_returned_points() {
        // pps=10_000, startup_blank=500µs → ceil(0.0005 * 10_000) = 5 points.
        let n = 10;
        let (mut cp, _ctrl) = make_producer_cfg(
            move |_req, buf| {
                fill_distinct_lit(buf, n);
                ChunkResult::Filled(n)
            },
            Duration::ZERO,
            Duration::from_micros(500),
            10_000,
        );

        // Arm the startup-blank window (the driver does this on the arm edge).
        cp.arm_startup_blank(10_000);

        let slice = cp.produce_chunk(n, 10_000, true).to_vec();
        assert_eq!(slice.len(), n);

        for (i, p) in slice.iter().take(5).enumerate() {
            assert_eq!(
                (p.r, p.g, p.b, p.intensity),
                (0, 0, 0, 0),
                "point {i} must be fully blanked during startup window"
            );
        }
        for (i, p) in slice.iter().enumerate().skip(5) {
            assert!(
                p.intensity > 0,
                "point {i} must be lit after startup window"
            );
        }
    }

    #[test]
    fn filled_then_committed_advances_state() {
        let (mut cp, _ctrl) = make_producer(
            |_req, buf| {
                for p in buf.iter_mut() {
                    *p = lit_point(0.0);
                }
                ChunkResult::Filled(buf.len())
            },
            IdlePolicy::Blank,
        );
        let slice = cp.produce_chunk(8, 30_000, true).to_vec();
        assert_eq!(slice.len(), 8);
        assert!(cp.cached_slice().is_some());
        cp.commit_written(8, true);
        assert!(cp.cached_slice().is_none());
        assert_eq!(cp.stats().points_written, 8);
        assert_eq!(cp.stats().chunks_written, 1);
    }

    #[test]
    fn starved_armed_blank_idle_policy_fills_blanks() {
        let (mut cp, _ctrl) = make_producer(|_req, _buf| ChunkResult::Starved, IdlePolicy::Blank);
        let slice = cp.produce_chunk(8, 30_000, true).to_vec();
        assert_eq!(slice.len(), 8);
        for p in &slice {
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }
        assert_eq!(cp.stats().underrun_count, 1);
    }

    #[test]
    fn starved_repeats_last_chunk_when_armed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let (mut cp, _ctrl) = make_producer(
            move |_req, buf| {
                let n = calls_c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    for p in buf.iter_mut() {
                        *p = lit_point(0.5);
                    }
                    ChunkResult::Filled(buf.len())
                } else {
                    ChunkResult::Starved
                }
            },
            IdlePolicy::RepeatLast,
        );
        let _ = cp.produce_chunk(4, 30_000, true);
        cp.commit_written(4, true);
        let slice = cp.produce_chunk(4, 30_000, true).to_vec();
        assert_eq!(slice.len(), 4);
        for p in &slice {
            assert!(p.intensity > 0, "RepeatLast should reuse last lit chunk");
        }
    }

    #[test]
    fn partial_fill_pads_remainder_with_idle_policy() {
        let (mut cp, _ctrl) = make_producer(
            |_req, buf| {
                buf[0] = lit_point(0.25);
                ChunkResult::Filled(1)
            },
            IdlePolicy::Park { x: 0.5, y: -0.5 },
        );

        let slice = cp.produce_chunk(4, 30_000, true).to_vec();

        assert_eq!(slice.len(), 4);
        assert_eq!(slice[0].x, 0.25);
        for p in &slice[1..] {
            assert_eq!(
                (p.x, p.y, p.r, p.g, p.b, p.intensity),
                (0.5, -0.5, 0, 0, 0, 0)
            );
        }
    }

    #[test]
    fn end_marks_ended_and_returns_empty() {
        let (mut cp, _ctrl) = make_producer(|_req, _buf| ChunkResult::End, IdlePolicy::Blank);
        let slice = cp.produce_chunk(4, 30_000, true);
        assert!(slice.is_empty());
        assert!(cp.is_ended());
    }

    #[test]
    fn idle_policy_stop_marks_ended_with_stop_error() {
        let (mut cp, _ctrl) = make_producer(|_req, _buf| ChunkResult::Starved, IdlePolicy::Stop);
        let _ = cp.produce_chunk(4, 30_000, true);
        assert!(cp.is_ended());
        assert!(<ChunkProducer as FifoContentSource>::take_stop_error(&mut cp).is_some());
    }

    #[test]
    fn disarmed_forces_blanks_through_idle_policy() {
        let (mut cp, _ctrl) = make_producer(
            |_req, buf| {
                for p in buf.iter_mut() {
                    *p = lit_point(0.5);
                }
                ChunkResult::Filled(buf.len())
            },
            IdlePolicy::Blank,
        );
        let slice = cp.produce_chunk(8, 30_000, false).to_vec();
        assert_eq!(slice.len(), 8);
        for p in &slice {
            assert_eq!((p.r, p.g, p.b, p.intensity), (0, 0, 0, 0));
        }
    }

    /// Build a producer with an explicit color delay AND a chosen idle policy.
    fn make_producer_delay_policy<F>(
        producer: F,
        color_delay: Duration,
        pps: u32,
        idle: IdlePolicy,
    ) -> (ChunkProducer, StreamControl)
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
    {
        let (tx, _rx) = mpsc::channel();
        let control = StreamControl::new(tx, color_delay, pps);
        let cp = ChunkProducer::new(producer, control.clone(), idle, Duration::ZERO, 32);
        (cp, control)
    }

    /// Regression pin for the RepeatLast × color-delay interaction.
    ///
    /// The color-delay line is a CONTINUOUS carry-over shift register (see
    /// `ColorDelayLine` in `presentation::engine` and `SlicePipeline`): every
    /// chunk of RAW content flows through it exactly once, and the tail colours
    /// carry into the next chunk. When the producer starves under `RepeatLast`
    /// we replay the last RAW frame back through the line — NOT the already
    /// delayed output — so colour stays continuously aligned to the repeated
    /// XY positions and the frame is fully drawn during sustained starvation.
    ///
    /// pps=10_000, color_delay=200µs → 2 delay points.
    ///
    /// Chunk 1 raw colours `[1000,2000,3000,4000]` → output `[0,0,1000,2000]`
    /// (leading blanks shift in), delay line left holding `[3000,4000]`.
    ///
    /// Chunk 2 STARVES → replay raw `[1000,2000,3000,4000]` through the line
    /// (carry `[3000,4000]`) → output `[3000,4000,1000,2000]` (steady state).
    ///
    /// The pre-fix code stored POST-delay output in `last_chunk` and re-delayed
    /// it, emitting `[3000,4000,0,0]` and then oscillating on further repeats —
    /// colour double-shifted, frame corrupted. This test pins the corrected
    /// contract so it can never silently drift again.
    #[test]
    fn repeat_last_replays_raw_frame_through_color_delay_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let (mut cp, _ctrl) = make_producer_delay_policy(
            move |_req, buf| {
                let n = calls_c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    for (i, p) in buf.iter_mut().take(4).enumerate() {
                        let c = (i as u16 + 1) * 1000;
                        *p = LaserPoint::new(0.5, -0.25, c, c, c, c);
                    }
                    ChunkResult::Filled(4)
                } else {
                    ChunkResult::Starved
                }
            },
            Duration::from_micros(200),
            10_000,
            IdlePolicy::RepeatLast,
        );

        // Chunk 1: raw [1000,2000,3000,4000] delayed by 2 → [0,0,1000,2000].
        let chunk1 = cp.produce_chunk(4, 10_000, true).to_vec();
        let colors1: Vec<u16> = chunk1.iter().map(|p| p.r).collect();
        assert_eq!(
            colors1,
            vec![0, 0, 1000, 2000],
            "chunk 1 must be the raw frame delayed by 2 points"
        );
        cp.commit_written(4, true);

        // Chunk 2 STARVES → replay the RAW frame through the continuous line.
        let chunk2 = cp.produce_chunk(4, 10_000, true).to_vec();
        let colors2: Vec<u16> = chunk2.iter().map(|p| p.r).collect();
        assert_eq!(
            colors2,
            vec![3000, 4000, 1000, 2000],
            "RepeatLast must replay the RAW frame through the color-delay line \
             exactly once (continuous), NOT re-delay already-delayed output"
        );
        // XY of the repeated frame is preserved verbatim.
        for p in &chunk2 {
            assert_eq!((p.x, p.y), (0.5, -0.25));
        }
        assert_eq!(cp.stats().underrun_count, 1);

        // A SECOND sustained-starvation repeat stays at the correct steady
        // state — proving the line does not oscillate/degrade (the old bug).
        cp.commit_written(4, true);
        let chunk3 = cp.produce_chunk(4, 10_000, true).to_vec();
        let colors3: Vec<u16> = chunk3.iter().map(|p| p.r).collect();
        assert_eq!(
            colors3,
            vec![3000, 4000, 1000, 2000],
            "sustained RepeatLast must hold the correct steady-state frame"
        );
    }

    /// Fill every point with the same lit colour `c` at a fixed position.
    fn fill_uniform_lit(buf: &mut [LaserPoint], n: usize, c: u16) {
        for p in buf.iter_mut().take(n) {
            *p = LaserPoint::new(0.5, -0.25, c, c, c, c);
        }
    }

    /// Finding #4a — dynamic `set_color_delay` mid-stream takes effect on the
    /// next produced chunk: the delay line resizes and the freshly-introduced
    /// leading points are blanked; setting it back to zero clears the line.
    #[test]
    fn set_color_delay_mid_stream_resizes_and_clears_line() {
        let (mut cp, ctrl) = make_producer_cfg(
            |_req, buf| {
                fill_uniform_lit(buf, buf.len(), 5000);
                ChunkResult::Filled(buf.len())
            },
            Duration::ZERO,
            Duration::ZERO,
            10_000,
        );

        // Chunk 1 with delay=0: no shift, everything lit.
        let c1 = cp.produce_chunk(4, 10_000, true).to_vec();
        assert!(c1.iter().all(|p| p.r == 5000), "delay=0 → no colour shift");
        cp.commit_written(4, true);

        // Grow the delay to 200µs (2 points) mid-stream.
        ctrl.set_color_delay(Duration::from_micros(200));
        let c2 = cp.produce_chunk(4, 10_000, true).to_vec();
        assert_eq!(
            c2.iter().map(|p| p.r).collect::<Vec<_>>(),
            vec![0, 0, 5000, 5000],
            "new delay must resize the line and blank the 2 leading points"
        );
        cp.commit_written(4, true);

        // Shrink back to zero: the line clears, output is fully lit again.
        ctrl.set_color_delay(Duration::ZERO);
        let c3 = cp.produce_chunk(4, 10_000, true).to_vec();
        assert!(
            c3.iter().all(|p| p.r == 5000),
            "delay reset to 0 must clear the line → no leading blanks"
        );
    }

    /// Finding #4b — `on_disarm` clears the color-delay carry so stale tail
    /// colours don't leak past a re-arm: the line is re-primed from empty and
    /// the leading points re-blank on the next armed chunk.
    #[test]
    fn color_delay_resets_on_disarm_arm() {
        let (mut cp, _ctrl) = make_producer_cfg(
            |_req, buf| {
                fill_uniform_lit(buf, buf.len(), 5000);
                ChunkResult::Filled(buf.len())
            },
            Duration::from_micros(200), // 2 delay points at 10k pps
            Duration::ZERO,
            10_000,
        );

        // Chunk 1: leading 2 blanked, line left carrying [5000,5000].
        let c1 = cp.produce_chunk(4, 10_000, true).to_vec();
        assert_eq!(
            c1.iter().map(|p| p.r).collect::<Vec<_>>(),
            vec![0, 0, 5000, 5000]
        );
        cp.commit_written(4, true);

        // Without a disarm, the carry would fully light the next chunk.
        // Disarm clears the line, so the leading points re-blank instead.
        <ChunkProducer as FifoContentSource>::on_disarm(&mut cp);
        let c2 = cp.produce_chunk(4, 10_000, true).to_vec();
        assert_eq!(
            c2.iter().map(|p| p.r).collect::<Vec<_>>(),
            vec![0, 0, 5000, 5000],
            "disarm must clear the delay line so the re-armed chunk re-blanks"
        );
    }

    /// Finding #4c — the startup-blank window re-primes on every arm edge:
    /// after consuming it once, a disarm + fresh `arm_startup_blank` re-blanks
    /// the first N points on the second arm.
    #[test]
    fn startup_blank_resets_on_rearm() {
        // 500µs @ 10k pps → 5 startup-blank points.
        let (mut cp, _ctrl) = make_producer_cfg(
            |_req, buf| {
                fill_uniform_lit(buf, buf.len(), 5000);
                ChunkResult::Filled(buf.len())
            },
            Duration::ZERO,
            Duration::from_micros(500),
            10_000,
        );

        // First arm: consume the whole 5-point startup window.
        cp.arm_startup_blank(10_000);
        let first = cp.produce_chunk(10, 10_000, true).to_vec();
        for (i, p) in first.iter().take(5).enumerate() {
            assert_eq!(p.intensity, 0, "first-arm startup point {i} must blank");
        }
        assert!(first[5].intensity > 0, "startup window is only 5 points");
        cp.commit_written(10, true);

        // A produce WITHOUT re-arming stays lit — the window is spent.
        let spent = cp.produce_chunk(10, 10_000, true).to_vec();
        assert!(
            spent.iter().all(|p| p.intensity > 0),
            "startup window must not re-blank without a fresh arm edge"
        );
        cp.commit_written(10, true);

        // Disarm then arm again: the window re-primes and re-blanks 5 points.
        <ChunkProducer as FifoContentSource>::on_disarm(&mut cp);
        cp.arm_startup_blank(10_000);
        let second = cp.produce_chunk(10, 10_000, true).to_vec();
        for (i, p) in second.iter().take(5).enumerate() {
            assert_eq!(p.intensity, 0, "second-arm startup point {i} must re-blank");
        }
        assert!(second[5].intensity > 0, "startup window is only 5 points");
    }

    #[test]
    fn build_request_threads_target_points_and_pps() {
        use std::sync::Mutex;
        let last_req: Arc<Mutex<Option<ChunkRequest>>> = Arc::new(Mutex::new(None));
        let last_req_c = Arc::clone(&last_req);
        let (mut cp, _ctrl) = make_producer(
            move |req, _buf| {
                *last_req_c.lock().unwrap() = Some(req.clone());
                ChunkResult::Filled(0)
            },
            IdlePolicy::Blank,
        );
        let _ = cp.produce_chunk(8, 30_000, true);
        let req = last_req.lock().unwrap().clone().unwrap();
        assert_eq!(req.target_points, 8);
        assert_eq!(req.pps, 30_000);
    }
}
