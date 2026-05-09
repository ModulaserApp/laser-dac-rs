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

    /// Most recent fully-formed chunk, kept for `IdlePolicy::RepeatLast`.
    last_chunk: Vec<LaserPoint>,
    last_chunk_len: usize,

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

    /// Fill the buffer with the appropriate idle-policy fallback for `n`
    /// points. Returns `false` if `IdlePolicy::Stop` triggered (the producer
    /// has marked itself ended); the caller must bail.
    fn fill_idle(&mut self, n: usize, is_armed: bool) -> bool {
        if is_armed {
            match &self.idle_policy {
                IdlePolicy::Stop => {
                    self.ended = true;
                    self.stop_error = Some(Error::Stopped);
                    return false;
                }
                IdlePolicy::RepeatLast if self.last_chunk_len > 0 => {
                    for i in 0..n {
                        self.buf[i] = self.last_chunk[i % self.last_chunk_len];
                    }
                }
                IdlePolicy::Park { x, y } => {
                    self.buf[..n].fill(LaserPoint::blanked(*x, *y));
                }
                _ => {
                    self.buf[..n].fill(LaserPoint::blanked(0.0, 0.0));
                }
            }
        } else {
            let park = match &self.idle_policy {
                IdlePolicy::Park { x, y } => LaserPoint::blanked(*x, *y),
                _ => LaserPoint::blanked(0.0, 0.0),
            };
            self.buf[..n].fill(park);
        }
        true
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
            ChunkResult::Filled(n) => n.min(target_points),
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
            debug_assert!(n <= self.last_chunk.len());
            self.last_chunk[..n].copy_from_slice(&self.buf[..n]);
            self.last_chunk_len = n;
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
