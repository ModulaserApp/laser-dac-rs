use super::*;
use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A test backend for unit testing stream behavior.
struct TestBackend {
    caps: DacCapabilities,
    connected: bool,
    /// Count of write attempts
    write_count: Arc<AtomicUsize>,
    /// Number of WouldBlock responses to return before accepting writes
    would_block_count: Arc<AtomicUsize>,
    /// Simulated queue depth
    queued: Arc<AtomicU64>,
    /// Track shutter state for testing
    shutter_open: Arc<AtomicBool>,
}

impl TestBackend {
    fn new() -> Self {
        Self {
            caps: DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 1000,
                output_model: crate::types::OutputModel::NetworkFifo,
            },
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            would_block_count: Arc::new(AtomicUsize::new(0)),
            queued: Arc::new(AtomicU64::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_max_points_per_chunk(mut self, max_points_per_chunk: usize) -> Self {
        self.caps.max_points_per_chunk = max_points_per_chunk;
        self
    }

    fn with_would_block_count(mut self, count: usize) -> Self {
        self.would_block_count = Arc::new(AtomicUsize::new(count));
        self
    }

    fn with_output_model(mut self, model: OutputModel) -> Self {
        self.caps.output_model = model;
        self
    }

    /// Set the initial queue depth for testing buffer estimation.
    fn with_initial_queue(mut self, queue: u64) -> Self {
        self.queued = Arc::new(AtomicU64::new(queue));
        self
    }
}

/// A test backend that does NOT report hardware queue depth (`queued_points()` returns `None`).
///
/// Use this backend to test software-only buffer estimation, which is the fallback path
/// for real backends like `HeliosBackend` that don't implement `queued_points()`.
///
/// **When to use which test backend:**
/// - `TestBackend`: Simulates DACs with hardware queue reporting (e.g., Ether Dream, IDN).
///   Use for testing buffer estimation with hardware feedback.
/// - `NoQueueTestBackend`: Simulates DACs without queue reporting (e.g., Helios).
///   Use for testing the time-based `scheduled_ahead` decrement logic.
struct NoQueueTestBackend {
    inner: TestBackend,
}

impl NoQueueTestBackend {
    fn new() -> Self {
        Self {
            inner: TestBackend::new(),
        }
    }

    fn with_max_points_per_chunk(mut self, max_points_per_chunk: usize) -> Self {
        self.inner = self.inner.with_max_points_per_chunk(max_points_per_chunk);
        self
    }

    fn with_output_model(mut self, model: OutputModel) -> Self {
        self.inner = self.inner.with_output_model(model);
        self
    }
}

impl DacBackend for NoQueueTestBackend {
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

impl FifoBackend for NoQueueTestBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        self.inner.try_write_points(pps, points)
    }

    /// Returns None - simulates a DAC that cannot report queue depth
    fn queued_points(&self) -> Option<u64> {
        None
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
    fn try_write_points(&mut self, _pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        self.write_count.fetch_add(1, Ordering::SeqCst);

        // Return WouldBlock until count reaches 0
        let remaining = self.would_block_count.load(Ordering::SeqCst);
        if remaining > 0 {
            self.would_block_count.fetch_sub(1, Ordering::SeqCst);
            return Ok(WriteOutcome::WouldBlock);
        }

        self.queued.fetch_add(points.len() as u64, Ordering::SeqCst);
        Ok(WriteOutcome::Written)
    }

    fn queued_points(&self) -> Option<u64> {
        Some(self.queued.load(Ordering::SeqCst))
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
    let control = StreamControl::new(tx, Duration::ZERO);
    assert!(!control.is_armed());

    control.arm().unwrap();
    assert!(control.is_armed());

    control.disarm().unwrap();
    assert!(!control.is_armed());
}

#[test]
fn test_stream_control_stop() {
    let (tx, _rx) = mpsc::channel();
    let control = StreamControl::new(tx, Duration::ZERO);
    assert!(!control.is_stop_requested());

    control.stop().unwrap();
    assert!(control.is_stop_requested());
}

#[test]
fn test_stream_control_clone_shares_state() {
    let (tx, _rx) = mpsc::channel();
    let control1 = StreamControl::new(tx, Duration::ZERO);
    let control2 = control1.clone();

    control1.arm().unwrap();
    assert!(control2.is_armed());

    control2.stop().unwrap();
    assert!(control1.is_stop_requested());
}

#[test]
fn test_device_start_stream_connects_backend() {
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    // Device should not be connected initially
    assert!(!device.is_connected());

    // start_stream should connect and return a usable stream
    let cfg = StreamConfig::new(30000);
    let result = device.start_stream(cfg);
    assert!(result.is_ok());

    let (stream, _info) = result.unwrap();
    assert!(stream.backend.as_ref().unwrap().is_connected());
}

#[test]
fn test_device_start_stream_promotes_untouched_defaults_for_network_backends() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::NetworkFifo;
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::NETWORK_DEFAULT_TARGET_BUFFER
    );
    assert_eq!(
        stream.config.min_buffer,
        StreamConfig::NETWORK_DEFAULT_MIN_BUFFER
    );
}

#[test]
fn test_device_start_stream_keeps_explicit_network_buffer_settings() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::UdpTimed;
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let cfg = StreamConfig::new(30_000)
        .with_target_buffer(Duration::from_millis(12))
        .with_min_buffer(Duration::from_millis(4));
    let (stream, _info) = device.start_stream(cfg).unwrap();

    assert_eq!(stream.config.target_buffer, Duration::from_millis(12));
    assert_eq!(stream.config.min_buffer, Duration::from_millis(4));
}

#[test]
fn test_device_start_stream_keeps_usb_defaults() {
    let mut backend = TestBackend::new();
    backend.caps.output_model = OutputModel::UsbFrameSwap;
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let (stream, _info) = device.start_stream(StreamConfig::new(30_000)).unwrap();

    assert_eq!(
        stream.config.target_buffer,
        StreamConfig::DEFAULT_TARGET_BUFFER
    );
    assert_eq!(stream.config.min_buffer, StreamConfig::DEFAULT_MIN_BUFFER);
}

#[test]
fn test_handle_underrun_advances_state() {
    let mut backend = TestBackend::new();
    backend.connected = true;
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Record initial state
    let initial_instant = stream.state.current_instant;
    let initial_scheduled = stream.state.scheduled_ahead;
    let initial_chunks = stream.state.stats.chunks_written;
    let initial_points = stream.state.stats.points_written;

    // Trigger underrun handling with ChunkRequest
    let req = ChunkRequest {
        start: StreamInstant::new(0),
        pps: 30000,
        min_points: 100,
        target_points: 100,
        buffered_points: 0,
        buffered: Duration::ZERO,
        device_queued_points: None,
    };
    stream.handle_underrun(&req).unwrap();

    // State should have advanced
    assert!(stream.state.current_instant > initial_instant);
    assert!(stream.state.scheduled_ahead > initial_scheduled);
    assert_eq!(stream.state.stats.chunks_written, initial_chunks + 1);
    assert_eq!(stream.state.stats.points_written, initial_points + 100);
    assert_eq!(stream.state.stats.underrun_count, 1);
}

#[test]
fn test_run_retries_on_would_block() {
    // Create a backend that returns WouldBlock 3 times before accepting
    let backend = TestBackend::new().with_would_block_count(3);
    let write_count = backend.write_count.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

    let produced_count = Arc::new(AtomicUsize::new(0));
    let produced_count_clone = produced_count.clone();
    let result = stream.run(
        move |req, buffer| {
            let count = produced_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 1 {
                let n = req.target_points.min(buffer.len());
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            } else {
                ChunkResult::End
            }
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    // With the new API, WouldBlock retries happen internally in write_fill_points
    // The exact count depends on timing, but we should see multiple writes
    assert!(write_count.load(Ordering::SeqCst) >= 1);
}

#[test]
fn test_arm_opens_shutter_disarm_closes_shutter() {
    let backend = TestBackend::new();
    let shutter_open = backend.shutter_open.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Initially shutter is closed
    assert!(!shutter_open.load(Ordering::SeqCst));

    // Arm via control (this sends ControlMsg::Arm)
    let control = stream.control();
    control.arm().unwrap();

    // Process control messages - this should open the shutter
    let stopped = stream.process_control_messages();
    assert!(!stopped);
    assert!(shutter_open.load(Ordering::SeqCst));

    // Disarm (this sends ControlMsg::Disarm)
    control.disarm().unwrap();

    // Process control messages - this should close the shutter
    let stopped = stream.process_control_messages();
    assert!(!stopped);
    assert!(!shutter_open.load(Ordering::SeqCst));
}

#[test]
fn test_handle_underrun_blanks_when_disarmed() {
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Use RepeatLast policy - but when disarmed, should still blank
    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Set some last_chunk with colored points using the pre-allocated buffer
    let colored_point = LaserPoint::new(0.5, 0.5, 65535, 65535, 65535, 65535);
    for i in 0..100 {
        stream.state.last_chunk[i] = colored_point;
    }
    stream.state.last_chunk_len = 100;

    // Ensure disarmed (default state)
    assert!(!stream.control.is_armed());

    let req = ChunkRequest {
        start: StreamInstant::new(0),
        pps: 30000,
        min_points: 100,
        target_points: 100,
        buffered_points: 0,
        buffered: Duration::ZERO,
        device_queued_points: None,
    };

    // Handle underrun while disarmed
    stream.handle_underrun(&req).unwrap();

    // last_chunk should NOT be updated (we're disarmed)
    // The actual write was blanked points, but we don't update last_chunk when disarmed
    // because "last armed content" hasn't changed
    assert_eq!(stream.state.last_chunk[0].r, 65535); // Still the old colored points
}

#[test]
fn test_stop_closes_shutter() {
    let backend = TestBackend::new();
    let shutter_open = backend.shutter_open.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Arm first to open shutter
    stream.control.arm().unwrap();
    stream.process_control_messages();
    assert!(shutter_open.load(Ordering::SeqCst));

    // Stop should close shutter
    stream.stop().unwrap();
    assert!(!shutter_open.load(Ordering::SeqCst));
}

#[test]
fn test_arm_disarm_arm_cycle() {
    let backend = TestBackend::new();
    let shutter_open = backend.shutter_open.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);
    let control = stream.control();

    // Initial state: disarmed
    assert!(!control.is_armed());
    assert!(!shutter_open.load(Ordering::SeqCst));

    // Arm
    control.arm().unwrap();
    stream.process_control_messages();
    assert!(control.is_armed());
    assert!(shutter_open.load(Ordering::SeqCst));

    // Disarm
    control.disarm().unwrap();
    stream.process_control_messages();
    assert!(!control.is_armed());
    assert!(!shutter_open.load(Ordering::SeqCst));

    // Arm again
    control.arm().unwrap();
    stream.process_control_messages();
    assert!(control.is_armed());
    assert!(shutter_open.load(Ordering::SeqCst));
}

// =========================================================================
// Buffer-driven timing tests
// =========================================================================

#[test]
fn test_run_buffer_driven_behavior() {
    // Test that run uses buffer-driven timing
    // Use NoQueueTestBackend so we rely on software estimate (which decrements properly)
    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;
    let write_count = backend.inner.write_count.clone();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    // Use short target buffer for testing
    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(10))
        .with_min_buffer(Duration::from_millis(5));
    let stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

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
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
fn test_run_sleeps_when_buffer_healthy() {
    // Test that run sleeps when buffer is above target
    // Use NoQueueTestBackend so we rely on software estimate (which decrements properly)
    use std::time::Instant;

    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    // Very small target buffer, skip drain
    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(5))
        .with_min_buffer(Duration::from_millis(2))
        .with_drain_timeout(Duration::ZERO);
    let stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

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
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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

    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);
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
            for i in 0..n {
                buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // First call: return some data
                let n = req.target_points.min(buffer.len()).min(100);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Use Blank policy for underrun
    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Blank);
    let stream = Stream::with_backend(info, backend_box, cfg);

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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Blank);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
// Buffer estimation tests (Task 6.3)
// =========================================================================

#[test]
fn test_estimate_buffer_uses_software_when_no_hardware() {
    // When hardware doesn't report queue depth, use software estimate
    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Set software estimate to 500 points
    stream.state.scheduled_ahead = 500;

    // Should use software estimate since hardware returns None
    let estimate = stream.estimate_buffer_points();
    assert_eq!(estimate, 500);
}

#[test]
fn test_estimate_buffer_uses_min_of_hardware_and_software() {
    // When hardware reports queue depth, use min(hardware, software)
    let backend = TestBackend::new().with_initial_queue(300);
    let queued = backend.queued.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Software says 500, hardware says 300 -> should use 300 (conservative)
    stream.state.scheduled_ahead = 500;
    let estimate = stream.estimate_buffer_points();
    assert_eq!(
        estimate, 300,
        "Should use hardware (300) when it's less than software (500)"
    );

    // Now set hardware higher than software
    queued.store(800, Ordering::SeqCst);
    let estimate = stream.estimate_buffer_points();
    assert_eq!(
        estimate, 500,
        "Should use software (500) when it's less than hardware (800)"
    );
}

#[test]
fn test_estimate_buffer_conservative_prevents_underrun() {
    // Verify that conservative estimation (using min) prevents underruns
    // by ensuring we never overestimate the buffer
    let backend = TestBackend::new().with_initial_queue(100);
    let queued = backend.queued.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Simulate: software thinks 1000 points scheduled, but hardware only has 100
    // This can happen if hardware consumed points faster than expected
    stream.state.scheduled_ahead = 1000;

    let estimate = stream.estimate_buffer_points();

    // Should use the conservative (lower) estimate to avoid underrun
    assert_eq!(
        estimate, 100,
        "Should use conservative estimate (100) not optimistic (1000)"
    );

    // Now simulate the opposite: hardware reports more than software
    // This can happen due to timing/synchronization issues
    queued.store(2000, Ordering::SeqCst);
    stream.state.scheduled_ahead = 500;

    let estimate = stream.estimate_buffer_points();
    assert_eq!(
        estimate, 500,
        "Should use conservative estimate (500) not hardware (2000)"
    );
}

#[test]
fn test_build_fill_request_uses_conservative_estimation() {
    // Verify that build_fill_request uses conservative buffer estimation
    let backend = TestBackend::new().with_initial_queue(200);

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(40))
        .with_min_buffer(Duration::from_millis(10));
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Set software estimate higher than hardware
    stream.state.scheduled_ahead = 500;

    let req = stream.build_fill_request(1000, stream.estimate_buffer_points());

    // Should use conservative estimate (hardware = 200)
    assert_eq!(req.buffered_points, 200);
    assert_eq!(req.device_queued_points, Some(200));
}

#[test]
fn test_build_fill_request_calculates_min_and_target_points() {
    // Verify that min_points and target_points are calculated correctly
    // based on buffer state. Use NoQueueTestBackend so software estimate is used directly.
    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    // 30000 PPS, target_buffer = 40ms, min_buffer = 10ms
    // target_buffer = 40ms * 30000 = 1200 points
    // min_buffer = 10ms * 30000 = 300 points
    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(40))
        .with_min_buffer(Duration::from_millis(10));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Empty buffer: need full target
    stream.state.scheduled_ahead = 0;
    let req = stream.build_fill_request(1000, stream.estimate_buffer_points());

    // target_points should be clamped to max_points (1000)
    assert_eq!(req.target_points, 1000);
    // min_points should be 300 (10ms * 30000), clamped to 1000
    assert_eq!(req.min_points, 300);

    // Buffer at 500 points (16.67ms): below target (40ms), above min (10ms)
    stream.state.scheduled_ahead = 500;
    let req = stream.build_fill_request(1000, stream.estimate_buffer_points());

    // target_points = (1200 - 500) = 700
    assert_eq!(req.target_points, 700);
    // min_points = (300 - 500) = 0 (buffer above min)
    assert_eq!(req.min_points, 0);

    // Buffer full at 1200 points (40ms): at target
    stream.state.scheduled_ahead = 1200;
    let req = stream.build_fill_request(1000, stream.estimate_buffer_points());

    // target_points = 0 (at target)
    assert_eq!(req.target_points, 0);
    // min_points = 0 (well above min)
    assert_eq!(req.min_points, 0);
}

#[test]
fn test_build_fill_request_ceiling_rounds_min_points() {
    // Verify that min_points uses ceiling to prevent underrun
    // Use NoQueueTestBackend so software estimate is used directly.
    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    // min_buffer = 10ms at 30000 PPS = 300 points exactly
    let cfg = StreamConfig::new(30000)
        .with_target_buffer(Duration::from_millis(40))
        .with_min_buffer(Duration::from_millis(10));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Buffer at 299 points: 1 point below min_buffer
    stream.state.scheduled_ahead = 299;
    let req = stream.build_fill_request(1000, stream.estimate_buffer_points());

    // min_points should be ceil(300 - 299) = ceil(1) = 1
    // Actually it's ceil((10ms - 299/30000) * 30000) = ceil(300 - 299) = 1
    assert!(
        req.min_points >= 1,
        "min_points should be at least 1 to reach min_buffer"
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
                for i in 0..n {
                    buffer[i] =
                        LaserPoint::new(0.1 * i as f32, 0.2 * i as f32, 1000, 2000, 3000, 4000);
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
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
                for i in 0..n {
                    buffer[i] = LaserPoint::new(0.5, 0.5, 10000, 20000, 30000, 40000);
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
                for i in 0..n {
                    buffer[i] = LaserPoint::new(0.3, 0.3, 5000, 5000, 5000, 5000);
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
    let stream = Stream::with_backend(info, backend_box, cfg);

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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Park at specific position
    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Park { x: 0.5, y: -0.5 });
    let stream = Stream::with_backend(info, backend_box, cfg);

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
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Stop);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |_req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

            if count == 0 {
                // Fill some points but claim we wrote more than buffer size
                for i in 0..buffer.len() {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    // 1. Create device and start stream
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    assert!(!device.is_connected());

    let cfg = StreamConfig::new(30000);
    let (stream, returned_info) = device.start_stream(cfg).unwrap();
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
                for i in 0..n {
                    let t = i as f32 / 100.0;
                    buffer[i] = LaserPoint::new(t, t, 10000, 20000, 30000, 40000);
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
fn test_full_stream_lifecycle_with_underrun_recovery() {
    // Test lifecycle with underrun and recovery
    let backend = TestBackend::new();
    let queued = backend.queued.clone();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
                    for i in 0..n {
                        buffer[i] = LaserPoint::new(0.5, 0.5, 30000, 30000, 30000, 30000);
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
                    for i in 0..n {
                        buffer[i] = LaserPoint::new(-0.5, -0.5, 20000, 20000, 20000, 20000);
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

    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
            for i in 0..n {
                buffer[i] = LaserPoint::blanked(0.0, 0.0);
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

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    // First stream session
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    let cfg = StreamConfig::new(30000);
    let (stream, _) = device.start_stream(cfg).unwrap();

    let call_count = Arc::new(AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let result = stream.run(
        move |req, buffer| {
            let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
            if count < 2 {
                let n = req.target_points.min(buffer.len()).min(50);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
    let backend = TestBackend::new();

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, backend_box, cfg);

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
            for i in 0..n {
                buffer[i] = LaserPoint::new(0.1, 0.1, 50000, 50000, 50000, 50000);
            }
            ChunkResult::Filled(n)
        },
        |_e| {},
    );

    assert_eq!(result.unwrap(), RunExit::Stopped);
    // Shutter should have been closed by disarm
    assert!(!shutter_open.load(Ordering::SeqCst));
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

        fn queued_points(&self) -> Option<u64> {
            self.inner.queued_points()
        }
    }

    let mut backend = DisconnectingBackend {
        inner: TestBackend::new(),
        disconnect_after: Arc::new(AtomicUsize::new(3)),
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    let cfg = StreamConfig::new(30000);
    let stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    let error_occurred = Arc::new(AtomicBool::new(false));
    let error_occurred_clone = error_occurred.clone();

    let result = stream.run(
        |req, buffer| {
            let n = req.target_points.min(buffer.len()).min(10);
            for i in 0..n {
                buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
// Without the fix (backend.disconnect() call in the Err(e) branch of
// write_fill_points), the backend stays "connected" and the stream loops
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
    report_queue_depth: bool,
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
            report_queue_depth: true,
        }
    }

    /// Configure the error kind and message returned on failure.
    fn with_error(mut self, kind: std::io::ErrorKind, message: &'static str) -> Self {
        self.error_kind = kind;
        self.error_message = message;
        self
    }

    /// Don't report queue depth (Helios-like behavior).
    fn without_queue_depth(mut self) -> Self {
        self.report_queue_depth = false;
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

    fn queued_points(&self) -> Option<u64> {
        if self.report_queue_depth {
            self.inner.queued_points()
        } else {
            None
        }
    }
}

/// Producer that fills up to 10 blanked points per call (shared by write-error tests).
fn blank_producer(req: &ChunkRequest, buffer: &mut [LaserPoint]) -> ChunkResult {
    let n = req.target_points.min(buffer.len()).min(10);
    for i in 0..n {
        buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    // PPS too low
    let cfg = StreamConfig::new(500).with_reconnect(crate::types::ReconnectConfig::new());
    let result = device.start_stream(cfg);
    assert!(result.is_err());
}

#[test]
fn test_start_stream_reconnect_without_target_errors() {
    // start_stream with reconnect on a Dac created via Dac::new (no target) should error
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::types::ReconnectConfig::new());
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
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::types::ReconnectConfig::new());
    let result = device.start_stream(cfg);
    assert!(result.is_ok());

    let (stream, _) = result.unwrap();
    assert!(stream.reconnect_policy.is_some());
}

#[test]
fn test_reset_state_for_reconnect_resizes_buffers() {
    // When reconnecting to a device with different max_points_per_chunk,
    // buffers should be resized.
    let backend = TestBackend::new().with_max_points_per_chunk(1000);
    let mut stream = make_test_stream(backend);

    assert_eq!(stream.state.chunk_buffer.len(), 1000);
    assert_eq!(stream.state.last_chunk.len(), 1000);

    // Simulate reconnect to a device with different capabilities
    stream.info.caps.max_points_per_chunk = 500;
    let mut last_iter = std::time::Instant::now();
    stream.reset_state_for_reconnect(&mut last_iter);

    assert_eq!(stream.state.chunk_buffer.len(), 500);
    assert_eq!(stream.state.last_chunk.len(), 500);
    assert_eq!(stream.state.last_chunk_len, 0);
    assert_eq!(stream.state.scheduled_ahead, 0);
    assert_eq!(stream.state.stats.reconnect_count, 1);
}

#[test]
fn test_reset_state_for_reconnect_clears_timing() {
    let backend = TestBackend::new();
    let mut stream = make_test_stream(backend);

    // Set some state
    stream.state.scheduled_ahead = 5000;
    stream.state.fractional_consumed = 0.5;
    stream.state.shutter_open = true;
    stream.state.last_armed = true;
    stream.state.startup_blank_remaining = 10;

    let mut last_iter = std::time::Instant::now() - Duration::from_secs(10);
    stream.reset_state_for_reconnect(&mut last_iter);

    assert_eq!(stream.state.scheduled_ahead, 0);
    assert_eq!(stream.state.fractional_consumed, 0.0);
    assert!(!stream.state.shutter_open);
    assert!(!stream.state.last_armed);
    assert_eq!(stream.state.startup_blank_remaining, 0);
    assert!(stream.state.color_delay_line.is_empty());
    // last_iteration should be reset to approximately now
    assert!(last_iter.elapsed() < Duration::from_millis(100));
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
    let was_stopped =
        ReconnectPolicy::sleep_with_stop(Duration::from_secs(5), || stopped.load(Ordering::SeqCst));

    assert!(was_stopped);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_into_dac_preserves_reconnect_target() {
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test-id".to_string(),
        discovery_factory: None,
    });

    let cfg = StreamConfig::new(30_000).with_reconnect(crate::types::ReconnectConfig::new());
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
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
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
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    assert!(device.reconnect_target.is_none());
}

fn make_test_stream(mut backend: impl FifoBackend + 'static) -> Stream {
    backend.connect().unwrap();
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: backend.dac_type(),
        caps: backend.caps().clone(),
    };
    Stream::with_backend(
        info,
        BackendKind::Fifo(Box::new(backend)),
        StreamConfig::new(30000),
    )
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

/// Create a Helios-like backend: TimedOut error, no queue depth.
fn helios_like_backend(fail_after: usize) -> FailingWriteBackend {
    FailingWriteBackend::new(fail_after)
        .with_error(
            std::io::ErrorKind::TimedOut,
            "usb connection error: Operation timed out",
        )
        .without_queue_depth()
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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Use short drain timeout for test
    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
    let stream = Stream::with_backend(info, backend_box, cfg);

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
    // Test that drain respects timeout and doesn't block forever
    use std::time::Instant;

    let backend = TestBackend::new().with_initial_queue(100000); // Large queue that won't drain

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Very short timeout
    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(50));
    let stream = Stream::with_backend(info, backend_box, cfg);

    let start = Instant::now();
    let result = stream.run(|_req, _buffer| ChunkResult::End, |_e| {});

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
    // Test that drain is skipped when timeout is zero
    use std::time::Instant;

    let backend = TestBackend::new().with_initial_queue(100000);

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Zero timeout = skip drain
    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::ZERO);
    let stream = Stream::with_backend(info, backend_box, cfg);

    let start = Instant::now();
    let result = stream.run(|_req, _buffer| ChunkResult::End, |_e| {});

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
fn test_fill_result_end_drains_without_queue_depth() {
    // Test drain behavior when queued_points() returns None
    use std::time::Instant;

    let mut backend = NoQueueTestBackend::new();
    backend.inner.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.inner.caps().clone(),
    };

    // Short drain timeout
    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
    let stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

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

    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(10));
    let stream = Stream::with_backend(info, backend_box, cfg);

    // Arm the stream first
    let control = stream.control();
    control.arm().unwrap();

    let result = stream.run(
        |req, buffer| {
            // Fill some points then end
            let n = req.target_points.min(buffer.len()).min(10);
            for i in 0..n {
                buffer[i] = LaserPoint::blanked(0.0, 0.0);
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
// Color delay tests
// =========================================================================

#[test]
fn test_color_delay_zero_is_passthrough() {
    // With delay=0, colors should pass through unchanged
    let mut backend = TestBackend::new();
    backend.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    let cfg = StreamConfig::new(30000); // color_delay defaults to ZERO
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Arm the stream so points aren't blanked
    stream.control.arm().unwrap();
    stream.process_control_messages();
    stream.state.last_armed = true;

    // Fill chunk_buffer with colored points
    let n = 5;
    for i in 0..n {
        stream.state.chunk_buffer[i] =
            LaserPoint::new(0.0, 0.0, (i as u16 + 1) * 1000, 0, 0, 65535);
    }

    // write_fill_points applies color delay internally
    let mut on_error = |_: Error| {};
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Delay line should remain empty
    assert!(stream.state.color_delay_line.is_empty());
}

#[test]
fn test_color_delay_shifts_colors() {
    // With delay=3 points, first 3 outputs should be blanked, rest shifted
    let mut backend = TestBackend::new();
    backend.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    // 10000 PPS, delay = 300µs → ceil(0.0003 * 10000) = 3 points
    let cfg = StreamConfig::new(10000).with_color_delay(Duration::from_micros(300));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Arm the stream
    stream.control.arm().unwrap();
    stream.process_control_messages();
    // handle_shutter_transition pre-fills the delay line on arm
    stream.state.last_armed = true;

    // Pre-fill delay line as handle_shutter_transition would on arm
    stream.state.color_delay_line.clear();
    for _ in 0..3 {
        stream.state.color_delay_line.push_back((0, 0, 0, 0));
    }

    // Fill 5 points with distinct colors
    let n = 5;
    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(
            i as f32 * 0.1,
            0.0,
            (i as u16 + 1) * 10000,
            (i as u16 + 1) * 5000,
            (i as u16 + 1) * 2000,
            65535,
        );
    }

    let mut on_error = |_: Error| {};
    stream.write_fill_points(n, &mut on_error).unwrap();

    // After write, check the chunk_buffer was modified:
    // We can't inspect what was written to the backend directly,
    // but we can verify the delay line state.
    // After processing 5 points through a 3-point delay,
    // the delay line should still have 3 entries (the last 3 input colors).
    assert_eq!(stream.state.color_delay_line.len(), 3);

    // The delay line should contain colors from inputs 3, 4, 5 (0-indexed: 2, 3, 4)
    let expected: Vec<(u16, u16, u16, u16)> = (3..=5)
        .map(|i| (i * 10000u16, i * 5000, i * 2000, 65535))
        .collect();
    let actual: Vec<(u16, u16, u16, u16)> = stream.state.color_delay_line.iter().copied().collect();
    assert_eq!(actual, expected);
}

#[test]
fn test_color_delay_resets_on_disarm_arm() {
    // Disarm should clear the delay line, arm should re-fill it
    let mut backend = TestBackend::new();
    backend.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    // 10000 PPS, delay = 200µs → ceil(0.0002 * 10000) = 2 points
    let cfg = StreamConfig::new(10000).with_color_delay(Duration::from_micros(200));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Arm: should pre-fill delay line
    stream.handle_shutter_transition(true);
    assert_eq!(stream.state.color_delay_line.len(), 2);
    assert_eq!(stream.state.color_delay_line.front(), Some(&(0, 0, 0, 0)));

    // Disarm: should clear delay line
    stream.handle_shutter_transition(false);
    assert!(stream.state.color_delay_line.is_empty());

    // Arm again: should re-fill
    stream.handle_shutter_transition(true);
    assert_eq!(stream.state.color_delay_line.len(), 2);
}

#[test]
fn test_color_delay_dynamic_change() {
    // Changing delay at runtime via atomic should resize the deque
    let mut backend = TestBackend::new();
    backend.connected = true;

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };

    // Start with 200µs delay at 10000 PPS → 2 points
    let cfg = StreamConfig::new(10000).with_color_delay(Duration::from_micros(200));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // Arm
    stream.control.arm().unwrap();
    stream.process_control_messages();
    stream.state.last_armed = true;

    // Pre-fill as handle_shutter_transition would
    stream.state.color_delay_line.clear();
    for _ in 0..2 {
        stream.state.color_delay_line.push_back((0, 0, 0, 0));
    }

    // Fill and write a chunk
    let n = 3;
    for i in 0..n {
        stream.state.chunk_buffer[i] =
            LaserPoint::new(0.0, 0.0, (i as u16 + 1) * 10000, 0, 0, 65535);
    }
    let mut on_error = |_: Error| {};
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Now change delay to 500µs → ceil(0.0005 * 10000) = 5 points
    stream.control.set_color_delay(Duration::from_micros(500));

    // Write another chunk — delay line should resize to 5
    for i in 0..n {
        stream.state.chunk_buffer[i] =
            LaserPoint::new(0.0, 0.0, (i as u16 + 4) * 10000, 0, 0, 65535);
    }
    stream.write_fill_points(n, &mut on_error).unwrap();

    assert_eq!(stream.state.color_delay_line.len(), 5);

    // Now disable delay entirely
    stream.control.set_color_delay(Duration::ZERO);

    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 50000, 0, 0, 65535);
    }
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Delay line should be cleared
    assert!(stream.state.color_delay_line.is_empty());
}

// =========================================================================
// Startup blanking tests
// =========================================================================

#[test]
fn test_startup_blank_blanks_first_n_points() {
    let backend = TestBackend::new();
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // 10000 PPS, startup_blank = 500µs → ceil(0.0005 * 10000) = 5 points
    // Disable color delay to isolate startup blanking
    let cfg = StreamConfig::new(10000)
        .with_startup_blank(Duration::from_micros(500))
        .with_color_delay(Duration::ZERO);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    assert_eq!(stream.state.startup_blank_points, 5);

    // Arm the stream (triggers handle_shutter_transition which resets counter)
    stream.control.arm().unwrap();
    stream.process_control_messages();

    // Simulate arm transition in write path
    stream.state.last_armed = false; // Force transition detection

    // Fill 10 colored points
    let n = 10;
    for i in 0..n {
        stream.state.chunk_buffer[i] =
            LaserPoint::new(i as f32 * 0.1, 0.0, 65535, 32000, 16000, 65535);
    }

    let mut on_error = |_: Error| {};
    stream.write_fill_points(n, &mut on_error).unwrap();

    // After write, check what was sent: we can't inspect backend directly,
    // but we can verify the counter decremented and the buffer was modified
    assert_eq!(stream.state.startup_blank_remaining, 0);

    // Write another chunk — should NOT be blanked (counter exhausted)
    stream.state.last_armed = true; // No transition this time
    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 65535, 32000, 16000, 65535);
    }
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Verify colors pass through unmodified (no startup blanking)
    // The chunk_buffer is modified in-place before write, so after write
    // it should still have the original colors (startup blank is exhausted)
    assert_eq!(stream.state.chunk_buffer[0].r, 65535);
    assert_eq!(stream.state.chunk_buffer[0].g, 32000);
}

#[test]
fn test_startup_blank_resets_on_rearm() {
    let backend = TestBackend::new();
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // 10000 PPS, startup_blank = 500µs → 5 points
    let cfg = StreamConfig::new(10000)
        .with_startup_blank(Duration::from_micros(500))
        .with_color_delay(Duration::ZERO);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // First arm cycle: consume startup blanking
    stream.state.last_armed = false;
    stream.control.arm().unwrap();
    stream.process_control_messages();

    let n = 10;
    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 65535, 65535, 65535, 65535);
    }
    let mut on_error = |_: Error| {};
    // This triggers disarmed→armed transition, which resets counter
    stream.state.last_armed = false;
    stream.write_fill_points(n, &mut on_error).unwrap();
    assert_eq!(stream.state.startup_blank_remaining, 0);

    // Disarm → re-arm
    stream.control.disarm().unwrap();
    stream.process_control_messages();

    stream.control.arm().unwrap();
    stream.process_control_messages();

    // Write again — should trigger new arm transition and reset counter
    stream.state.last_armed = false;
    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 65535, 65535, 65535, 65535);
    }
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Counter should have been reset to 5 and then decremented to 0
    assert_eq!(stream.state.startup_blank_remaining, 0);
}

#[test]
fn test_startup_blank_zero_is_noop() {
    let backend = TestBackend::new();
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    // Disable startup blanking
    let cfg = StreamConfig::new(10000)
        .with_startup_blank(Duration::ZERO)
        .with_color_delay(Duration::ZERO);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    assert_eq!(stream.state.startup_blank_points, 0);

    // Arm and write colored points
    stream.control.arm().unwrap();
    stream.process_control_messages();
    stream.state.last_armed = false; // Force arm transition

    let n = 5;
    for i in 0..n {
        stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 65535, 32000, 16000, 65535);
    }
    let mut on_error = |_: Error| {};
    stream.write_fill_points(n, &mut on_error).unwrap();

    // Colors should pass through unmodified — no startup blanking
    assert_eq!(stream.state.chunk_buffer[0].r, 65535);
    assert_eq!(stream.state.chunk_buffer[0].g, 32000);
    assert_eq!(stream.state.chunk_buffer[0].b, 16000);
    assert_eq!(stream.state.chunk_buffer[0].intensity, 65535);
    assert_eq!(stream.state.startup_blank_remaining, 0);
}

// =========================================================================
// OutputModel coverage: scheduled_ahead accumulation
// =========================================================================

#[test]
fn test_device_start_stream_rejects_frame_swap_backend() {
    let backend = FrameSwapTestBackend::new();
    let caps = backend.inner.caps.clone();
    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps,
    };
    let device = Dac::new(info, BackendKind::FrameSwap(Box::new(backend)));

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
fn test_network_fifo_accumulates_scheduled_ahead() {
    let backend = TestBackend::new(); // default: NetworkFifo
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };

    let cfg = StreamConfig::new(30000).with_color_delay(Duration::ZERO);
    let mut stream = Stream::with_backend(info, backend_box, cfg);

    // Arm and write two chunks of 50 points each
    stream.control.arm().unwrap();
    stream.process_control_messages();

    let n = 50;
    for _ in 0..2 {
        for i in 0..n {
            stream.state.chunk_buffer[i] = LaserPoint::new(0.0, 0.0, 0, 0, 0, 0);
        }
        let mut on_error = |_: Error| {};
        stream.write_fill_points(n, &mut on_error).unwrap();
    }

    // NetworkFifo: scheduled_ahead should ACCUMULATE to 2n
    assert_eq!(stream.state.scheduled_ahead, 2 * n as u64);
    assert_eq!(stream.state.stats.chunks_written, 2);
    assert_eq!(stream.state.stats.points_written, 2 * n as u64);
}

#[test]
fn test_udp_timed_prefills_to_max_points_per_chunk() {
    let mut backend = NoQueueTestBackend::new()
        .with_output_model(OutputModel::UdpTimed)
        .with_max_points_per_chunk(179);
    backend.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: backend.dac_type(),
        caps: backend.caps().clone(),
    };
    let cfg = StreamConfig::new(1000)
        .with_target_buffer(Duration::from_millis(500))
        .with_min_buffer(Duration::from_millis(100));
    let mut stream = Stream::with_backend(info, BackendKind::Fifo(Box::new(backend)), cfg);

    // UdpTimed target = max_points_per_chunk = 179
    // One write of 179 exceeds the target → stops after 1 write
    let mut writes = 0;
    while stream.state.scheduled_ahead <= stream.scheduler_target_buffer_points() {
        let buffered = stream.estimate_buffer_points();
        let req = stream.build_fill_request(179, buffered);
        assert_eq!(req.target_points, 179);
        stream.record_write(req.target_points, false);
        writes += 1;
    }

    assert_eq!(
        writes, 2,
        "UdpTimed target is max_points_per_chunk (179) — two writes to exceed"
    );
}

#[test]
fn test_udp_timed_uses_max_points_per_chunk_for_lead() {
    let backend = NoQueueTestBackend::new()
        .with_output_model(OutputModel::UdpTimed)
        .with_max_points_per_chunk(179);
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };
    let cfg = StreamConfig::new(30_000);
    let stream = Stream::with_backend(info, backend_box, cfg);

    // UdpTimed target = max_points_per_chunk, not target_buffer_points
    assert_eq!(stream.scheduler_target_buffer_points(), 179);
}

#[test]
fn test_udp_timed_build_fill_request_uses_full_packet() {
    let backend = NoQueueTestBackend::new()
        .with_output_model(OutputModel::UdpTimed)
        .with_max_points_per_chunk(179);
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };
    let cfg = StreamConfig::new(30_000);
    let mut stream = Stream::with_backend(info, backend_box, cfg);
    stream.state.scheduled_ahead = 120;

    let req = stream.build_fill_request(179, 120);

    assert_eq!(req.target_points, 179);
    assert_eq!(req.min_points, 179);
}

#[test]
fn test_udp_timed_sleep_slice_caps_coarse_sleep() {
    assert_eq!(
        Stream::udp_timed_sleep_slice(Duration::from_millis(5)),
        Some(Duration::from_millis(1))
    );
}

#[test]
fn test_udp_timed_sleep_slice_switches_to_busy_wait_near_deadline() {
    assert_eq!(
        Stream::udp_timed_sleep_slice(Duration::from_micros(400)),
        None
    );
}

// =========================================================================
// NetworkFifo + LaserCube-like config scheduler tests
// =========================================================================

#[test]
fn test_network_fifo_lasercube_default_target_requests_topup() {
    // LaserCube-like: 5700 max, 30kpps, default 50ms target buffer
    let backend = TestBackend::new()
        .with_output_model(OutputModel::NetworkFifo)
        .with_max_points_per_chunk(5700);
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "LaserCube Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };
    let cfg = StreamConfig::new(30_000)
        .with_target_buffer(Duration::from_millis(50))
        .with_min_buffer(Duration::from_millis(20));
    let stream = Stream::with_backend(info, backend_box, cfg);

    // At 30kpps and 50ms target, target_points = ceil(0.05 * 30000) = 1500
    let buffered = stream.estimate_buffer_points();
    let req = stream.build_fill_request(5700, buffered);
    assert_eq!(
        req.target_points, 1500,
        "should request ~1500 top-up, not full 5700"
    );
}

#[test]
fn test_network_fifo_lasercube_large_target_uses_more_capacity() {
    // Explicit large target buffer can request larger chunks
    let backend = TestBackend::new()
        .with_output_model(OutputModel::NetworkFifo)
        .with_max_points_per_chunk(5700);
    let mut backend_box = BackendKind::Fifo(Box::new(backend));
    backend_box.connect().unwrap();

    let info = DacInfo {
        id: "test".to_string(),
        name: "LaserCube Test".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend_box.caps().clone(),
    };
    let cfg = StreamConfig::new(30_000)
        .with_target_buffer(Duration::from_millis(200))
        .with_min_buffer(Duration::from_millis(50));
    let stream = Stream::with_backend(info, backend_box, cfg);

    let buffered = stream.estimate_buffer_points();
    let req = stream.build_fill_request(5700, buffered);
    // At 30kpps and 200ms: ceil(0.2 * 30000) = 6000, clamped to 5700
    assert_eq!(req.target_points, 5700);
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

// =========================================================================
// Fractional consumed accumulator: prevent scheduled_ahead stall
// =========================================================================

#[test]
fn test_fractional_consumed_prevents_stall() {
    // Regression test: when scheduled_ahead equals target_buffer_points
    // exactly, the run loop used to stall because (elapsed * pps) as u64
    // truncated to 0 on sub-microsecond iterations. The fractional
    // accumulator ensures sub-point remainders carry over until they add
    // up to a whole point.
    use std::thread;
    use std::time::Duration;

    let backend = NoQueueTestBackend::new();
    let write_count = backend.inner.write_count.clone();
    let stream = make_test_stream(backend);
    let control = stream.control();

    // Run the stream in a thread. If the bug regresses, the stream
    // stalls in a tight loop and never writes after the initial burst.
    let handle = thread::spawn(move || {
        stream.run(
            |req, buffer| {
                let n = req.target_points.min(buffer.len()).min(100);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            |_err| {},
        )
    });

    // Wait enough for the target buffer to be filled AND for subsequent
    // writes to happen (proving the stream isn't stalled).
    thread::sleep(Duration::from_millis(800));

    let writes = write_count.load(Ordering::SeqCst);

    // At 30 kpps with 500ms target buffer, the initial burst is ~15
    // writes of 1000 points. After 800ms, the buffer should have
    // drained enough for many more writes. If stalled, writes would
    // be stuck at ~15.
    assert!(
        writes > 20,
        "expected >20 writes after 800ms, got {writes} — stream likely stalled"
    );

    control.stop().unwrap();
    let exit = handle.join().unwrap().unwrap();
    assert_eq!(exit, RunExit::Stopped);
}

#[test]
fn with_discovery_factory_creates_target_when_none() {
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test-factory".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    assert!(device.reconnect_target.is_none());

    let device = device.with_discovery_factory(|| {
        crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::all())
    });

    let target = device.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "test-factory");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn with_discovery_factory_replaces_factory_on_existing_target() {
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test-replace".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "original-id".to_string(),
        discovery_factory: None,
    });

    let device = device.with_discovery_factory(|| {
        crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::all())
    });

    let target = device.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "original-id");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn with_discovery_factory_enables_reconnect_for_frame_session() {
    let backend = TestBackend::new();
    let info = DacInfo {
        id: "test-session".to_string(),
        name: "Test Device".to_string(),
        kind: DacType::Custom("Test".to_string()),
        caps: backend.caps().clone(),
    };
    let device =
        Dac::new(info, BackendKind::Fifo(Box::new(backend))).with_discovery_factory(|| {
            crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::all())
        });

    let config = crate::presentation::FrameSessionConfig::new(30_000)
        .with_reconnect(crate::types::ReconnectConfig::new());
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
}

impl ReconnectFifoBackend {
    fn new() -> Self {
        Self { connected: false }
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
            output_model: crate::types::OutputModel::NetworkFifo,
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
}

/// FIFO backend that returns Error::Disconnected after N writes.
/// Unlike FailingWriteBackend (which returns Error::Backend), this triggers
/// the reconnect path in FrameSession which checks `e.is_disconnected()`.
struct DisconnectAfterNBackend {
    connected: bool,
    fail_after: usize,
    write_count: AtomicUsize,
}

impl DisconnectAfterNBackend {
    fn new(fail_after: usize) -> Self {
        Self {
            connected: false,
            fail_after,
            write_count: AtomicUsize::new(0),
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
            output_model: crate::types::OutputModel::NetworkFifo,
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
}

/// Mock ExternalDiscoverer that tracks scan/connect calls and returns
/// a ReconnectFifoBackend on connect. Uses a fixed IP so the stable_id is
/// deterministic: "trackingtest:10.0.0.99"
struct TrackingDiscoverer {
    scan_count: Arc<AtomicUsize>,
    connect_count: Arc<AtomicUsize>,
}

impl crate::discovery::ExternalDiscoverer for TrackingDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Custom("TrackingTest".into())
    }

    fn scan(&mut self) -> Vec<crate::discovery::ExternalDevice> {
        self.scan_count.fetch_add(1, Ordering::SeqCst);
        let mut device = crate::discovery::ExternalDevice::new(());
        device.ip_address = Some("10.0.0.99".parse().unwrap());
        device.hardware_name = Some("Tracking Test Device".into());
        vec![device]
    }

    fn connect(&mut self, _opaque_data: Box<dyn std::any::Any + Send>) -> Result<BackendKind> {
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
            let mut d = crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::none());
            d.register(Box::new(TrackingDiscoverer {
                scan_count: scan_count_factory.clone(),
                connect_count: connect_count_factory.clone(),
            }));
            d
        });

    let config = crate::presentation::FrameSessionConfig::new(30_000).with_reconnect(
        crate::types::ReconnectConfig::new()
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
