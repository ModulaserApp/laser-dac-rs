use super::*;

fn make_point(x: f32, y: f32) -> LaserPoint {
    LaserPoint::new(x, y, 65535, 0, 0, 65535)
}

// =========================================================================
// Frame tests
// =========================================================================

#[test]
fn test_authored_frame_new_and_points() {
    let pts = vec![make_point(0.0, 0.0), make_point(1.0, 1.0)];
    let frame = Frame::new(pts.clone());
    assert_eq!(frame.points().len(), 2);
    assert_eq!(frame.points()[0].x, 0.0);
    assert_eq!(frame.points()[1].x, 1.0);
}

#[test]
fn test_authored_frame_first_last_point() {
    let frame = Frame::new(vec![
        make_point(-1.0, -1.0),
        make_point(0.0, 0.0),
        make_point(1.0, 1.0),
    ]);
    assert_eq!(frame.first_point().unwrap().x, -1.0);
    assert_eq!(frame.last_point().unwrap().x, 1.0);
}

#[test]
fn test_authored_frame_empty() {
    let frame = Frame::new(vec![]);
    assert!(frame.is_empty());
    assert_eq!(frame.len(), 0);
    assert!(frame.first_point().is_none());
    assert!(frame.last_point().is_none());
}

#[test]
fn test_authored_frame_len() {
    let frame = Frame::new(vec![make_point(0.0, 0.0); 42]);
    assert_eq!(frame.len(), 42);
    assert!(!frame.is_empty());
}

#[test]
fn test_authored_frame_from_vec() {
    let pts = vec![make_point(0.5, 0.5)];
    let frame: Frame = pts.into();
    assert_eq!(frame.len(), 1);
    assert_eq!(frame.points()[0].x, 0.5);
}

#[test]
fn test_authored_frame_clone_shares_data() {
    let frame = Frame::new(vec![make_point(0.0, 0.0)]);
    let clone = frame.clone();
    // Both should point to the same Arc data
    assert_eq!(frame.len(), clone.len());
    assert_eq!(frame.points()[0].x, clone.points()[0].x);
}

// =========================================================================
// default_transition tests
// =========================================================================

#[test]
fn test_default_transition_scales_with_distance() {
    let near = default_transition(&make_point(0.0, 0.0), &make_point(0.005, 0.0));
    let far = default_transition(&make_point(-1.0, -1.0), &make_point(1.0, 1.0));
    let mid = default_transition(&make_point(0.0, 0.0), &make_point(1.0, 0.0));

    // Near but above threshold (0.02) still gets minimal transition
    assert!(near.is_empty(), "very near points should produce no transition, got {}", near.len());
    assert!(far.len() >= 100, "far should produce many points, got {}", far.len());
    assert!(mid.len() < far.len(), "medium should be less than far");
}

#[test]
fn test_default_transition_suppressed_for_tiny_distance() {
    // Points closer than 0.02 should produce no transition
    let result = default_transition(&make_point(0.0, 0.0), &make_point(0.01, 0.0));
    assert!(result.is_empty(), "tiny distance should produce empty transition");

    // Points just above threshold should produce transition
    let result = default_transition(&make_point(0.0, 0.0), &make_point(0.03, 0.0));
    assert!(!result.is_empty(), "above-threshold distance should produce transition");
}

#[test]
fn test_default_transition_all_blanked() {
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 1.0);
    let result = default_transition(&from, &to);
    for p in &result {
        assert_eq!(p.r, 0, "r should be 0");
        assert_eq!(p.g, 0, "g should be 0");
        assert_eq!(p.b, 0, "b should be 0");
        assert_eq!(p.intensity, 0, "intensity should be 0");
    }
}

#[test]
fn test_default_transition_has_dwell_travel_dwell_structure() {
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 0.0);
    let result = default_transition(&from, &to);

    // First points should dwell at `from`
    assert_eq!(result[0].x, 0.0, "should start dwelling at from");
    assert_eq!(result[1].x, 0.0, "should still be dwelling at from");

    // Last points should dwell at `to`
    let last = result.last().unwrap();
    assert_eq!(last.x, 1.0, "should end dwelling at to");
    let second_last = &result[result.len() - 2];
    assert_eq!(second_last.x, 1.0, "should still be dwelling at to");

    // Middle points should be between from and to
    let mid_idx = result.len() / 2;
    assert!(result[mid_idx].x > 0.0 && result[mid_idx].x < 1.0,
        "middle point should be between from and to, got {}", result[mid_idx].x);
}

#[test]
fn test_default_transition_same_point_produces_empty() {
    let p = make_point(0.5, -0.3);
    let result = default_transition(&p, &p);
    assert!(result.is_empty(), "zero distance should produce no transition");
}

// =========================================================================
// PresentationEngine tests
// =========================================================================

/// Create an engine with a transition that produces 2 blanked interpolated points.
fn make_engine() -> PresentationEngine {
    PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        vec![
            LaserPoint::blanked(from.x * 0.5 + to.x * 0.5, from.y * 0.5 + to.y * 0.5),
            LaserPoint::blanked(to.x, to.y),
        ]
    }))
}

/// Create an engine with zero transition points.
fn make_engine_no_transition() -> PresentationEngine {
    PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| vec![]))
}

fn make_frame(points: Vec<LaserPoint>) -> Arc<Frame> {
    Arc::new(Frame::new(points))
}

#[test]
fn test_engine_before_first_frame_blanks_at_origin() {
    let mut engine = make_engine();
    let mut buffer = vec![LaserPoint::default(); 10];
    let n = engine.fill_chunk(&mut buffer, 10);
    assert_eq!(n, 10);
    for p in &buffer {
        assert_eq!(p.x, 0.0);
        assert_eq!(p.y, 0.0);
        assert_eq!(p.intensity, 0);
    }
}

#[test]
fn test_engine_set_pending_promotes_when_no_current() {
    let mut engine = make_engine_no_transition();
    let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    engine.set_pending(frame);

    // Should have promoted to current
    assert!(engine.current_base.is_some());
    assert!(engine.pending_base.is_none());
}

#[test]
fn test_engine_set_pending_overwrites_existing_pending() {
    let mut engine = make_engine_no_transition();
    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(2.0, 0.0)]);
    let frame_c = make_frame(vec![make_point(3.0, 0.0)]);

    engine.set_pending(frame_a); // promotes to current
    engine.set_pending(frame_b); // pending
    engine.set_pending(frame_c); // overwrites pending

    assert!(engine.pending_base.is_some());
    assert_eq!(engine.pending_base.as_ref().unwrap().points()[0].x, 3.0);
}

#[test]
fn test_engine_fill_chunk_cycles_frame() {
    let mut engine = make_engine_no_transition();
    let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    engine.set_pending(frame);

    let mut buffer = vec![LaserPoint::default(); 6];
    let n = engine.fill_chunk(&mut buffer, 6);
    assert_eq!(n, 6);
    // Frame is [1.0, 2.0], cycles: 1.0, 2.0, 1.0, 2.0, 1.0, 2.0
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[1].x, 2.0);
    assert_eq!(buffer[2].x, 1.0);
    assert_eq!(buffer[3].x, 2.0);
}

#[test]
fn test_engine_fill_chunk_self_loop_no_transition() {
    let mut engine = make_engine(); // 2-point transition (unused on self-loop)
    let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame);

    // Self-loop: no transition points, just frame cycling: [0.0, 1.0, 0.0, 1.0, ...]
    let mut buffer = vec![LaserPoint::default(); 8];
    let n = engine.fill_chunk(&mut buffer, 8);
    assert_eq!(n, 8);

    // All points are frame points, cycling
    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].x, 1.0);
    assert_eq!(buffer[2].x, 0.0);
    assert_eq!(buffer[3].x, 1.0);
    // No blanked transition points
    assert_eq!(buffer[0].intensity, 65535);
    assert_eq!(buffer[1].intensity, 65535);
}

#[test]
fn test_engine_fill_chunk_frame_change_inserts_transition() {
    let mut engine = make_engine(); // 2-point transition
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(1.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b); // pending

    // frame_a (1 point) → transition (2 points) → frame_b cycling
    let mut buffer = vec![LaserPoint::default(); 6];
    let n = engine.fill_chunk(&mut buffer, 6);
    assert_eq!(n, 6);

    assert_eq!(buffer[0].x, 0.0);        // frame_a
    assert_eq!(buffer[1].intensity, 0);   // transition point (blanked)
    assert_eq!(buffer[2].intensity, 0);   // transition point (blanked)
    assert_eq!(buffer[3].x, 1.0);        // frame_b
    assert_eq!(buffer[4].x, 1.0);        // frame_b self-loop
}

#[test]
fn test_engine_fill_chunk_promotes_pending_at_frame_end() {
    let mut engine = make_engine_no_transition();
    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(2.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b); // pending

    // First point is frame_a, then frame_b takes over
    let mut buffer = vec![LaserPoint::default(); 4];
    let n = engine.fill_chunk(&mut buffer, 4);
    assert_eq!(n, 4);
    assert_eq!(buffer[0].x, 1.0); // frame_a
    assert_eq!(buffer[1].x, 2.0); // frame_b promoted
    assert_eq!(buffer[2].x, 2.0); // frame_b cycles
}

#[test]
fn test_engine_fill_chunk_frame_skip() {
    let mut engine = make_engine_no_transition();
    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(2.0, 0.0)]);
    let frame_c = make_frame(vec![make_point(3.0, 0.0)]);

    engine.set_pending(frame_a);
    // B is pending, then overwritten by C
    engine.set_pending(frame_b);
    engine.set_pending(frame_c);

    let mut buffer = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buffer, 4);
    // frame_a plays, then C (B was skipped)
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[1].x, 3.0);
}

#[test]
fn test_engine_fill_chunk_cursor_continuity() {
    let mut engine = make_engine_no_transition();
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]);
    engine.set_pending(frame);

    // Fill 2 points
    let mut buf1 = vec![LaserPoint::default(); 2];
    engine.fill_chunk(&mut buf1, 2);
    assert_eq!(buf1[0].x, 0.0);
    assert_eq!(buf1[1].x, 1.0);

    // Fill 2 more — should continue from where we left off
    let mut buf2 = vec![LaserPoint::default(); 2];
    engine.fill_chunk(&mut buf2, 2);
    assert_eq!(buf2[0].x, 2.0);
    // Next is wrap back to 0.0
    assert_eq!(buf2[1].x, 0.0);
}

#[test]
fn test_engine_compose_hardware_frame_self_loop() {
    let mut engine = make_engine(); // 2-point transition
    let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // Layout: 2 transition + 2 frame = 4
    assert_eq!(composed.len(), 4);
    // Transition points are blanked
    assert_eq!(composed[0].intensity, 0);
    assert_eq!(composed[1].intensity, 0);
    // Frame points
    assert_eq!(composed[2].x, 0.0);
    assert_eq!(composed[3].x, 1.0);
}

#[test]
fn test_engine_compose_hardware_frame_promotes_pending() {
    let mut engine = make_engine_no_transition();
    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(2.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b);

    let composed = engine.compose_hardware_frame();
    // Just 1 frame point (no transition from no_transition engine)
    assert_eq!(composed.len(), 1);
    // Frame content is frame_b (promoted, latest-wins)
    assert_eq!(composed[0].x, 2.0);
}

#[test]
fn test_engine_compose_hardware_frame_empty_before_first_frame() {
    let mut engine = make_engine();
    let composed = engine.compose_hardware_frame();
    assert!(composed.is_empty());
}

#[test]
fn test_engine_compose_hardware_frame_a_to_b_transition() {
    // Verify that A→B computes transition_fn(A.last, B.first), not B self-loop.
    //
    // Use a transition that records what it was called with by encoding
    // from.x and to.x into the blanked transition point positions.
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        vec![LaserPoint::blanked(from.x, to.x)]
    }));

    // Frame A: points at x=1.0 and x=2.0
    let frame_a = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A, self-loop

    // Frame B: points at x=5.0 and x=6.0
    let frame_b = make_frame(vec![make_point(5.0, 0.0), make_point(6.0, 0.0)]);
    engine.set_pending(frame_b);
    let composed = engine.compose_hardware_frame();

    // First point is the transition: from=A.last(2.0), to=B.first(5.0)
    assert_eq!(composed[0].x, 2.0, "transition 'from' should be A.last");
    assert_eq!(composed[0].y, 5.0, "transition 'to' should be B.first");
    // Then B's frame points
    assert_eq!(composed[1].x, 5.0);
    assert_eq!(composed[2].x, 6.0);
}

#[test]
fn test_engine_compose_hardware_frame_self_loop_after_transition() {
    // After A→B transition, next call without pending should produce B self-loop.
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        vec![LaserPoint::blanked(from.x, to.x)]
    }));

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let frame_b = make_frame(vec![make_point(5.0, 0.0), make_point(6.0, 0.0)]);
    engine.set_pending(frame_b);
    let _ = engine.compose_hardware_frame(); // A→B transition, promote B

    // No new pending: should be B self-loop
    let composed = engine.compose_hardware_frame();
    // Transition: from=B.last(6.0), to=B.first(5.0)
    assert_eq!(composed[0].x, 6.0, "self-loop 'from' should be B.last");
    assert_eq!(composed[0].y, 5.0, "self-loop 'to' should be B.first");
    assert_eq!(composed[1].x, 5.0);
    assert_eq!(composed[2].x, 6.0);
}

#[test]
fn test_engine_compose_hardware_frame_skip() {
    // A→C frame skip: B is overwritten before it plays.
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        vec![LaserPoint::blanked(from.x, to.x)]
    }));

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let frame_b = make_frame(vec![make_point(5.0, 0.0)]);
    engine.set_pending(frame_b);
    let frame_c = make_frame(vec![make_point(9.0, 0.0)]);
    engine.set_pending(frame_c); // overwrites B

    let composed = engine.compose_hardware_frame();
    // Transition: from=A.last(1.0), to=C.first(9.0) — B is skipped
    assert_eq!(composed[0].x, 1.0, "transition 'from' should be A.last");
    assert_eq!(composed[0].y, 9.0, "transition 'to' should be C.first (B skipped)");
    assert_eq!(composed[1].x, 9.0);
}

#[test]
fn test_engine_fill_chunk_multiple_wraps_in_single_call() {
    let mut engine = make_engine_no_transition();
    let frame = make_frame(vec![make_point(5.0, 0.0)]);
    engine.set_pending(frame);

    // Fill 10 points from a 1-point frame — should wrap 10 times
    let mut buffer = vec![LaserPoint::default(); 10];
    let n = engine.fill_chunk(&mut buffer, 10);
    assert_eq!(n, 10);
    for p in &buffer {
        assert_eq!(p.x, 5.0);
    }
}

// =========================================================================
// FrameSession tests
// =========================================================================

use crate::backend::{BackendKind, DacBackend, FifoBackend, FrameSwapBackend};
use crate::error::Result as DacResult;
use crate::types::RunExit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Minimal FIFO test backend for FrameSession tests.
struct FifoTestBackend {
    connected: bool,
    write_count: Arc<AtomicUsize>,
    points_written: Arc<AtomicUsize>,
    shutter_open: Arc<AtomicBool>,
}

impl FifoTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            points_written: Arc::new(AtomicUsize::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DacBackend for FifoTestBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("FifoTest".into())
    }
    fn caps(&self) -> &crate::types::DacCapabilities {
        static CAPS: crate::types::DacCapabilities = crate::types::DacCapabilities {
            pps_min: 1000,
            pps_max: 100000,
            max_points_per_chunk: 1000,
            output_model: crate::types::OutputModel::NetworkFifo,
        };
        &CAPS
    }
    fn connect(&mut self) -> DacResult<()> {
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) -> DacResult<()> {
        self.connected = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn stop(&mut self) -> DacResult<()> {
        Ok(())
    }
    fn set_shutter(&mut self, open: bool) -> DacResult<()> {
        self.shutter_open.store(open, Ordering::SeqCst);
        Ok(())
    }
}

impl FifoBackend for FifoTestBackend {
    fn try_write_points(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.write_count.fetch_add(1, Ordering::SeqCst);
        self.points_written
            .fetch_add(points.len(), Ordering::SeqCst);
        Ok(crate::backend::WriteOutcome::Written)
    }
}

/// Minimal FrameSwap test backend.
struct FrameSwapTestBackend {
    connected: bool,
    write_count: Arc<AtomicUsize>,
    last_frame_size: Arc<AtomicUsize>,
    shutter_open: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
}

impl FrameSwapTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            last_frame_size: Arc::new(AtomicUsize::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl DacBackend for FrameSwapTestBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("FrameSwapTest".into())
    }
    fn caps(&self) -> &crate::types::DacCapabilities {
        static CAPS: crate::types::DacCapabilities = crate::types::DacCapabilities {
            pps_min: 1000,
            pps_max: 100000,
            max_points_per_chunk: 4095,
            output_model: crate::types::OutputModel::UsbFrameSwap,
        };
        &CAPS
    }
    fn connect(&mut self) -> DacResult<()> {
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) -> DacResult<()> {
        self.connected = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    fn stop(&mut self) -> DacResult<()> {
        Ok(())
    }
    fn set_shutter(&mut self, open: bool) -> DacResult<()> {
        self.shutter_open.store(open, Ordering::SeqCst);
        Ok(())
    }
}

impl FrameSwapBackend for FrameSwapTestBackend {
    fn frame_capacity(&self) -> usize {
        4095
    }
    fn is_ready_for_frame(&mut self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    fn write_frame(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.write_count.fetch_add(1, Ordering::SeqCst);
        self.last_frame_size
            .store(points.len(), Ordering::SeqCst);
        Ok(crate::backend::WriteOutcome::Written)
    }
}

#[test]
fn test_frame_session_fifo_submit_frame_writes_points() {
    let backend = FifoTestBackend::new();
    let points_written = backend.points_written.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(30000)
        .with_startup_blank(std::time::Duration::ZERO);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
    ]));

    // Give the scheduler time to process
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        points_written.load(Ordering::SeqCst) > 0,
        "Should have written points"
    );

    session.control().stop().unwrap();
    let exit = session.join().unwrap();
    assert_eq!(exit, RunExit::Stopped);
}

#[test]
fn test_frame_session_frame_swap_writes_frames() {
    let backend = FrameSwapTestBackend::new();
    let write_count = backend.write_count.clone();
    let last_frame_size = backend.last_frame_size.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));

    let config = FrameSessionConfig::new(30000)
        .with_startup_blank(std::time::Duration::ZERO);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
    ]));

    std::thread::sleep(std::time::Duration::from_millis(50));

    assert!(
        write_count.load(Ordering::SeqCst) > 0,
        "Should have written frames"
    );
    // Frame size = 8 transition + 2 base = 10 (with default transition)
    assert!(
        last_frame_size.load(Ordering::SeqCst) > 0,
        "Frame should have points"
    );

    session.control().stop().unwrap();
    let exit = session.join().unwrap();
    assert_eq!(exit, RunExit::Stopped);
}

#[test]
fn test_frame_session_arm_disarm() {
    let backend = FifoTestBackend::new();
    let shutter = backend.shutter_open.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(30000);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    assert!(!session.control().is_armed());

    session.control().arm().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(shutter.load(Ordering::SeqCst));

    session.control().disarm().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!shutter.load(Ordering::SeqCst));

    session.control().stop().unwrap();
    session.join().unwrap();
}

#[test]
fn test_frame_session_stop() {
    let backend = FifoTestBackend::new();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(30000);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().stop().unwrap();
    let exit = session.join().unwrap();
    assert_eq!(exit, RunExit::Stopped);
}

#[test]
fn test_color_delay_line_carries_across_chunks() {
    use super::ColorDelayLine;

    let mut delay_line = ColorDelayLine::new(1);

    // First chunk: [A, B, C]
    let mut chunk1 = vec![
        LaserPoint::new(0.0, 0.0, 100, 200, 300, 400),
        LaserPoint::new(1.0, 0.0, 500, 600, 700, 800),
        LaserPoint::new(2.0, 0.0, 900, 1000, 1100, 1200),
    ];
    delay_line.apply(&mut chunk1);

    // First point blanked (no prior), rest shifted by 1
    assert_eq!(chunk1[0].r, 0);
    assert_eq!(chunk1[1].r, 100);
    assert_eq!(chunk1[2].r, 500);

    // Second chunk: [D, E] — first point should get C's colors (carried over)
    let mut chunk2 = vec![
        LaserPoint::new(3.0, 0.0, 1300, 1400, 1500, 1600),
        LaserPoint::new(4.0, 0.0, 1700, 1800, 1900, 2000),
    ];
    delay_line.apply(&mut chunk2);

    // First point gets C's original colors (900, 1000, 1100, 1200) — NOT blanked!
    assert_eq!(chunk2[0].r, 900);
    assert_eq!(chunk2[0].g, 1000);
    assert_eq!(chunk2[0].b, 1100);
    assert_eq!(chunk2[0].intensity, 1200);
    // Second point gets D's original colors
    assert_eq!(chunk2[1].r, 1300);

    // XY unchanged
    assert_eq!(chunk2[0].x, 3.0);
    assert_eq!(chunk2[1].x, 4.0);
}

#[test]
fn test_color_delay_line_zero_delay_is_noop() {
    use super::ColorDelayLine;

    let mut delay_line = ColorDelayLine::new(0);
    let mut points = vec![LaserPoint::new(0.0, 0.0, 100, 200, 300, 400)];
    delay_line.apply(&mut points);

    assert_eq!(points[0].r, 100);
    assert_eq!(points[0].g, 200);
}

#[test]
fn test_engine_empty_transition_result() {
    let mut engine = make_engine_no_transition();
    let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    engine.set_pending(frame);

    let mut buffer = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buffer, 4);
    // No transition points, just frame cycling: 1, 2, 1, 2
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[1].x, 2.0);
    assert_eq!(buffer[2].x, 1.0);
    assert_eq!(buffer[3].x, 2.0);
}

// =========================================================================
// Reconnect-related tests
// =========================================================================

#[test]
fn test_engine_reset_clears_state() {
    use super::engine::PresentationEngine;

    let mut engine = PresentationEngine::new(Box::new(default_transition));

    // Set a frame and advance cursor
    let frame = Arc::new(Frame::new(vec![
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]));
    engine.set_pending(frame);

    let mut buffer = vec![LaserPoint::default(); 3];
    engine.fill_chunk(&mut buffer, 3);

    // Engine should have state now
    assert!(engine.current_base.is_some());

    // Reset should clear everything
    engine.reset();
    assert!(engine.current_base.is_none());
    assert!(engine.pending_base.is_none());

    // After reset, fill_chunk should return blanks at origin
    let mut buffer2 = vec![LaserPoint::default(); 2];
    let n = engine.fill_chunk(&mut buffer2, 2);
    assert_eq!(n, 2);
    assert_eq!(buffer2[0].r, 0);
    assert_eq!(buffer2[0].g, 0);
}

#[test]
fn test_engine_reset_then_replay_frame() {
    use super::engine::PresentationEngine;

    let mut engine = PresentationEngine::new(Box::new(|_, _| vec![]));

    // Play a frame
    let frame = Arc::new(Frame::new(vec![make_point(1.0, 0.0)]));
    engine.set_pending(frame.clone());
    let mut buffer = vec![LaserPoint::default(); 2];
    engine.fill_chunk(&mut buffer, 2);

    // Reset then replay same frame
    engine.reset();
    engine.set_pending(frame);

    let mut buffer2 = vec![LaserPoint::default(); 2];
    let n = engine.fill_chunk(&mut buffer2, 2);
    assert_eq!(n, 2);
    assert_eq!(buffer2[0].x, 1.0);
}

#[test]
fn test_color_delay_reset_clears_carry() {
    use super::engine::ColorDelayLine;

    let mut delay = ColorDelayLine::new(3);

    // Apply some points so carry buffer has non-zero values
    let mut points = vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(0.1, 0.0, 32000, 0, 0, 32000),
        LaserPoint::new(0.2, 0.0, 16000, 0, 0, 16000),
    ];
    delay.apply(&mut points);

    // Carry buffer should now have color values from the chunk
    // Reset should clear it back to zeros
    delay.reset();

    // Apply new points — first 3 should get zero colors from the reset carry
    let mut points2 = vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(0.1, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(0.2, 0.0, 65535, 0, 0, 65535),
    ];
    delay.apply(&mut points2);

    // After reset, the first 3 points should have zero colors (from the zeroed carry)
    assert_eq!(points2[0].r, 0);
    assert_eq!(points2[0].intensity, 0);
    assert_eq!(points2[1].r, 0);
    assert_eq!(points2[2].r, 0);
}

#[test]
fn test_frame_session_config_with_reconnect() {
    let config = FrameSessionConfig::new(30_000)
        .with_reconnect(crate::types::ReconnectConfig::new().max_retries(3));

    assert!(config.reconnect.is_some());
    assert_eq!(config.reconnect.as_ref().unwrap().max_retries, Some(3));
}

#[test]
fn test_frame_session_start_frame_session_rejects_invalid_pps_with_reconnect() {
    use crate::backend::BackendKind;
    use crate::stream::Dac;
    use crate::types::{DacCapabilities, DacInfo, DacType, OutputModel};

    // Create a Dac with reconnect target
    let caps = DacCapabilities {
        pps_min: 1000,
        pps_max: 100_000,
        max_points_per_chunk: 1000,
        output_model: OutputModel::NetworkFifo,
    };

    struct MinimalBackend { caps: DacCapabilities, connected: bool }
    impl crate::backend::DacBackend for MinimalBackend {
        fn dac_type(&self) -> DacType { DacType::Custom("Test".into()) }
        fn caps(&self) -> &DacCapabilities { &self.caps }
        fn connect(&mut self) -> crate::backend::Result<()> { self.connected = true; Ok(()) }
        fn disconnect(&mut self) -> crate::backend::Result<()> { Ok(()) }
        fn is_connected(&self) -> bool { self.connected }
        fn stop(&mut self) -> crate::backend::Result<()> { Ok(()) }
        fn set_shutter(&mut self, _: bool) -> crate::backend::Result<()> { Ok(()) }
    }
    impl crate::backend::FifoBackend for MinimalBackend {
        fn try_write_points(&mut self, _: u32, _: &[LaserPoint]) -> crate::backend::Result<crate::backend::WriteOutcome> {
            Ok(crate::backend::WriteOutcome::Written)
        }
        fn queued_points(&self) -> Option<u64> { None }
    }

    let backend = MinimalBackend { caps: caps.clone(), connected: false };
    let info = DacInfo::new("test", "Test", DacType::Custom("Test".into()), caps);
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    // PPS 500 is below pps_min=1000 — should be rejected even with reconnect
    let config = FrameSessionConfig::new(500)
        .with_reconnect(crate::types::ReconnectConfig::new());
    let result = device.start_frame_session(config);
    assert!(result.is_err());
}
