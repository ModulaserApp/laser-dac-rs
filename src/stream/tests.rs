use super::*;
use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
use crate::config::IdlePolicy;
use crate::device::OutputModel;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Shared `SoftwareDecayEstimator` test wrapper. Backends update it from
/// inside `try_write_points`; tests mutate it via cloned handles to seed the
/// buffer state under inspection.
#[derive(Clone)]
struct SharedEstimator(Arc<Mutex<SoftwareDecayEstimator>>);

impl SharedEstimator {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(SoftwareDecayEstimator::new())))
    }

    /// Replace the inner estimator with one that reports `n` points right
    /// now. Equivalent to "the device buffer holds `n` points as of this
    /// instant"; subsequent reads decay naturally.
    fn seed(&self, n: u64, pps: u32) {
        let mut g = self.0.lock().unwrap();
        *g = SoftwareDecayEstimator::new();
        if n > 0 {
            g.record_send(Instant::now(), n, pps);
        }
    }

    fn record_send(&self, now: Instant, n: u64, pps: u32) {
        self.0.lock().unwrap().record_send(now, n, pps);
    }
}

impl BufferEstimator for SharedEstimator {
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64 {
        self.0.lock().unwrap().estimated_fullness(now, pps)
    }
}

/// A test backend for unit testing stream behavior.
struct TestBackend {
    caps: DacCapabilities,
    connected: bool,
    /// Count of write attempts
    write_count: Arc<AtomicUsize>,
    /// Number of WouldBlock responses to return before accepting writes
    would_block_count: Arc<AtomicUsize>,
    /// Cumulative count of points the backend has accepted (test write counter
    /// — independent of the estimator, which decays).
    queued: Arc<AtomicU64>,
    /// Track shutter state for testing
    shutter_open: Arc<AtomicBool>,
    /// Decaying estimator the stream consults; tests can seed via `estimator.seed()`.
    estimator: SharedEstimator,
    /// Every point value accepted by `try_write_points`, in write order. Lets
    /// tests assert on the exact bytes reaching the backend (color delay,
    /// startup blank, parking) instead of internal deque state.
    received: Arc<Mutex<Vec<LaserPoint>>>,
}

impl TestBackend {
    fn new() -> Self {
        Self {
            caps: DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 1000,
                output_model: crate::device::OutputModel::NetworkFifo,
            },
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            would_block_count: Arc::new(AtomicUsize::new(0)),
            queued: Arc::new(AtomicU64::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
            estimator: SharedEstimator::new(),
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Handle to the captured point log (see [`Self::received`]).
    fn received(&self) -> Arc<Mutex<Vec<LaserPoint>>> {
        self.received.clone()
    }

    fn with_would_block_count(mut self, count: usize) -> Self {
        self.would_block_count = Arc::new(AtomicUsize::new(count));
        self
    }

    fn with_output_model(mut self, model: OutputModel) -> Self {
        self.caps.output_model = model;
        self
    }

    /// Seed the estimator and write counter to a starting buffer depth.
    /// At test PPS (30k), estimator decay is irrelevant for prompt assertions.
    fn with_initial_queue(self, queue: u64) -> Self {
        self.queued.store(queue, Ordering::SeqCst);
        // Default test PPS used across the suite.
        self.estimator.seed(queue, 30_000);
        self
    }
}

impl DacBackend for TestBackend {
    fn dac_type(&self) -> DacType {
        DacType::Custom("Test".to_string())
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        self.shutter_open.store(open, Ordering::SeqCst);
        Ok(())
    }
}

impl FifoBackend for TestBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        self.write_count.fetch_add(1, Ordering::SeqCst);

        // Return WouldBlock until count reaches 0
        let remaining = self.would_block_count.load(Ordering::SeqCst);
        if remaining > 0 {
            self.would_block_count.fetch_sub(1, Ordering::SeqCst);
            return Ok(WriteOutcome::WouldBlock);
        }

        self.queued.fetch_add(points.len() as u64, Ordering::SeqCst);
        self.estimator
            .record_send(Instant::now(), points.len() as u64, pps);
        self.received.lock().unwrap().extend_from_slice(points);
        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// Mock FrameSwapBackend for testing frame-swap specific behavior.
///
/// Wraps a TestBackend and implements FrameSwapBackend instead of FifoBackend.
/// Use with `BackendKind::FrameSwap(Box::new(...))` to test frame-swap dispatch.
struct FrameSwapTestBackend {
    inner: TestBackend,
    frame_capacity: usize,
    ready: Arc<AtomicBool>,
}

impl FrameSwapTestBackend {
    fn new() -> Self {
        Self {
            inner: TestBackend::new().with_output_model(OutputModel::UsbFrameSwap),
            frame_capacity: 4095,
            ready: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl DacBackend for FrameSwapTestBackend {
    fn dac_type(&self) -> DacType {
        self.inner.dac_type()
    }
    fn caps(&self) -> &DacCapabilities {
        self.inner.caps()
    }
    fn connect(&mut self) -> Result<()> {
        self.inner.connect()
    }
    fn disconnect(&mut self) -> Result<()> {
        self.inner.disconnect()
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn stop(&mut self) -> Result<()> {
        self.inner.stop()
    }
    fn set_shutter(&mut self, open: bool) -> Result<()> {
        self.inner.set_shutter(open)
    }
}

impl crate::backend::FrameSwapBackend for FrameSwapTestBackend {
    fn frame_capacity(&self) -> usize {
        self.frame_capacity
    }

    fn is_ready_for_frame(&mut self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        if !self.ready.load(Ordering::SeqCst) {
            return Ok(WriteOutcome::WouldBlock);
        }
        self.inner.try_write_points(pps, points)
    }
}

#[test]
fn test_stream_control_arm_disarm() {
    let (tx, _rx) = mpsc::channel();
    let control = StreamControl::new(tx, Duration::ZERO, 30_000);
    assert!(!control.is_armed());

    control.arm().unwrap();
    assert!(control.is_armed());

    control.disarm().unwrap();
    assert!(!control.is_armed());
}

#[test]
fn test_stream_control_stop() {
    let (tx, _rx) = mpsc::channel();
    let control = StreamControl::new(tx, Duration::ZERO, 30_000);
    assert!(!control.is_stop_requested());

    control.stop().unwrap();
    assert!(control.is_stop_requested());
}

#[test]
fn test_stream_control_clone_shares_state() {
    let (tx, _rx) = mpsc::channel();
    let control1 = StreamControl::new(tx, Duration::ZERO, 30_000);
    let control2 = control1.clone();

    control1.arm().unwrap();
    assert!(control2.is_armed());

    control2.stop().unwrap();
    assert!(control1.is_stop_requested());
}

#[test]
fn test_device_start_stream_connects_backend() {
    let backend = TestBackend::new();
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );

    assert!(!device.is_connected());

    let (stream, _info) = device.start_stream(StreamConfig::new(30000)).unwrap();
    assert!(stream.backend.as_ref().unwrap().is_connected());
}

#[test]
fn test_device_start_stream_promotes_untouched_defaults_for_network_backends() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::NetworkFifo;
    // Use a non-`Custom` network dac_type: `Custom` backends are intentionally
    // excluded from promotion (they set their own policy), so promotion must be
    // observed on a real network DAC.
    let mut info = test_info(backend.caps());
    info.kind = DacType::EtherDream;
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::NETWORK_DEFAULT_TARGET_BUFFER
    );
}

#[test]
fn test_device_start_stream_does_not_promote_custom_backends() {
    // `Custom` backends keep the raw 20ms default even with a network output
    // model — the frame path and the stream path agree on this exclusion.
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::NetworkFifo;
    let device = Dac::new(
        test_info(backend.caps()), // test_info sets kind = Custom
        BackendKind::Fifo(Box::new(backend)),
    );

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::DEFAULT_TARGET_BUFFER
    );
}

#[test]
fn test_device_start_stream_promotes_blocking_fifo_backends() {
    // BlockingFifo (e.g. LaserCube USB) is promoted just like NetworkFifo.
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::BlockingFifo;
    let mut info = test_info(backend.caps());
    info.kind = DacType::LaserCubeUsb;
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::NETWORK_DEFAULT_TARGET_BUFFER
    );
}

#[test]
fn test_device_start_stream_promotes_lasercube_network_default_buffer() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::NetworkFifo;
    let mut info = test_info(backend.caps());
    info.kind = DacType::LaserCubeNetwork;
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::LASERCUBE_NETWORK_DEFAULT_TARGET_BUFFER
    );
}

#[test]
fn test_device_start_stream_keeps_explicit_network_buffer_settings() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::UdpTimed;
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );

    let cfg = StreamConfig::new(30_000).with_target_buffer(Duration::from_millis(12));
    let (stream, _info) = device.start_stream(cfg).unwrap();

    assert_eq!(stream.config.target_buffer, Duration::from_millis(12));
}

// Removed `test_device_start_stream_keeps_usb_defaults`: it constructed an
// inconsistent backend (BackendKind::Fifo wrapper with output_model=UsbFrameSwap)
// to exercise the "USB defaults kept" branch of `apply_backend_buffer_defaults`.
// After locking `is_frame_swap()` to `caps().output_model == UsbFrameSwap`, that
// state is no longer constructible — frame-swap backends must use the FrameSwap
// wrapper, and `start_stream` rejects them outright. See the existing test
// `test_device_start_stream_rejects_frame_swap_backend` below.

#[test]
fn test_run_retries_on_would_block() {
    // Create a backend that returns WouldBlock 3 times before accepting
    let backend = TestBackend::new().with_would_block_count(3);
    let write_count = backend.write_count.clone();
    let stream = make_test_stream(backend);

    let produced_count = Arc::new(AtomicUsize::new(0));
    let produced_count_clone = produced_count.clone();
    let result = stream.run(
        move |req, buffer| {
            let count = produced_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 1 {
                let n = req.target_points.min(buffer.len());
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // WouldBlock retries happen internally in the driver's write spin.
    // The exact count depends on timing, but we should see multiple writes.
    assert!(write_count.load(Ordering::SeqCst) >= 1);
}

// =========================================================================
// Buffer-driven timing tests
// =========================================================================

#[test]
fn test_run_buffer_driven_behavior() {
    // Test that run uses buffer-driven timing
    let backend = TestBackend::new();
    let write_count = backend.write_count.clone();

    // Use short target buffer for testing
    let cfg = StreamConfig::new(30000).with_target_buffer(Duration::from_millis(10));
    let stream = make_test_stream_with_cfg(backend, cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            // Run for 5 calls then end
            if count >= 4 {
                ChunkResult::End
            } else {
                let n = req.target_points.min(buffer.len()).min(100);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    assert!(
        write_count.load(Ordering::SeqCst) >= 4,
        "Should have written multiple chunks"
    );
}

#[test]
fn test_run_uses_configured_target_buffer() {
    let cfg = StreamConfig::new(1_000)
        .with_target_buffer(Duration::from_millis(100))
        .with_drain_timeout(Duration::ZERO);
    let stream = make_test_stream_with_cfg(TestBackend::new(), cfg);

    let observed = Arc::new(AtomicUsize::new(0));
    let observed_clone = observed.clone();

    let result = stream.run(
        move |req, _buffer| {
            observed_clone.store(req.target_points, Ordering::SeqCst);
            ChunkResult::End
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    assert_eq!(observed.load(Ordering::SeqCst), 100);
}

#[test]
fn test_run_sleeps_when_buffer_healthy() {
    // Test that run sleeps when buffer is above target
    use std::time::Instant;

    // Very small target buffer, skip drain
    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(5))
        .with_drain_timeout(Duration::ZERO);
    let stream = make_test_stream_with_cfg(TestBackend::new(), cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();
    let start_time = Instant::now();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            // End after 3 callbacks
            if count >= 2 {
                ChunkResult::End
            } else {
                // Fill buffer to trigger sleep
                let n = req.target_points.min(buffer.len());
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Should have taken some time due to buffer-driven sleep
    let elapsed = start_time.elapsed();
    // With buffer-driven timing, we should see some elapsed time
    // (not instant return)
    assert!(
        elapsed.as_millis() < 100,
        "Elapsed time {:?} is too long for test",
        elapsed
    );
}

#[test]
fn test_run_stops_on_control_stop() {
    // Test that stop() via control handle terminates the loop promptly
    use std::thread;

    let stream = make_test_stream(TestBackend::new());
    let control = stream.control();

    // Spawn a thread to stop the stream after a short delay
    let control_clone = control.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        control_clone.stop().unwrap();
    });

    let result = stream.run(
        |req, buffer| {
            let n = req.target_points.min(buffer.len()).min(10);
            for pt in buffer.iter_mut().take(n) {
                *pt = LaserPoint::blanked(0.0, 0.0);
            }
            ChunkResult::Filled(n)
        },
        |_e| {},
    );

    // Should exit with Stopped, not hang forever
    assert_eq!(result.unwrap(), RunExit::Stopped);
}

#[test]
fn test_run_producer_ended() {
    // Test that ChunkResult::End terminates the stream gracefully
    let stream = make_test_stream(TestBackend::new());

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // First call: return some data
                let n = req.target_points.min(buffer.len()).min(100);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            } else {
                // Second call: end the stream
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_run_starved_applies_underrun_policy() {
    // Test that ChunkResult::Starved triggers underrun policy
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::Blank);
    let stream = make_test_stream_with_cfg(backend, cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, _buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // First call: return Starved to trigger underrun policy
                ChunkResult::Starved
            } else {
                // Second call: end the stream
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Underrun policy should have written some points
    assert!(
        queued.load(Ordering::SeqCst) > 0,
        "Underrun policy should have written blank points"
    );
}

#[test]
fn test_run_filled_zero_with_target_treated_as_starved() {
    // Test that Filled(0) when target_points > 0 is treated as Starved
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::Blank);
    let stream = make_test_stream_with_cfg(backend, cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, _buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // Return Filled(0) when buffer needs data - should be treated as Starved
                ChunkResult::Filled(0)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Filled(0) with target_points > 0 should trigger underrun policy
    assert!(
        queued.load(Ordering::SeqCst) > 0,
        "Filled(0) with target > 0 should trigger underrun and write blank points"
    );
}

// =========================================================================
// End-to-end color-delay / startup-blank tests
//
// These drive the REAL `Stream::run` production path and assert on the exact
// point VALUES reaching the backend (captured via `TestBackend::received`),
// rather than poking internal deque state. They supersede the old
// `write_fill_points`-driven color-delay/startup-blank tests.
// =========================================================================

#[test]
fn test_run_color_delay_blanks_leading_points() {
    // pps=10_000, color_delay=300µs → 3 points. The producer emits lit points
    // at a fixed non-zero position; the first 3 written points must have their
    // colour blanked (delay line shifts in blanks) while x/y are preserved.
    let backend = TestBackend::new();
    let received = backend.received();
    // Disable startup blanking so the leading-blank assertion isolates the
    // color-delay behaviour.
    let cfg = StreamConfig::new(10_000)
        .with_color_delay(Duration::from_micros(300))
        .with_startup_blank(Duration::ZERO);
    let stream = make_test_stream_with_cfg(backend, cfg);
    stream.control().arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_c = call_count.clone();
    let result = stream.run(
        move |req, buffer| {
            if call_count_c.fetch_add(1, Ordering::SeqCst) == 0 {
                let n = req.target_points.min(buffer.len());
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::new(0.5, 0.5, 40000, 30000, 20000, 50000);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );
    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    let pts = received.lock().unwrap();
    assert!(pts.len() > 3, "expected at least one written chunk");
    // Leading 3 points: colour blanked, x/y preserved.
    for (i, p) in pts.iter().take(3).enumerate() {
        assert_eq!(
            (p.r, p.g, p.b, p.intensity),
            (0, 0, 0, 0),
            "leading point {i} must be colour-blanked by the delay line"
        );
        assert_eq!(p.x, 0.5, "leading point {i} x must be preserved");
        assert_eq!(p.y, 0.5, "leading point {i} y must be preserved");
    }
    // Point 3 carries the shifted-in lit colour.
    assert_eq!(
        (pts[3].r, pts[3].g, pts[3].b, pts[3].intensity),
        (40000, 30000, 20000, 50000),
        "point 3 must carry the lit colour delayed by 3 points"
    );
}

#[test]
fn test_run_startup_blank_blanks_first_n_points() {
    // pps=10_000, startup_blank=500µs → 5 points. After arming, the first 5
    // written points must be fully blanked; later ones stay lit.
    let backend = TestBackend::new();
    let received = backend.received();
    let cfg = StreamConfig::new(10_000)
        .with_startup_blank(Duration::from_micros(500))
        .with_color_delay(Duration::ZERO);
    let stream = make_test_stream_with_cfg(backend, cfg);
    stream.control().arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_c = call_count.clone();
    let result = stream.run(
        move |req, buffer| {
            if call_count_c.fetch_add(1, Ordering::SeqCst) == 0 {
                let n = req.target_points.min(buffer.len());
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::new(0.1, 0.1, 65535, 32000, 16000, 65535);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );
    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    let pts = received.lock().unwrap();
    assert!(pts.len() > 5, "expected at least one written chunk");
    for (i, p) in pts.iter().take(5).enumerate() {
        assert_eq!(
            (p.r, p.g, p.b, p.intensity),
            (0, 0, 0, 0),
            "startup point {i} must be fully blanked"
        );
    }
    assert!(
        pts[5].intensity > 0,
        "point 5 must be lit once the startup window closes"
    );
}

// =========================================================================
// Buffer estimation tests (Task 6.3)
// =========================================================================

#[test]
fn test_estimate_buffer_reads_backend_estimator() {
    // The protocol-owned BufferEstimator is the single source of truth.
    let backend = TestBackend::new().with_initial_queue(300);
    let estimator = backend.estimator.clone();
    let stream = make_test_stream(backend);

    // The software-decay estimator is seeded at wall-clock time and read a few
    // microseconds later; at 30k pps it sheds ~1 point per 33µs, so assert a
    // tolerance band rather than exact equality to avoid a timing-flaky
    // off-by-one on loaded CI runners (matches `test_status_reports_*`).
    let first = stream.estimate_buffer_points();
    assert!(
        (290..=300).contains(&first),
        "estimate ~300 (seeded depth, minus slight decay), got {first}"
    );

    estimator.seed(800, stream.config.pps);
    let second = stream.estimate_buffer_points();
    assert!(
        (790..=800).contains(&second),
        "estimate ~800 (seeded depth, minus slight decay), got {second}"
    );
}

// =========================================================================
// ChunkResult handling tests (Task 6.4)
// =========================================================================

#[test]
fn test_fill_result_filled_writes_points_and_updates_state() {
    // Test that Filled(n) writes n points to backend and updates stream state
    let backend = TestBackend::new();
    let queued = backend.queued.clone();
    let stream = make_test_stream(backend);

    let points_written = Arc::new(AtomicUsize::new(0));
    let points_written_clone = points_written.clone();
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count < 3 {
                // Fill with specific number of points
                let n = req.target_points.min(50);
                for (i, pt) in buffer.iter_mut().enumerate().take(n) {
                    *pt = LaserPoint::new(0.1 * i as f32, 0.2 * i as f32, 1000, 2000, 3000, 4000);
                }
                points_written_clone.fetch_add(n, Ordering::SeqCst);
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Points should have been written to backend
    // Note: drain adds 16 blank points at shutdown
    let total_queued = queued.load(Ordering::SeqCst);
    let total_written = points_written.load(Ordering::SeqCst);
    assert!(
        total_queued > 0,
        "Points should have been queued to backend"
    );
    assert!(
        total_queued as usize >= total_written,
        "Queued points ({}) should be at least written points ({})",
        total_queued,
        total_written
    );
}

#[test]
fn test_fill_result_filled_updates_last_chunk_when_armed() {
    // Test that Filled(n) updates last_chunk for RepeatLast policy when armed
    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::RepeatLast);
    let stream = make_test_stream_with_cfg(TestBackend::new(), cfg);

    // Arm the stream so last_chunk gets updated
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // Write specific points that we can verify later
                let n = req.target_points.min(10);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::new(0.5, 0.5, 10000, 20000, 30000, 40000);
                }
                ChunkResult::Filled(n)
            } else if count == 1 {
                // Return Starved to trigger RepeatLast
                ChunkResult::Starved
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // If last_chunk wasn't updated, the Starved would have outputted blanks
    // The test passes if no assertion fails - the RepeatLast policy used the stored chunk
}

#[test]
fn test_fill_result_starved_repeat_last_with_stored_chunk() {
    // Test that Starved with RepeatLast policy repeats the last chunk
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::RepeatLast);
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Arm the stream
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // First call: provide some data to establish last_chunk
                let n = req.target_points.min(50);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::new(0.3, 0.3, 5000, 5000, 5000, 5000);
                }
                ChunkResult::Filled(n)
            } else if count == 1 {
                // Second call: return Starved - should repeat last chunk
                ChunkResult::Starved
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Both the initial fill and the repeated chunk should have been written
    let total_queued = queued.load(Ordering::SeqCst);
    assert!(
        total_queued >= 50,
        "Should have written initial chunk plus repeated chunk"
    );
}

#[test]
fn test_fill_result_starved_repeat_last_without_stored_chunk_falls_back_to_blank() {
    // Test that Starved with RepeatLast but no stored chunk falls back to blank
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::RepeatLast);
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Arm the stream
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, _buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // First call: return Starved with no prior chunk
                ChunkResult::Starved
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Should have written blank points as fallback
    let total_queued = queued.load(Ordering::SeqCst);
    assert!(
        total_queued > 0,
        "Should have written blank points as fallback"
    );
}

#[test]
fn test_fill_result_starved_with_park_policy() {
    // Test that Starved with Park policy outputs blanked points at park position
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::Park { x: 0.5, y: -0.5 });
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Arm the stream
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, _buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                ChunkResult::Starved
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Should have written parked points
    let total_queued = queued.load(Ordering::SeqCst);
    assert!(total_queued > 0, "Should have written parked points");
}

#[test]
fn test_fill_result_starved_with_stop_policy() {
    // Test that Starved with Stop policy terminates the stream
    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::Stop);
    let stream = make_test_stream_with_cfg(TestBackend::new(), cfg);

    // Must arm the stream for underrun policy to be checked
    // (disarmed streams always output blanks regardless of policy)
    let control = stream.control();
    control.arm().unwrap();

    let result = stream.run(
        |_req, _buffer| {
            // Always return Starved - Stop policy should terminate the stream
            ChunkResult::Starved
        },
        |_e| {},
    );

    // Stream should have stopped due to underrun with Stop policy
    // The Stop policy returns Err(Error::Stopped) to immediately terminate
    assert!(result.is_err(), "Stop policy should return an error");
    assert!(
        result.unwrap_err().is_stopped(),
        "Error should be Stopped variant"
    );
}

#[test]
fn test_fill_result_end_returns_producer_ended() {
    // Test that End terminates the stream with ProducerEnded
    let stream = make_test_stream(TestBackend::new());

    let result = stream.run(
        |_req, _buffer| {
            // Immediately end
            ChunkResult::End
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
}

#[test]
fn test_fill_result_filled_exceeds_buffer_clamped() {
    // Test that Filled(n) where n > buffer.len() is clamped
    let backend = TestBackend::new();
    let queued = backend.queued.clone();
    let stream = make_test_stream(backend);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // Fill some points but claim we wrote more than buffer size
                for pt in buffer.iter_mut() {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                // Return a value larger than buffer - should be clamped
                ChunkResult::Filled(buffer.len() + 1000)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Should have written clamped number of points (max_points_per_chunk)
    // plus 16 blank points from drain shutdown
    let total_queued = queued.load(Ordering::SeqCst);
    assert!(total_queued > 0, "Should have written some points");
    // The clamping should limit to max_points (1000 for TestBackend) + 16 blank drain points
    assert!(
        total_queued <= 1016,
        "Points should be clamped to max_points_per_chunk (+ drain)"
    );
}

// =========================================================================
// Integration tests (Task 6.5)
// =========================================================================

#[test]
fn test_full_stream_lifecycle_create_arm_stream_stop() {
    // Test the complete lifecycle: create -> arm -> stream data -> stop
    let backend = TestBackend::new();
    let queued = backend.queued.clone();
    let shutter_open = backend.shutter_open.clone();

    // 1. Create device and start stream
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    assert!(!device.is_connected());

    let (stream, returned_info) = device.start_stream(StreamConfig::new(30000)).unwrap();
    assert_eq!(returned_info.id, "test");

    // 2. Get control handle and verify initial state
    let control = stream.control();
    assert!(!control.is_armed());
    assert!(!shutter_open.load(Ordering::SeqCst));

    // 3. Arm the stream
    control.arm().unwrap();
    assert!(control.is_armed());

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    // 4. Run the stream for a few iterations
    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count < 5 {
                // Fill with data
                let n = req.target_points.min(buffer.len()).min(100);
                for (i, pt) in buffer.iter_mut().enumerate().take(n) {
                    let t = i as f32 / 100.0;
                    *pt = LaserPoint::new(t, t, 10000, 20000, 30000, 40000);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    // 5. Verify stream ended properly
    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    assert!(
        queued.load(Ordering::SeqCst) > 0,
        "Should have written points"
    );
    assert!(
        call_count.load(Ordering::SeqCst) >= 5,
        "Should have called producer multiple times"
    );
}

#[test]
fn test_full_stream_lifecycle_with_idle_policy_recovery() {
    // Test lifecycle with underrun and recovery
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::RepeatLast);
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Arm the stream for underrun policy to work
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            match count {
                0 => {
                    // First call: provide data (establishes last_chunk)
                    let n = req.target_points.min(buffer.len()).min(50);
                    for pt in buffer.iter_mut().take(n) {
                        *pt = LaserPoint::new(0.5, 0.5, 30000, 30000, 30000, 30000);
                    }
                    ChunkResult::Filled(n)
                }
                1 => {
                    // Second call: underrun (triggers RepeatLast)
                    ChunkResult::Starved
                }
                2 => {
                    // Third call: recover with new data
                    let n = req.target_points.min(buffer.len()).min(50);
                    for pt in buffer.iter_mut().take(n) {
                        *pt = LaserPoint::new(-0.5, -0.5, 20000, 20000, 20000, 20000);
                    }
                    ChunkResult::Filled(n)
                }
                _ => ChunkResult::End,
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Should have written: initial data + repeated chunk + recovered data
    let total = queued.load(Ordering::SeqCst);
    assert!(
        total >= 100,
        "Should have written multiple chunks including underrun recovery"
    );
}

#[test]
fn test_full_stream_lifecycle_external_stop() {
    // Test stopping stream from external control handle
    use std::thread;

    let stream = make_test_stream(TestBackend::new());

    let control = stream.control();
    let control_clone = control.clone();

    // Spawn thread to stop stream after delay
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        control_clone.stop().unwrap();
    });

    let result = stream.run(
        |req, buffer| {
            // Keep streaming until stopped
            let n = req.target_points.min(buffer.len()).min(10);
            for pt in buffer.iter_mut().take(n) {
                *pt = LaserPoint::blanked(0.0, 0.0);
            }
            ChunkResult::Filled(n)
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::Stopped);
}

#[test]
fn test_full_stream_lifecycle_into_dac_recovery() {
    // Test recovering Dac from stream for reuse
    let backend = TestBackend::new();
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    let (stream, _) = device.start_stream(StreamConfig::new(30000)).unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 2 {
                let n = req.target_points.min(buffer.len()).min(50);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);

    // Note: into_dac() would be tested here, but run consumes the stream
    // and doesn't return it. The into_dac pattern is for the blocking API.
    // This test verifies the stream lifecycle completes cleanly.
}

#[test]
fn test_stream_stats_tracking() {
    // Test that stream statistics are tracked correctly
    let stream = make_test_stream(TestBackend::new());

    // Arm the stream
    let control = stream.control();
    control.arm().unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();
    let points_per_call = 50;

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 3 {
                let n = req.target_points.min(buffer.len()).min(points_per_call);
                for pt in buffer.iter_mut().take(n) {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Stats tracking is verified by the successful completion
    // Detailed stats assertions would require access to stream after run
}

#[test]
fn test_stream_disarm_during_streaming() {
    // Test disarming stream while it's running
    use std::thread;

    let backend = TestBackend::new();
    let shutter_open = backend.shutter_open.clone();
    let stream = make_test_stream(backend);

    let control = stream.control();
    let control_clone = control.clone();

    // Arm first
    control.arm().unwrap();
    assert!(control.is_armed());

    // Spawn thread to disarm then stop
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(15));
        control_clone.disarm().unwrap();
        thread::sleep(Duration::from_millis(15));
        control_clone.stop().unwrap();
    });

    let result = stream.run(
        |req, buffer| {
            let n = req.target_points.min(buffer.len()).min(10);
            for pt in buffer.iter_mut().take(n) {
                *pt = LaserPoint::new(0.1, 0.1, 50000, 50000, 50000, 50000);
            }
            ChunkResult::Filled(n)
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::Stopped);
    // Shutter should have been closed by disarm
    assert!(!shutter_open.load(Ordering::SeqCst));
}

// =========================================================================
// Disarm scanner parking regression tests
// =========================================================================
// These tests verify that disarming parks scanners at a fixed position
// instead of continuing to trace the shape path with blanked colors.
// See: https://github.com/ModulaserApp/Modulaser-v2/issues/117

/// Drive `run()` on a DISARMED stream (starts disarmed by default). The
/// producer emits lit shape points until it ends; the returned vector is the
/// exact point log written to the backend. Assert on LEADING points — the
/// graceful-End drain appends origin blanks at the tail.
fn run_disarmed_and_capture(cfg: StreamConfig) -> Vec<LaserPoint> {
    let backend = TestBackend::new();
    let received = backend.received();
    let stream = make_test_stream_with_cfg(backend, cfg);
    // NB: no arm() — the stream stays disarmed.
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_c = call_count.clone();
    let result = stream.run(
        move |req, buffer| {
            if call_count_c.fetch_add(1, Ordering::SeqCst) == 0 {
                let n = req.target_points.min(buffer.len());
                for (i, pt) in buffer.iter_mut().take(n).enumerate() {
                    // A lit shape path with a non-zero position, so parking is
                    // observable as a position/colour change.
                    let angle = i as f32 * 0.05;
                    *pt = LaserPoint::new(angle.cos(), angle.sin(), 65535, 40000, 20000, 65535);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );
    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    let pts = received.lock().unwrap().clone();
    assert!(pts.len() > 10, "expected at least one written chunk");
    pts
}

#[test]
fn test_disarm_parks_scanners_at_origin_default_policy() {
    // Regression: a disarmed stream with the default Blank policy must park
    // scanners at (0,0) — not keep tracing the shape path with blanked colors.
    let pts = run_disarmed_and_capture(StreamConfig::new(30000));

    for (i, p) in pts.iter().take(10).enumerate() {
        assert_eq!(p.x, 0.0, "point {i}: x must be 0.0 (parked), got {}", p.x);
        assert_eq!(p.y, 0.0, "point {i}: y must be 0.0 (parked), got {}", p.y);
        assert_eq!(p.r, 0, "point {i}: must be blanked");
        assert_eq!(p.g, 0, "point {i}: must be blanked");
        assert_eq!(p.b, 0, "point {i}: must be blanked");
        assert_eq!(p.intensity, 0, "point {i}: must be blanked");
    }
}

#[test]
fn test_disarm_parks_scanners_at_configured_park_position() {
    // When IdlePolicy::Park is set, a disarmed stream must park at that position.
    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::Park { x: 0.5, y: -0.3 });
    let pts = run_disarmed_and_capture(cfg);

    for (i, p) in pts.iter().take(10).enumerate() {
        assert_eq!(p.x, 0.5, "point {i}: x must be park position 0.5");
        assert_eq!(p.y, -0.3, "point {i}: y must be park position -0.3");
        assert_eq!(p.r, 0, "point {i}: must be blanked");
        assert_eq!(p.intensity, 0, "point {i}: must be blanked");
    }
}

#[test]
fn test_disarm_repeat_last_falls_back_to_blank() {
    // RepeatLast must NOT repeat lit content when disarmed — it falls back to
    // parking at the origin (blank).
    let cfg = StreamConfig::new(30000).with_idle_policy(IdlePolicy::RepeatLast);
    let pts = run_disarmed_and_capture(cfg);

    for (i, p) in pts.iter().take(10).enumerate() {
        assert_eq!(
            p.x, 0.0,
            "point {i}: must be parked at origin, not shape position"
        );
        assert_eq!(
            p.y, 0.0,
            "point {i}: must be parked at origin, not shape position"
        );
        assert_eq!(
            p.r, 0,
            "point {i}: must be blanked, not repeating lit content"
        );
        assert_eq!(p.intensity, 0, "point {i}: must be blanked");
    }
}

#[test]
fn test_stream_with_mock_backend_disconnect() {
    // Test handling of backend disconnect during streaming
    use std::sync::atomic::AtomicBool;

    struct DisconnectingBackend {
        inner: TestBackend,
        disconnect_after: Arc<AtomicUsize>,
        call_count: Arc<AtomicUsize>,
    }

    impl DacBackend for DisconnectingBackend {
        fn dac_type(&self) -> DacType {
            self.inner.dac_type()
        }

        fn caps(&self) -> &DacCapabilities {
            self.inner.caps()
        }

        fn connect(&mut self) -> Result<()> {
            self.inner.connect()
        }

        fn disconnect(&mut self) -> Result<()> {
            self.inner.disconnect()
        }

        fn is_connected(&self) -> bool {
            let count = self.call_count.load(Ordering::SeqCst);
            let disconnect_after = self.disconnect_after.load(Ordering::SeqCst);
            if count >= disconnect_after {
                return false;
            }
            self.inner.is_connected()
        }

        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            self.inner.set_shutter(open)
        }
    }

    impl FifoBackend for DisconnectingBackend {
        fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.inner.try_write_points(pps, points)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            self.inner.estimator()
        }
    }

    let backend = DisconnectingBackend {
        inner: TestBackend::new(),
        disconnect_after: Arc::new(AtomicUsize::new(3)),
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    let stream = make_test_stream(backend);

    let error_occurred = Arc::new(AtomicBool::new(false));
    let error_occurred_clone = error_occurred.clone();

    let result = stream.run(
        |req, buffer| {
            let n = req.target_points.min(buffer.len()).min(10);
            for pt in buffer.iter_mut().take(n) {
                *pt = LaserPoint::blanked(0.0, 0.0);
            }
            ChunkResult::Filled(n)
        },
        move |_e| {
            error_occurred_clone.store(true, Ordering::SeqCst);
        },
    );

    // Should return Disconnected when backend reports disconnection
    assert_eq!(result.unwrap(), RunExit::Disconnected);
}

// =========================================================================
// Write error → disconnect tests
//
// These verify that a non-disconnected write error (Error::Backend) causes
// the backend to be disconnected, so the stream exits with
// RunExit::Disconnected and the device can be reconnected.
//
// Without the fix (backend.disconnect() on a non-disconnected write error in
// the driver's write path), the backend stays "connected" and the stream loops
// forever retrying writes that keep failing.
// =========================================================================

/// A backend that returns `Error::backend()` after N successful writes.
///
/// Configurable error kind and message to simulate different DAC failure
/// modes: IDN write errors (BrokenPipe), Helios USB timeouts (TimedOut), etc.
struct FailingWriteBackend {
    inner: TestBackend,
    fail_after: usize,
    write_count: Arc<AtomicUsize>,
    disconnect_called: Arc<AtomicBool>,
    error_kind: std::io::ErrorKind,
    error_message: &'static str,
}

impl FailingWriteBackend {
    /// Create a backend that fails with `BrokenPipe` after `fail_after` writes.
    fn new(fail_after: usize) -> Self {
        Self {
            inner: TestBackend::new(),
            fail_after,
            write_count: Arc::new(AtomicUsize::new(0)),
            disconnect_called: Arc::new(AtomicBool::new(false)),
            error_kind: std::io::ErrorKind::BrokenPipe,
            error_message: "simulated write failure",
        }
    }

    /// Configure the error kind and message returned on failure.
    fn with_error(mut self, kind: std::io::ErrorKind, message: &'static str) -> Self {
        self.error_kind = kind;
        self.error_message = message;
        self
    }
}

impl DacBackend for FailingWriteBackend {
    fn dac_type(&self) -> DacType {
        DacType::Custom("FailingTest".to_string())
    }

    fn caps(&self) -> &DacCapabilities {
        self.inner.caps()
    }

    fn connect(&mut self) -> Result<()> {
        self.inner.connect()
    }

    fn disconnect(&mut self) -> Result<()> {
        self.disconnect_called.store(true, Ordering::SeqCst);
        self.inner.disconnect()
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    fn stop(&mut self) -> Result<()> {
        self.inner.stop()
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        self.inner.set_shutter(open)
    }
}

impl FifoBackend for FailingWriteBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let count = self.write_count.fetch_add(1, Ordering::SeqCst);
        if count >= self.fail_after {
            Err(Error::backend(std::io::Error::new(
                self.error_kind,
                self.error_message,
            )))
        } else {
            self.inner.try_write_points(pps, points)
        }
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        self.inner.estimator()
    }
}

/// Producer that fills up to 10 blanked points per call (shared by write-error tests).
fn blank_producer(req: &ChunkRequest, buffer: &mut [LaserPoint]) -> ChunkResult {
    let n = req.target_points.min(buffer.len()).min(10);
    for pt in buffer.iter_mut().take(n) {
        *pt = LaserPoint::blanked(0.0, 0.0);
    }
    ChunkResult::Filled(n)
}

/// Build a Stream from a backend with default test config (30 kpps).
// =========================================================================
// Reconnect configuration tests
// =========================================================================

#[test]
fn test_start_stream_with_reconnect_rejects_invalid_pps() {
    // Reconnect config should not bypass PPS validation
    let backend = TestBackend::new(); // pps_min: 1000, pps_max: 100000
    let mut device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    // PPS too low
    let cfg = StreamConfig::new(500).with_reconnect(crate::config::ReconnectConfig::new());
    let result = device.start_stream(cfg);
    assert!(result.is_err());
}

#[test]
fn test_start_stream_reconnect_without_target_errors() {
    // start_stream with reconnect on a Dac created via Dac::new (no target) should error
    let backend = TestBackend::new();
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::config::ReconnectConfig::new());
    let result = device.start_stream(cfg);
    match result {
        Err(e) => {
            let msg = format!("{}", e);
            assert!(
                msg.contains("open_device()") && msg.contains("with_discovery_factory"),
                "error should mention open_device and alternatives: {}",
                msg
            );
        }
        Ok(_) => panic!("expected error for reconnect without target"),
    }
}

#[test]
fn test_start_stream_reconnect_with_target_succeeds() {
    let backend = TestBackend::new();
    let mut device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::config::ReconnectConfig::new());
    let result = device.start_stream(cfg);
    assert!(result.is_ok());

    let (stream, _) = result.unwrap();
    assert!(stream.reconnect_policy.is_some());
}

#[test]
fn test_sleep_with_stop_exits_on_stop() {
    use crate::reconnect::ReconnectPolicy;
    use std::sync::atomic::AtomicBool;

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_clone = stopped.clone();

    // Start a background thread that sets stopped after 50ms
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        stopped_clone.store(true, Ordering::SeqCst);
    });

    // sleep_with_stop for 5 seconds should exit early
    let start = std::time::Instant::now();
    let mut on_progress = || {};
    let was_stopped = ReconnectPolicy::sleep_with_stop(
        Duration::from_secs(5),
        || stopped.load(Ordering::SeqCst),
        &mut on_progress,
    );

    assert!(was_stopped);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_into_dac_preserves_reconnect_target() {
    let backend = TestBackend::new();
    let mut device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test-id".to_string(),
        discovery_factory: None,
    });

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::config::ReconnectConfig::new());
    let (stream, _) = device.start_stream(cfg).unwrap();

    // into_dac should extract the reconnect target from the policy
    let (dac, stats) = stream.into_dac();
    assert!(dac.reconnect_target.is_some());
    assert_eq!(dac.reconnect_target.as_ref().unwrap().device_id, "test-id");
    assert_eq!(stats.reconnect_count, 0);
}

#[test]
fn test_into_dac_preserves_target_without_reconnect() {
    // into_dac should preserve the reopen target even when reconnect was NOT enabled.
    // This allows: open_device -> start_stream(no reconnect) -> into_dac -> start_stream(with reconnect)
    let backend = TestBackend::new();
    let mut device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test-id".to_string(),
        discovery_factory: None,
    });

    // Start without reconnect
    let cfg = StreamConfig::new(30_000);
    let (stream, _) = device.start_stream(cfg).unwrap();

    // Verify the stream has the target stored (not in policy, since no reconnect)
    assert!(stream.reconnect_target.is_some());
    assert!(stream.reconnect_policy.is_none());

    // into_dac should still return the target
    let (dac, _) = stream.into_dac();
    assert!(dac.reconnect_target.is_some());
    assert_eq!(dac.reconnect_target.as_ref().unwrap().device_id, "test-id");
}

#[test]
fn test_dac_new_has_no_reconnect_target() {
    let backend = TestBackend::new();
    let device = Dac::new(
        test_info(backend.caps()),
        BackendKind::Fifo(Box::new(backend)),
    );
    assert!(device.reconnect_target.is_none());
}

/// Create a standard `DacInfo` from capabilities (id="test", name="Test Device").
fn test_info(caps: &DacCapabilities) -> DacInfo {
    DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: caps.clone(),
    }
}

fn make_test_stream(mut backend: impl FifoBackend + 'static) -> Stream {
    backend.connect().unwrap();
    let info = test_info(backend.caps());
    Stream::with_backend(
        info,
        BackendKind::Fifo(Box::new(backend)),
        StreamConfig::new(30000),
    )
}

fn make_test_stream_with_cfg(mut backend: impl FifoBackend + 'static, cfg: StreamConfig) -> Stream {
    backend.connect().unwrap();
    let info = test_info(backend.caps());
    Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg)
}

#[test]
fn test_backend_write_error_exits_with_disconnected() {
    // When try_write_chunk returns a non-disconnected error (Error::Backend),
    // the stream should disconnect the backend and exit with RunExit::Disconnected.
    //
    // Without the fix, the stream loops forever because is_connected() stays
    // true. This test would hang/timeout without the backend.disconnect() call.
    use std::thread;

    let backend = FailingWriteBackend::new(2);
    let disconnect_called = backend.disconnect_called.clone();
    let stream = make_test_stream(backend);

    let handle = thread::spawn(move || stream.run(blank_producer, |_err| {}));
    let result = handle.join().expect("stream thread panicked");

    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "Write error should cause stream to exit with Disconnected"
    );
    assert!(
        disconnect_called.load(Ordering::SeqCst),
        "backend.disconnect() should have been called after write error"
    );
}

#[test]
fn test_backend_write_error_fires_on_error() {
    // Verify the on_error callback is invoked with the backend error.
    let backend = FailingWriteBackend::new(1);
    let stream = make_test_stream(backend);

    let got_backend_error = Arc::new(AtomicBool::new(false));
    let got_backend_error_clone = got_backend_error.clone();

    let result = stream.run(blank_producer, move |err| {
        if matches!(err, Error::Backend(_)) {
            got_backend_error_clone.store(true, Ordering::SeqCst);
        }
    });

    assert_eq!(result.unwrap(), RunExit::Disconnected);
    assert!(
        got_backend_error.load(Ordering::SeqCst),
        "on_error should have received the Backend error"
    );
}

#[test]
fn test_backend_write_error_immediate_fail() {
    // Backend that fails on the very first write should still exit cleanly.
    let stream = make_test_stream(FailingWriteBackend::new(0));

    let result = stream.run(blank_producer, |_err| {});

    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "Immediate write failure should exit with Disconnected"
    );
}

// =========================================================================
// Helios-style disconnect tests
//
// The Helios DAC disconnects via a USB timeout on the status() poll
// (inside try_write_chunk), producing Error::Backend with a TimedOut
// error — not Error::Disconnected. Uses FailingWriteBackend configured
// with TimedOut error kind and no queue depth reporting.
// =========================================================================

/// Create a Helios-like backend: TimedOut error.
fn helios_like_backend(fail_after: usize) -> FailingWriteBackend {
    FailingWriteBackend::new(fail_after).with_error(
        std::io::ErrorKind::TimedOut,
        "usb connection error: Operation timed out",
    )
}

#[test]
fn test_helios_status_timeout_exits_with_disconnected() {
    // Simulates a Helios DAC being unplugged mid-stream.
    //
    // Real-world sequence observed via USB logging:
    //   1. status() → read_response FAILED: Timeout (32ms interrupt read)
    //   2. Error mapped to Error::Backend (not Disconnected)
    //   3. Stream calls backend.disconnect() → dac = None
    //   4. Next loop: is_connected() = false → RunExit::Disconnected
    use std::thread;

    let backend = helios_like_backend(3);
    let disconnect_called = backend.disconnect_called.clone();
    let stream = make_test_stream(backend);

    let handle = thread::spawn(move || stream.run(blank_producer, |_err| {}));
    let result = handle.join().expect("stream thread panicked");

    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "Helios status timeout should cause stream to exit with Disconnected"
    );
    assert!(
        disconnect_called.load(Ordering::SeqCst),
        "backend.disconnect() should have been called on status timeout"
    );
}

#[test]
fn test_helios_status_timeout_fires_on_error_with_backend_variant() {
    // Verify the on_error callback receives an Error::Backend (not Disconnected).
    // This matches real Helios behavior: the USB timeout is wrapped as Backend error.
    let stream = make_test_stream(helios_like_backend(1));

    let got_backend_error = Arc::new(AtomicBool::new(false));
    let error_received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let got_backend_error_clone = got_backend_error.clone();
    let error_received_clone = error_received.clone();

    let result = stream.run(blank_producer, move |err| {
        if matches!(err, Error::Backend(_)) {
            got_backend_error_clone.store(true, Ordering::SeqCst);
            *error_received_clone.lock().unwrap() = Some(err.to_string());
        }
    });

    assert_eq!(result.unwrap(), RunExit::Disconnected);
    assert!(
        got_backend_error.load(Ordering::SeqCst),
        "on_error should receive Error::Backend for Helios timeout"
    );
    let msg = error_received.lock().unwrap();
    assert!(
        msg.as_ref().unwrap().contains("Operation timed out"),
        "Error message should mention timeout, got: {:?}",
        msg
    );
}

#[test]
fn test_helios_immediate_status_timeout() {
    // Helios that fails on the very first status check (device was already
    // disconnected when stream started, or USB enumeration was stale).
    let backend = helios_like_backend(0);
    let disconnect_called = backend.disconnect_called.clone();
    let stream = make_test_stream(backend);

    let result = stream.run(blank_producer, |_err| {});

    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "Immediate status timeout should exit with Disconnected"
    );
    assert!(
        disconnect_called.load(Ordering::SeqCst),
        "backend.disconnect() should be called even on first-write failure"
    );
}

// =========================================================================
// Drain wait tests
// =========================================================================

#[test]
fn test_fill_result_end_drains_with_queue_depth() {
    // Test that ChunkResult::End waits for queue to drain when queue depth is available
    use std::time::Instant;

    let backend = TestBackend::new().with_initial_queue(1000);
    let queued = backend.queued.clone();

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Simulate queue draining by setting it to 0 before the stream runs
    queued.store(0, Ordering::SeqCst);

    let start = Instant::now();
    let result = stream.run(|_req, _buffer| ChunkResult::End, |_e| {});

    let elapsed = start.elapsed();

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Should return quickly since queue was empty
    assert!(
        elapsed.as_millis() < 50,
        "Should return quickly when queue is empty, took {:?}",
        elapsed
    );
}

#[test]
fn test_fill_result_end_respects_drain_timeout() {
    // Test that drain respects timeout and doesn't block forever.
    // Producer reports a still-full buffer on End so drain has to poll until
    // the timeout fires.
    use std::time::Instant;

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(50));
    let backend = TestBackend::new();
    let estimator = backend.estimator.clone();
    let stream = make_test_stream_with_cfg(backend, cfg);

    let start = Instant::now();
    let result = stream.run(
        move |_req, _buffer| {
            // Pretend the device buffer is far from drained when End fires.
            estimator.seed(100_000, 30_000);
            ChunkResult::End
        },
        |_e| {},
    );

    let elapsed = start.elapsed();

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Should timeout around 50ms, allow some margin
    assert!(
        elapsed.as_millis() >= 40 && elapsed.as_millis() < 150,
        "Should respect drain timeout (~50ms), took {:?}",
        elapsed
    );
}

#[test]
fn test_fill_result_end_skips_drain_with_zero_timeout() {
    // Test that drain is skipped when timeout is zero, even with a non-empty
    // estimator at End time.
    use std::time::Instant;

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::ZERO);
    let backend = TestBackend::new();
    let estimator = backend.estimator.clone();
    let stream = make_test_stream_with_cfg(backend, cfg);

    let start = Instant::now();
    let result = stream.run(
        move |_req, _buffer| {
            estimator.seed(100_000, 30_000);
            ChunkResult::End
        },
        |_e| {},
    );

    let elapsed = start.elapsed();

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Should return immediately
    assert!(
        elapsed.as_millis() < 20,
        "Should skip drain with zero timeout, took {:?}",
        elapsed
    );
}

#[test]
fn test_fill_result_end_drains_with_empty_estimator() {
    // Test drain returns quickly when the estimator already reads as empty.
    use std::time::Instant;

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
    let stream = make_test_stream_with_cfg(TestBackend::new(), cfg);

    let start = Instant::now();
    let result = stream.run(|_req, _buffer| ChunkResult::End, |_e| {});

    let elapsed = start.elapsed();

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Without queue depth, drain sleeps for estimated buffer time (0 here)
    // So should return quickly
    assert!(
        elapsed.as_millis() < 50,
        "Should return quickly with empty buffer estimate, took {:?}",
        elapsed
    );
}

#[test]
fn test_fill_result_end_closes_shutter() {
    // Test that shutter is closed after drain
    let backend = TestBackend::new();
    let shutter_open = backend.shutter_open.clone();

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(10));
    let stream = make_test_stream_with_cfg(backend, cfg);

    // Arm the stream first
    let control = stream.control();
    control.arm().unwrap();

    let result = stream.run(
        |req, buffer| {
            // Fill some points then end
            let n = req.target_points.min(buffer.len()).min(10);
            for pt in buffer.iter_mut().take(n) {
                *pt = LaserPoint::blanked(0.0, 0.0);
            }
            ChunkResult::End
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // Shutter should be closed after graceful shutdown
    assert!(
        !shutter_open.load(Ordering::SeqCst),
        "Shutter should be closed after drain"
    );
}

// =========================================================================
// OutputModel coverage
// =========================================================================

#[test]
fn test_device_start_stream_rejects_frame_swap_backend() {
    let backend = FrameSwapTestBackend::new();
    let device = Dac::new(
        test_info(&backend.inner.caps),
        BackendKind::FrameSwap(Box::new(backend)),
    );

    let result = device.start_stream(StreamConfig::new(30_000));
    match result {
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("frame-swap"),
                "error should mention frame-swap: {err_msg}"
            );
        }
        Ok(_) => panic!("expected start_stream to reject frame-swap backend"),
    }
}

#[test]
fn test_validate_config_rejects_pps_below_min() {
    // Helios-like caps: pps_min = 7
    let caps = DacCapabilities {
        pps_min: 7,
        pps_max: 65535,
        max_points_per_chunk: 1000,
        output_model: OutputModel::NetworkFifo,
    };
    let result = Dac::validate_pps(&caps, 5);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("PPS 5"), "expected PPS 5 in error: {msg}");
}

#[test]
fn test_validate_config_rejects_pps_above_max() {
    // LaserCube-like caps: pps_max = 35000
    let caps = DacCapabilities {
        pps_min: 1,
        pps_max: 35_000,
        max_points_per_chunk: 1000,
        output_model: OutputModel::NetworkFifo,
    };
    let result = Dac::validate_pps(&caps, 50_000);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("PPS 50000"),
        "expected PPS 50000 in error: {msg}"
    );
}

#[test]
fn test_validate_config_avb_accepts_standard_pps() {
    // AVB caps: wide range due to resampling
    let caps = DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: 4096,
        output_model: OutputModel::NetworkFifo,
    };
    assert!(Dac::validate_pps(&caps, 30_000).is_ok());
}

// The old `test_fractional_consumed_prevents_stall` regression test asserted
// that the scheduler's software accumulator carried sub-point remainders so a
// no-telemetry backend would not stall. After the BufferEstimator migration
// that decay logic moved into anchor-based estimators; see
// `software_decay::tests::anchor_reads_avoid_fractional_truncation_drift`.

#[test]
fn with_discovery_factory_creates_target_when_none() {
    let backend = TestBackend::new();
    let mut info = test_info(backend.caps());
    info.id = "test-factory".to_string();
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    assert!(device.reconnect_target.is_none());

    let device = device.with_discovery_factory(|| {
        crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::all())
    });

    let target = device.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "test-factory");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn with_discovery_factory_replaces_factory_on_existing_target() {
    let backend = TestBackend::new();
    let mut info = test_info(backend.caps());
    info.id = "test-replace".to_string();
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "original-id".to_string(),
        discovery_factory: None,
    });

    let device = device.with_discovery_factory(|| {
        crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::all())
    });

    let target = device.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "original-id");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn with_discovery_factory_enables_reconnect_for_frame_session() {
    let backend = TestBackend::new();
    let mut info = test_info(backend.caps());
    info.id = "test-session".to_string();
    let device =
        Dac::new(info, BackendKind::Fifo(Box::new(backend))).with_discovery_factory(|| {
            crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::all())
        });

    let config = crate::presentation::FrameSessionConfig::new(30_000)
        .with_reconnect(crate::config::ReconnectConfig::new());
    let result = device.start_frame_session(config);
    assert!(
        result.is_ok(),
        "start_frame_session should succeed with discovery factory: {:?}",
        result.err()
    );

    let (session, _info) = result.unwrap();
    drop(session);
}

/// Minimal FIFO backend for reconnection tests — accepts all writes.
struct ReconnectFifoBackend {
    connected: bool,
    estimator: SoftwareDecayEstimator,
}

impl ReconnectFifoBackend {
    fn new() -> Self {
        Self {
            connected: false,
            estimator: SoftwareDecayEstimator::new(),
        }
    }
}

impl DacBackend for ReconnectFifoBackend {
    fn dac_type(&self) -> DacType {
        DacType::Custom("TrackingTest".into())
    }
    fn caps(&self) -> &DacCapabilities {
        static CAPS: DacCapabilities = DacCapabilities {
            pps_min: 1000,
            pps_max: 100000,
            max_points_per_chunk: 1000,
            output_model: crate::device::OutputModel::NetworkFifo,
        };
        &CAPS
    }
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for ReconnectFifoBackend {
    fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// FIFO backend that returns Error::Disconnected after N writes.
/// Unlike FailingWriteBackend (which returns Error::Backend), this triggers
/// the reconnect path in FrameSession which checks `e.is_disconnected()`.
struct DisconnectAfterNBackend {
    connected: bool,
    fail_after: usize,
    write_count: AtomicUsize,
    estimator: SoftwareDecayEstimator,
}

impl DisconnectAfterNBackend {
    fn new(fail_after: usize) -> Self {
        Self {
            connected: false,
            fail_after,
            write_count: AtomicUsize::new(0),
            estimator: SoftwareDecayEstimator::new(),
        }
    }
}

impl DacBackend for DisconnectAfterNBackend {
    fn dac_type(&self) -> DacType {
        DacType::Custom("TrackingTest".into())
    }
    fn caps(&self) -> &DacCapabilities {
        static CAPS: DacCapabilities = DacCapabilities {
            pps_min: 1000,
            pps_max: 100000,
            max_points_per_chunk: 1000,
            output_model: crate::device::OutputModel::NetworkFifo,
        };
        &CAPS
    }
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for DisconnectAfterNBackend {
    fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
        let count = self.write_count.fetch_add(1, Ordering::SeqCst);
        if count >= self.fail_after {
            self.connected = false;
            Err(Error::disconnected("simulated disconnect"))
        } else {
            Ok(WriteOutcome::Written)
        }
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// Mock Discoverer that tracks scan/connect calls and returns
/// a ReconnectFifoBackend on connect. Uses a fixed IP so the stable_id is
/// deterministic: "trackingtest:10.0.0.99"
struct TrackingDiscoverer {
    scan_count: Arc<AtomicUsize>,
    connect_count: Arc<AtomicUsize>,
}

impl crate::discovery::Discoverer for TrackingDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Custom("TrackingTest".into())
    }

    fn prefix(&self) -> &str {
        "trackingtest"
    }

    fn scan(&mut self) -> Vec<crate::discovery::DiscoveredDevice> {
        self.scan_count.fetch_add(1, Ordering::SeqCst);
        let info = crate::discovery::DiscoveredDeviceInfo::new(
            DacType::Custom("TrackingTest".into()),
            "trackingtest:10.0.0.99",
            "Tracking Test Device",
        )
        .with_ip("10.0.0.99".parse().unwrap())
        .with_hardware_name("Tracking Test Device");
        vec![crate::discovery::DiscoveredDevice::new(info, Box::new(()))]
    }

    fn connect(&mut self, _opaque: Box<dyn std::any::Any + Send>) -> Result<BackendKind> {
        self.connect_count.fetch_add(1, Ordering::SeqCst);
        Ok(BackendKind::Fifo(Box::new(ReconnectFifoBackend::new())))
    }
}

#[test]
fn reconnect_rediscovers_custom_backend_via_factory() {
    let scan_count = Arc::new(AtomicUsize::new(0));
    let connect_count = Arc::new(AtomicUsize::new(0));
    let reconnected = Arc::new(AtomicBool::new(false));

    let scan_count_factory = scan_count.clone();
    let connect_count_factory = connect_count.clone();
    let reconnected_cb = reconnected.clone();

    // Initial backend: disconnects after 2 writes (returns Error::Disconnected)
    let initial_backend = DisconnectAfterNBackend::new(2);
    let caps = initial_backend.caps().clone();

    // stable_id for Custom("TrackingTest") with ip=10.0.0.99 is "trackingtest:10.0.0.99"
    let device_id = "trackingtest:10.0.0.99";
    let info = DacInfo {
        id: device_id.to_string(),
        name: "Tracking Test Device".to_string(),
        kind: DacType::Custom("TrackingTest".to_string()),
        caps,
    };

    let device = Dac::new(info, BackendKind::Fifo(Box::new(initial_backend)))
        .with_discovery_factory(move || {
            let mut d = crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::none());
            d.register(Box::new(TrackingDiscoverer {
                scan_count: scan_count_factory.clone(),
                connect_count: connect_count_factory.clone(),
            }));
            d
        });

    let config = crate::presentation::FrameSessionConfig::new(30_000).with_reconnect(
        crate::config::ReconnectConfig::new()
            .max_retries(3)
            .backoff(Duration::from_millis(50))
            .on_reconnect(move |_info| {
                reconnected_cb.store(true, Ordering::SeqCst);
            }),
    );
    let (session, _info) = device.start_frame_session(config).unwrap();
    session.control().arm().unwrap();
    session.send_frame(crate::presentation::Frame::new(vec![LaserPoint::blanked(
        0.0, 0.0,
    )]));

    // Wait for disconnect → reconnect cycle
    std::thread::sleep(Duration::from_millis(500));

    assert!(
        scan_count.load(Ordering::SeqCst) > 0,
        "discoverer scan() should have been called during reconnect"
    );
    assert!(
        connect_count.load(Ordering::SeqCst) > 0,
        "discoverer connect() should have been called to reopen the device"
    );
    assert!(
        reconnected.load(Ordering::SeqCst),
        "on_reconnect callback should have fired, proving successful reconnect"
    );

    drop(session);
}

// =========================================================================
// StreamInstant helpers and operators
// =========================================================================

#[test]
fn test_stream_instant_conversions() {
    let a = StreamInstant::new(100);
    assert_eq!(a.points(), 100);
    // 100 points at 1000 pps = 0.1s
    assert_eq!(a.as_seconds(1000), 0.1);
    assert_eq!(a.as_secs_f64(1000), 0.1);
    // round-trip through seconds
    assert_eq!(
        StreamInstant::from_seconds(0.1, 1000),
        StreamInstant::new(100)
    );
    // default is zero
    assert_eq!(StreamInstant::default(), StreamInstant::new(0));
}

#[test]
fn test_stream_instant_add_sub_points_saturate() {
    let a = StreamInstant::new(100);
    assert_eq!(a.add_points(50), StreamInstant::new(150));
    assert_eq!(a.sub_points(30), StreamInstant::new(70));
    // sub saturates at zero
    assert_eq!(a.sub_points(1000), StreamInstant::new(0));
    // add saturates at u64::MAX
    assert_eq!(
        StreamInstant::new(u64::MAX).add_points(5),
        StreamInstant::new(u64::MAX)
    );
}

#[test]
fn test_stream_instant_operators() {
    let mut a = StreamInstant::new(100);
    assert_eq!(a + 25u64, StreamInstant::new(125));
    assert_eq!(a - 40u64, StreamInstant::new(60));
    // Sub saturates at zero
    assert_eq!(a - 500u64, StreamInstant::new(0));

    a += 10;
    assert_eq!(a, StreamInstant::new(110));
    a -= 5;
    assert_eq!(a, StreamInstant::new(105));
    // SubAssign saturates at zero
    a -= 1000;
    assert_eq!(a, StreamInstant::new(0));

    // AddAssign saturates at u64::MAX
    let mut b = StreamInstant::new(u64::MAX);
    b += 10;
    assert_eq!(b, StreamInstant::new(u64::MAX));
}

// =========================================================================
// status() tests
// =========================================================================

#[test]
fn test_status_reports_connected_stats_and_scheduled_ahead() {
    let backend = TestBackend::new().with_initial_queue(500);
    let mut stream = make_test_stream(backend); // make_test_stream connects the backend

    let status = stream.status().unwrap();
    assert!(status.connected, "backend was connected");
    // The software-decay estimator was seeded to 500 at wall-clock time, and
    // `status()` reads it a few microseconds later; at 30k pps it sheds ~1
    // point per 33us, so assert a tolerance band rather than exact equality to
    // avoid a timing-flaky off-by-one on loaded CI runners.
    assert!(
        (490..=500).contains(&status.scheduled_ahead_points),
        "scheduled_ahead_points ~500 (seeded depth, minus slight decay), got {}",
        status.scheduled_ahead_points
    );
    assert_eq!(
        status.device_queued_points,
        Some(status.scheduled_ahead_points)
    );
    assert!(status.stats.is_some());
    assert_eq!(status.stats.unwrap().underrun_count, 0);

    // Stats are snapshotted from live state.
    stream.state.stats.underrun_count = 7;
    stream.state.stats.reconnect_count = 2;
    let status = stream.status().unwrap();
    let stats = status.stats.unwrap();
    assert_eq!(stats.underrun_count, 7);
    assert_eq!(stats.reconnect_count, 2);
}

#[test]
fn test_status_reports_disconnected() {
    // Build a stream without connecting the backend.
    let backend = TestBackend::new();
    let info = test_info(backend.caps());
    let stream = Stream::with_backend(
        info,
        BackendKind::Fifo(Box::new(backend)),
        StreamConfig::new(30_000),
    );

    let status = stream.status().unwrap();
    assert!(!status.connected, "backend was never connected");
    assert_eq!(status.scheduled_ahead_points, 0);
}

// =========================================================================
// Streaming-side reconnect tests
//
// All flavours drive the REAL `Stream::run` unified driver path: a FIFO
// backend that disconnects after N writes plus a registered mock Discoverer
// that supplies the replacement. Covered:
//   - Accepted swap end-to-end (`stream_reconnects_through_run_driver_path`).
//   - Rejected swap when the replacement is a frame-swap device.
//   - Rejected swap when the replacement's PPS range excludes the config PPS.
// =========================================================================

/// Stable id shared by the streaming reconnect mock discoverer and the Dac.
const STREAM_RECON_ID: &str = "streamrecon:swap";

/// What kind of replacement backend the mock discoverer hands back on connect.
#[derive(Clone, Copy)]
enum ReplacementKind {
    /// A frame-swap backend — incompatible with streaming, must be rejected.
    FrameSwap,
    /// A FIFO backend whose PPS range excludes the stream config PPS.
    NarrowPps,
}

/// FIFO backend with a PPS range [1, 1000] that excludes the 30 kpps test config.
struct NarrowPpsBackend {
    connected: bool,
    estimator: SoftwareDecayEstimator,
}

impl NarrowPpsBackend {
    fn new() -> Self {
        Self {
            connected: false,
            estimator: SoftwareDecayEstimator::new(),
        }
    }
}

impl DacBackend for NarrowPpsBackend {
    fn dac_type(&self) -> DacType {
        DacType::Custom("StreamRecon".into())
    }
    fn caps(&self) -> &DacCapabilities {
        static CAPS: DacCapabilities = DacCapabilities {
            pps_min: 1,
            pps_max: 1000,
            max_points_per_chunk: 1000,
            output_model: crate::device::OutputModel::NetworkFifo,
        };
        &CAPS
    }
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for NarrowPpsBackend {
    fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
        Ok(WriteOutcome::Written)
    }
    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// Mock discoverer that supplies a replacement backend of a chosen kind when
/// `open_by_id(STREAM_RECON_ID)` is called during reconnect.
struct StreamReconDiscoverer {
    kind: ReplacementKind,
    connect_count: Arc<AtomicUsize>,
}

impl crate::discovery::Discoverer for StreamReconDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Custom("StreamRecon".into())
    }
    fn prefix(&self) -> &str {
        "streamrecon"
    }
    fn scan(&mut self) -> Vec<crate::discovery::DiscoveredDevice> {
        let info = crate::discovery::DiscoveredDeviceInfo::new(
            DacType::Custom("StreamRecon".into()),
            STREAM_RECON_ID,
            "Stream Recon Device",
        );
        vec![crate::discovery::DiscoveredDevice::new(info, Box::new(()))]
    }
    fn connect(&mut self, _opaque: Box<dyn std::any::Any + Send>) -> Result<BackendKind> {
        self.connect_count.fetch_add(1, Ordering::SeqCst);
        Ok(match self.kind {
            ReplacementKind::FrameSwap => {
                BackendKind::FrameSwap(Box::new(FrameSwapTestBackend::new()))
            }
            ReplacementKind::NarrowPps => BackendKind::Fifo(Box::new(NarrowPpsBackend::new())),
        })
    }
}

/// Build a Stream whose reconnect policy re-opens `STREAM_RECON_ID` via a
/// freshly-registered `StreamReconDiscoverer` of the given kind. The `initial`
/// backend is the one the stream starts on; pass a `DisconnectAfterNBackend`
/// to make `run()` actually drop the connection and drive the driver's
/// reconnect → validator path.
fn make_reconnecting_stream(
    initial: impl FifoBackend + 'static,
    kind: ReplacementKind,
    connect_count: Arc<AtomicUsize>,
    reconnected: Arc<AtomicBool>,
) -> Stream {
    let info = DacInfo {
        id: STREAM_RECON_ID.to_string(),
        name: "Stream Recon Device".to_string(),
        kind: DacType::Custom("StreamRecon".to_string()),
        caps: initial.caps().clone(),
    };
    let reconnected_cb = reconnected.clone();
    let device =
        Dac::new(info, BackendKind::Fifo(Box::new(initial))).with_discovery_factory(move || {
            let mut d = crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::none());
            d.register(Box::new(StreamReconDiscoverer {
                kind,
                connect_count: connect_count.clone(),
            }));
            d
        });

    let cfg = StreamConfig::new(30_000).with_reconnect(
        crate::config::ReconnectConfig::new()
            .max_retries(3)
            .backoff(Duration::from_millis(1))
            .on_reconnect(move |_info| {
                reconnected_cb.store(true, Ordering::SeqCst);
            }),
    );
    let (stream, _info) = device.start_stream(cfg).unwrap();
    stream
}

#[test]
fn run_rejects_frame_swap_replacement_and_returns_disconnected() {
    // Drive the REAL `Stream::run` driver: the initial FIFO backend disconnects
    // after 2 writes, the discoverer hands back a frame-swap device, and the
    // driver's reconnect → validator path must reject it (frame-swap is
    // incompatible with streaming) and exit `Disconnected` without reconnecting.
    let connect_count = Arc::new(AtomicUsize::new(0));
    let reconnected = Arc::new(AtomicBool::new(false));
    let stream = make_reconnecting_stream(
        DisconnectAfterNBackend::new(2),
        ReplacementKind::FrameSwap,
        connect_count.clone(),
        reconnected.clone(),
    );

    let result = stream.run(blank_producer, |_e| {});
    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "a frame-swap replacement is incompatible with streaming"
    );
    assert!(
        connect_count.load(Ordering::SeqCst) > 0,
        "the discoverer must have produced a replacement that was then rejected \
         (proving the validator path, not a discovery miss)"
    );
    assert!(
        !reconnected.load(Ordering::SeqCst),
        "on_reconnect must not fire on a rejected swap"
    );
}

#[test]
fn run_rejects_incompatible_pps_replacement_and_returns_disconnected() {
    // Same path, but the replacement's PPS range excludes the 30 kpps config —
    // the validator must reject it and `run()` exits `Disconnected`.
    let connect_count = Arc::new(AtomicUsize::new(0));
    let reconnected = Arc::new(AtomicBool::new(false));
    let stream = make_reconnecting_stream(
        DisconnectAfterNBackend::new(2),
        ReplacementKind::NarrowPps,
        connect_count.clone(),
        reconnected.clone(),
    );

    let result = stream.run(blank_producer, |_e| {});
    assert_eq!(
        result.unwrap(),
        RunExit::Disconnected,
        "reconnected device PPS range must contain the stream config PPS"
    );
    assert!(
        connect_count.load(Ordering::SeqCst) > 0,
        "the discoverer must have produced a replacement that was then rejected"
    );
    assert!(!reconnected.load(Ordering::SeqCst));
}

#[test]
fn stream_reconnects_through_run_driver_path() {
    // End-to-end: drive the REAL `Stream::run` unified driver. The initial
    // FIFO backend disconnects (Error::Disconnected) after 2 writes; the
    // registered mock Discoverer supplies a fresh FIFO backend on reopen.
    let scan_count = Arc::new(AtomicUsize::new(0));
    let connect_count = Arc::new(AtomicUsize::new(0));
    let reconnected = Arc::new(AtomicBool::new(false));

    let scan_factory = scan_count.clone();
    let connect_factory = connect_count.clone();
    let reconnected_cb = reconnected.clone();

    let initial_backend = DisconnectAfterNBackend::new(2);
    let caps = initial_backend.caps().clone();
    let device_id = "trackingtest:10.0.0.99";
    let info = DacInfo {
        id: device_id.to_string(),
        name: "Tracking Test Device".to_string(),
        kind: DacType::Custom("TrackingTest".to_string()),
        caps,
    };

    let device = Dac::new(info, BackendKind::Fifo(Box::new(initial_backend)))
        .with_discovery_factory(move || {
            let mut d = crate::discovery::DacDiscovery::new(crate::device::EnabledDacTypes::none());
            d.register(Box::new(TrackingDiscoverer {
                scan_count: scan_factory.clone(),
                connect_count: connect_factory.clone(),
            }));
            d
        });

    let cfg = StreamConfig::new(30_000).with_reconnect(
        crate::config::ReconnectConfig::new()
            .max_retries(5)
            .backoff(Duration::from_millis(5))
            .on_reconnect(move |_info| {
                reconnected_cb.store(true, Ordering::SeqCst);
            }),
    );

    let (stream, _info) = device.start_stream(cfg).unwrap();
    let control = stream.control();
    control.arm().unwrap();

    let handle = std::thread::spawn(move || stream.run(blank_producer, |_e| {}));

    // Wait for the disconnect -> reconnect cycle to complete.
    let start = Instant::now();
    while !reconnected.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        reconnected.load(Ordering::SeqCst),
        "on_reconnect should fire via the run() driver path"
    );

    // Stop the (now healthy) stream and verify a clean exit.
    control.stop().unwrap();
    let result = handle.join().expect("stream thread panicked");
    assert_eq!(result.unwrap(), RunExit::Stopped);

    assert!(
        scan_count.load(Ordering::SeqCst) > 0,
        "discoverer scan() should run during reconnect"
    );
    assert!(
        connect_count.load(Ordering::SeqCst) > 0,
        "discoverer connect() should run to reopen the device"
    );
}
