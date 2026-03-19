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

/// Helper: call default transition at 30k PPS.
fn call_default_transition(from: &LaserPoint, to: &LaserPoint) -> TransitionPlan {
    let tf = default_transition(30_000);
    tf(from, to)
}

#[test]
fn test_default_transition_scales_with_distance() {
    let near = call_default_transition(&make_point(0.0, 0.0), &make_point(0.005, 0.0));
    let far = call_default_transition(&make_point(-1.0, -1.0), &make_point(1.0, 1.0));
    let mid = call_default_transition(&make_point(0.0, 0.0), &make_point(1.0, 0.0));

    let near_pts = match near {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("near should produce Transition"),
    };
    let far_pts = match far {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("far should produce Transition"),
    };
    let mid_pts = match mid {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("mid should produce Transition"),
    };

    // All distances produce blanking (end_dwell + transit + start_dwell)
    assert!(
        near_pts.len() >= 3,
        "near should still produce blanking, got {}",
        near_pts.len()
    );
    assert!(
        far_pts.len() > mid_pts.len(),
        "far should produce more points than medium"
    );
    assert!(
        mid_pts.len() > near_pts.len(),
        "medium should produce more points than near"
    );
}

#[test]
fn test_default_transition_always_blanks() {
    // Even very close points get blanking (end dwell + start dwell)
    let result = call_default_transition(&make_point(0.0, 0.0), &make_point(0.01, 0.0));
    let pts = match result {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("should produce Transition"),
    };
    assert!(
        pts.len() >= 3,
        "even tiny distance should produce blanking, got {}",
        pts.len()
    );

    // Farther points get more (more transit points)
    let result = call_default_transition(&make_point(0.0, 0.0), &make_point(1.0, 0.0));
    let pts2 = match result {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("should produce Transition"),
    };
    assert!(pts2.len() > pts.len());
}

#[test]
fn test_default_transition_all_blanked() {
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 1.0);
    let result = match call_default_transition(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };
    for p in &result {
        assert_eq!(p.r, 0, "r should be 0");
        assert_eq!(p.g, 0, "g should be 0");
        assert_eq!(p.b, 0, "b should be 0");
        assert_eq!(p.intensity, 0, "intensity should be 0");
    }
}

#[test]
fn test_default_transition_three_phase_structure() {
    // At 30k PPS: end_dwell = round(100 * 30000 / 1e6) = 3 points
    //             start_dwell = round(400 * 30000 / 1e6) = 12 points
    //             transit for d_inf=1.0: ceil(32*1.0) = 32 points
    // Total: 3 + 32 + 12 = 47
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 0.0);
    let result = match call_default_transition(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };

    assert_eq!(
        result.len(),
        47,
        "3 end_dwell + 32 transit + 12 start_dwell"
    );

    // Phase 1: end dwell — blanked at source position
    for p in &result[..3] {
        assert_eq!(p.x, 0.0, "end dwell should be at from.x");
        assert_eq!(p.intensity, 0);
    }

    // Phase 2: transit — between source and destination
    for p in &result[3..35] {
        assert!(
            p.x > 0.0 && p.x < 1.0,
            "transit should be between from and to"
        );
        assert_eq!(p.intensity, 0);
    }

    // Phase 3: start dwell — blanked at destination position
    for p in &result[35..] {
        assert_eq!(p.x, 1.0, "start dwell should be at to.x");
        assert_eq!(p.intensity, 0);
    }
}

#[test]
fn test_default_transition_full_diagonal() {
    // Full-range diagonal (-1,-1)→(1,1): d_inf = max(2.0, 2.0) = 2.0
    // transit = ceil(32 * 2.0) = 64 (clamped max)
    // At 30k: end_dwell=3, start_dwell=12, total = 3+64+12 = 79
    let from = make_point(-1.0, -1.0);
    let to = make_point(1.0, 1.0);
    let result = match call_default_transition(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };
    assert_eq!(result.len(), 79, "3 end + 64 transit + 12 start");
}

#[test]
fn test_default_transition_same_point() {
    // Same point: d_inf = 0, transit = 0
    // Only end_dwell + start_dwell = 3 + 12 = 15 at 30k PPS
    let p = make_point(0.5, -0.3);
    let result = match call_default_transition(&p, &p) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };
    assert_eq!(result.len(), 15, "end_dwell + start_dwell, no transit");
    for pt in &result {
        assert_eq!(pt.intensity, 0);
        assert_eq!(pt.x, 0.5);
        assert_eq!(pt.y, -0.3);
    }
}

#[test]
fn test_default_transition_quintic_easing() {
    // Transit should use quintic ease-in-out, not linear.
    // Near the start it should be slower (closer to from).
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 0.0);
    let result = match call_default_transition(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };

    // Transit is result[3..35] (32 points for d_inf=1.0)
    let transit = &result[3..35];

    // First transit point should be closer to from (ease-in is slow)
    assert!(
        transit[0].x < 0.25,
        "first transit point should be near from, got {}",
        transit[0].x
    );
    // Last transit point should be closer to to (ease-out is slow)
    assert!(
        transit[31].x > 0.75,
        "last transit point should be near to, got {}",
        transit[31].x
    );
}

#[test]
fn test_default_transition_pps_scales_dwells() {
    // Higher PPS should produce more dwell points
    let tf_low = default_transition(10_000);
    let tf_high = default_transition(100_000);

    let from = make_point(0.0, 0.0);
    let to = make_point(0.0, 0.0); // same point, transit=0

    let low_pts = match tf_low(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };
    let high_pts = match tf_high(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };

    assert!(
        high_pts.len() > low_pts.len(),
        "higher PPS should produce more dwell points: {} vs {}",
        high_pts.len(),
        low_pts.len()
    );
}

// =========================================================================
// PresentationEngine tests
// =========================================================================

/// Create an engine with a transition that produces 2 blanked interpolated points.
fn make_engine() -> PresentationEngine {
    PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        TransitionPlan::Transition(vec![
            LaserPoint::blanked(from.x * 0.5 + to.x * 0.5, from.y * 0.5 + to.y * 0.5),
            LaserPoint::blanked(to.x, to.y),
        ])
    }))
}

/// Create an engine with zero transition points (Transition with empty vec).
fn make_engine_no_transition() -> PresentationEngine {
    PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![])
    }))
}

/// Create an engine that always returns Coalesce.
fn make_engine_coalesce() -> PresentationEngine {
    PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Coalesce
    }))
}

fn make_frame(points: Vec<LaserPoint>) -> Frame {
    Frame::new(points)
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
fn test_engine_fill_chunk_self_loop_with_transition() {
    let mut engine = make_engine(); // 2-point transition
    let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame);

    // Self-loop: drawable = [0.0, 1.0, transition_mid, transition_end]
    // (transition from last=1.0 to first=0.0: mid=0.5, end=0.0)
    let mut buffer = vec![LaserPoint::default(); 8];
    let n = engine.fill_chunk(&mut buffer, 8);
    assert_eq!(n, 8);

    // First cycle: frame points + transition points
    assert_eq!(buffer[0].x, 0.0); // frame[0]
    assert_eq!(buffer[1].x, 1.0); // frame[1]
    assert_eq!(buffer[2].intensity, 0);
    assert_eq!(buffer[3].intensity, 0);
    // Second cycle
    assert_eq!(buffer[4].x, 0.0);
    assert_eq!(buffer[5].x, 1.0);
}

#[test]
fn test_engine_fill_chunk_frame_change_inserts_transition() {
    let mut engine = make_engine(); // 2-point transition
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(1.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b); // pending

    // A drawable = [0.0]. At seam: A→B transition directly (no stale A→A seam).
    let mut buffer = vec![LaserPoint::default(); 6];
    let n = engine.fill_chunk(&mut buffer, 6);
    assert_eq!(n, 6);

    // A's base point
    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[0].intensity, 65535);
    // A→B transition (2 blanked points)
    assert_eq!(buffer[1].intensity, 0);
    assert_eq!(buffer[2].intensity, 0);
    // B's base point
    assert_eq!(buffer[3].x, 1.0);
    assert_eq!(buffer[3].intensity, 65535);
    // B→B self-loop transition
    assert_eq!(buffer[4].intensity, 0);
    assert_eq!(buffer[5].intensity, 0);
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
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
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
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
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
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
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
    assert_eq!(
        composed[0].y, 9.0,
        "transition 'to' should be C.first (B skipped)"
    );
    assert_eq!(composed[1].x, 9.0);
}

#[test]
fn test_engine_compose_hardware_frame_empty_frame_produces_blanked_point() {
    // Submitting an empty frame should produce a single blanked origin point,
    // not an empty drawable that causes the scheduler to skip the write.
    let mut engine = make_engine_no_transition();
    let empty = make_frame(vec![]);
    engine.set_pending(empty);

    let composed = engine.compose_hardware_frame();
    assert_eq!(
        composed.len(),
        1,
        "empty frame should produce exactly 1 blanked point"
    );
    assert_eq!(composed[0].x, 0.0);
    assert_eq!(composed[0].y, 0.0);
    assert_eq!(composed[0].intensity, 0, "point should be blanked");
}

#[test]
fn test_engine_compose_hardware_frame_transition_to_empty_produces_blanked_point() {
    // Transition from a non-empty frame A to an empty frame B should still
    // produce a non-empty drawable (blanked origin), not stall on A.
    let mut engine = make_engine_no_transition();
    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let empty = make_frame(vec![]);
    engine.set_pending(empty);
    let composed = engine.compose_hardware_frame();
    assert!(
        !composed.is_empty(),
        "A→empty transition should not produce empty drawable"
    );
    // All points should be blanked (transition is empty since B has no first_point)
    for p in composed {
        assert_eq!(p.intensity, 0, "all points should be blanked");
    }
}

#[test]
fn test_engine_compose_hardware_frame_self_loop_empty_produces_blanked_point() {
    // Self-loop on an empty frame should produce a blanked point, not empty.
    let mut engine = make_engine_no_transition();
    let empty = make_frame(vec![]);
    engine.set_pending(empty);
    let _ = engine.compose_hardware_frame(); // promote empty

    // Second call: self-loop on the empty frame
    let composed = engine.compose_hardware_frame();
    assert_eq!(
        composed.len(),
        1,
        "self-loop on empty should produce 1 blanked point"
    );
    assert_eq!(composed[0].intensity, 0);
}

#[test]
fn test_engine_compose_hardware_frame_clamps_to_capacity() {
    // When frame + transition exceeds capacity, the transition prefix is truncated.
    // Use a transition that always produces 10 points.
    let mut engine = PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(0.0, 0.0); 10])
    }));
    engine.set_frame_capacity(Some(8)); // capacity = 8

    // Frame with 5 points + 10 transition = 15, should clamp to 8
    let frame = make_frame(vec![
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
        make_point(3.0, 0.0),
        make_point(4.0, 0.0),
        make_point(5.0, 0.0),
    ]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    assert!(
        composed.len() <= 8,
        "composed frame should not exceed capacity, got {}",
        composed.len()
    );
    // The 5 authored frame points should all be present (only transition trimmed)
    let frame_points: Vec<f32> = composed
        .iter()
        .filter(|p| p.intensity > 0)
        .map(|p| p.x)
        .collect();
    assert_eq!(
        frame_points,
        vec![1.0, 2.0, 3.0, 4.0, 5.0],
        "authored points must be preserved"
    );
}

#[test]
fn test_engine_compose_hardware_frame_no_truncation_under_capacity() {
    // When frame + transition fits within capacity, no truncation occurs.
    let mut engine = PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(0.0, 0.0); 3])
    }));
    engine.set_frame_capacity(Some(100)); // plenty of room

    let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // 3 transition + 2 frame = 5
    assert_eq!(composed.len(), 5, "no truncation expected");
}

#[test]
fn test_engine_compose_hardware_frame_self_loop_clamps_to_capacity() {
    // Self-loop transition should also be clamped.
    let mut engine = PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(0.0, 0.0); 20])
    }));
    engine.set_frame_capacity(Some(6));

    let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame);
    let _ = engine.compose_hardware_frame(); // promote, self-loop → dirty

    // Self-loop: 20 transition + 2 frame = 22, should clamp to 6
    let composed = engine.compose_hardware_frame();
    assert!(
        composed.len() <= 6,
        "self-loop should clamp to capacity, got {}",
        composed.len()
    );
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
// FIFO seam correctness tests
// =========================================================================

#[test]
fn test_fifo_no_stale_self_loop_seam() {
    // Key regression test: when B is pending, the seam must be A→B directly,
    // not A→A (stale self-loop) followed by A→B.
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        // Encode from/to into the blanked point for verification
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
    }));

    let frame_a = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b);

    let mut buffer = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buffer, 4);

    // A base: 1.0, 2.0
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[0].intensity, 65535);
    assert_eq!(buffer[1].x, 2.0);
    assert_eq!(buffer[1].intensity, 65535);
    // A→B transition: from=A.last(2.0), to=B.first(5.0)
    // NOT an A→A self-loop transition (which would have to=1.0)
    assert_eq!(buffer[2].x, 2.0, "transition 'from' should be A.last");
    assert_eq!(
        buffer[2].y, 5.0,
        "transition 'to' should be B.first, not A.first"
    );
    assert_eq!(buffer[2].intensity, 0);
    // B base: 5.0
    assert_eq!(buffer[3].x, 5.0);
    assert_eq!(buffer[3].intensity, 65535);
}

#[test]
fn test_fifo_pending_at_seam_uses_latest() {
    // When multiple frames are submitted before the seam, only the latest
    // is used for the transition (B is skipped, seam goes A→C).
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
    }));

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]);
    let frame_c = make_frame(vec![make_point(9.0, 0.0)]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b);
    engine.set_pending(frame_c); // overwrites B

    let mut buffer = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buffer, 4);

    assert_eq!(buffer[0].x, 1.0);
    // A→C transition (B skipped)
    assert_eq!(buffer[1].x, 1.0, "transition 'from' = A.last");
    assert_eq!(buffer[1].y, 9.0, "transition 'to' = C.first, B skipped");
    assert_eq!(buffer[1].intensity, 0);
    assert_eq!(buffer[2].x, 9.0);
    assert_eq!(buffer[2].intensity, 65535);
}

#[test]
fn test_fifo_stale_self_loop_discarded_across_chunks() {
    // P1 regression: self-loop transition points queued at the end of one
    // fill_chunk call must be discarded if a pending frame arrives before
    // the next fill_chunk call drains them.
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        TransitionPlan::Transition(vec![
            LaserPoint::blanked(from.x, to.x),
            LaserPoint::blanked(to.x, to.y),
        ])
    }));

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    engine.set_pending(frame_a);

    // First call: emit A's point (1.0), then at seam: self-loop queues
    // 2 transition points. Request exactly 1 point so the transition
    // points remain unconsumed in transition_buf.
    let mut buf1 = vec![LaserPoint::default(); 1];
    engine.fill_chunk(&mut buf1, 1);
    assert_eq!(buf1[0].x, 1.0);

    // Now B arrives while stale A→A transition is queued
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]);
    engine.set_pending(frame_b);

    // Second call: the stale A→A transition must be discarded.
    // We should see A→B transition then B, NOT stale A→A then A→B.
    let mut buf2 = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buf2, 4);

    // A→B transition (2 points) then B(5.0) then B→B self-loop trans
    assert_eq!(
        buf2[0].intensity, 0,
        "should be A→B transition, not stale A→A"
    );
    assert_eq!(buf2[1].intensity, 0);
    assert_eq!(buf2[2].x, 5.0);
    assert_eq!(buf2[2].intensity, 65535);
}

// =========================================================================
// FIFO Coalesce tests
// =========================================================================

#[test]
fn test_fifo_self_loop_coalesce_omits_last_point() {
    // Coalesce means last ≈ first, so self-loop skips the first point on wrap.
    let mut engine = make_engine_coalesce();
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(1.0, 0.0),
        make_point(0.0, 0.0), // same as first → will be omitted
    ]);
    engine.set_pending(frame);

    // Drawable should be [0.0, 1.0] (last point omitted).
    // Cycling: 0.0, 1.0, 0.0, 1.0, ...
    let mut buffer = vec![LaserPoint::default(); 6];
    let n = engine.fill_chunk(&mut buffer, 6);
    assert_eq!(n, 6);

    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].x, 1.0);
    assert_eq!(buffer[2].x, 0.0); // wrap — no duplicate
    assert_eq!(buffer[3].x, 1.0);
    // No blanked points anywhere
    for p in &buffer[..6] {
        assert_eq!(p.intensity, 65535);
    }
}

#[test]
fn test_fifo_self_loop_coalesce_single_point_preserved() {
    // Coalesce on a single-point frame must not produce zero points.
    let mut engine = make_engine_coalesce();
    let frame = make_frame(vec![make_point(5.0, 0.0)]);
    engine.set_pending(frame);

    let mut buffer = vec![LaserPoint::default(); 4];
    let n = engine.fill_chunk(&mut buffer, 4);
    assert_eq!(n, 4);
    for p in &buffer {
        assert_eq!(p.x, 5.0);
        assert_eq!(p.intensity, 65535);
    }
}

#[test]
fn test_fifo_a_to_b_coalesce_skips_incoming_first() {
    // A→B with Coalesce: B's first point is skipped since A.last ≈ B.first.
    let mut engine = make_engine_coalesce();
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    let frame_b = make_frame(vec![
        make_point(0.0, 0.0), // ≈ A.last → should be skipped
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b);

    // frame_a drawable (coalesced, 1 point) = [0.0]
    // At wrap: A→B coalesce → skip B[0], start at B[1]=1.0
    // B drawable (coalesced: last omitted since B[2]≈B[0]) = [0.0, 1.0]
    // But cursor starts at 1 in the B drawable → outputs B drawable[1]=1.0
    let mut buffer = vec![LaserPoint::default(); 6];
    engine.fill_chunk(&mut buffer, 6);

    assert_eq!(buffer[0].x, 0.0); // frame_a
    assert_eq!(buffer[1].x, 1.0); // frame_b[1] — B[0] skipped
}

#[test]
fn test_fifo_a_to_c_skip_latest_coalesce() {
    // A→C where B is overwritten. C's first point ≈ A's last → coalesce.
    let mut engine = make_engine_coalesce();
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]); // will be skipped
    let frame_c = make_frame(vec![
        make_point(0.0, 0.0), // ≈ A.last
        make_point(3.0, 0.0),
    ]);

    engine.set_pending(frame_a);
    engine.set_pending(frame_b);
    engine.set_pending(frame_c); // overwrites B

    let mut buffer = vec![LaserPoint::default(); 4];
    engine.fill_chunk(&mut buffer, 4);

    // A=[0.0], emit 0.0. At seam: pending=C, Coalesce → skip C[0].
    // C drawable=[0.0, 3.0], cursor=1 → emit 3.0.
    // Self-loop Coalesce → cursor=1 → emit 3.0 again.
    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].x, 3.0);
    assert_eq!(buffer[2].x, 3.0);
    assert_eq!(buffer[3].x, 3.0);
}

// =========================================================================
// FrameSwap Coalesce tests
// =========================================================================

#[test]
fn test_frame_swap_self_loop_coalesce() {
    // FrameSwap self-loop with Coalesce: last base point omitted.
    let mut engine = make_engine_coalesce();
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(1.0, 0.0),
        make_point(0.0, 0.0), // same as first → omitted by Coalesce
    ]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // Self-loop: coalesce omits last point → [0.0, 1.0]
    assert_eq!(composed.len(), 2, "coalesce should omit last point");
    assert_eq!(composed[0].x, 0.0);
    assert_eq!(composed[1].x, 1.0);
    // All lit (no transition)
    assert_eq!(composed[0].intensity, 65535);
    assert_eq!(composed[1].intensity, 65535);
}

#[test]
fn test_frame_swap_a_to_b_coalesce() {
    // FrameSwap A→B with Coalesce: A.last ≈ B.first, so B's first point
    // is skipped to avoid a duplicate seam sample.
    let mut engine = make_engine_coalesce();
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let frame_b = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame_b);

    let composed = engine.compose_hardware_frame();
    // Coalesce: B[0] skipped (≈ A.last), only B[1..] emitted
    assert_eq!(composed.len(), 1);
    assert_eq!(composed[0].x, 1.0);
    assert_eq!(composed[0].intensity, 65535);
}

// =========================================================================
// Backend parity tests
// =========================================================================

#[test]
fn test_parity_self_loop_with_transition() {
    // Both FIFO and FrameSwap: A→A with Transition([t1, t2])
    // Verify both include transition points in the self-loop.
    let transition_fn = || -> TransitionFn {
        Box::new(|from: &LaserPoint, to: &LaserPoint| {
            TransitionPlan::Transition(vec![
                LaserPoint::blanked(from.x * 0.5 + to.x * 0.5, 0.0),
                LaserPoint::blanked(to.x, to.y),
            ])
        })
    };

    let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);

    // FIFO: drawable = [frame | transition_suffix]
    let mut fifo_engine = PresentationEngine::new(transition_fn());
    fifo_engine.set_pending(frame.clone());
    let mut buffer = vec![LaserPoint::default(); 8];
    fifo_engine.fill_chunk(&mut buffer, 8);
    // Expect: 0.0, 1.0, blank, blank, 0.0, 1.0, blank, blank
    let fifo_intensities: Vec<u16> = buffer.iter().map(|p| p.intensity).collect();
    assert_eq!(
        fifo_intensities,
        vec![65535, 65535, 0, 0, 65535, 65535, 0, 0],
        "FIFO self-loop should include transition"
    );

    // FrameSwap: drawable = [transition_prefix | frame]
    let mut swap_engine = PresentationEngine::new(transition_fn());
    swap_engine.set_pending(frame);
    let composed = swap_engine.compose_hardware_frame();
    assert_eq!(composed.len(), 4, "FrameSwap self-loop: 2 trans + 2 frame");
    assert_eq!(composed[0].intensity, 0); // transition
    assert_eq!(composed[1].intensity, 0);
    assert_eq!(composed[2].intensity, 65535); // frame
    assert_eq!(composed[3].intensity, 65535);
}

#[test]
fn test_parity_self_loop_with_coalesce() {
    // Both FIFO and FrameSwap: A→A with Coalesce.
    // Both handle the seam correctly — no duplicate at the wrap point.
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]);

    // FIFO: drawable = [0.0, 1.0, 2.0]. At seam: Coalesce → skip first on wrap.
    // First cycle: 0.0, 1.0, 2.0. Subsequent cycles start at index 1: 1.0, 2.0.
    let mut fifo_engine = make_engine_coalesce();
    fifo_engine.set_pending(frame.clone());
    let mut buffer = vec![LaserPoint::default(); 6];
    fifo_engine.fill_chunk(&mut buffer, 6);
    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].x, 1.0);
    assert_eq!(buffer[2].x, 2.0);
    assert_eq!(buffer[3].x, 1.0);
    assert_eq!(buffer[4].x, 2.0);
    assert_eq!(buffer[5].x, 1.0);

    // FrameSwap: Coalesce omits last base point → [0.0, 1.0]
    let mut swap_engine = make_engine_coalesce();
    swap_engine.set_pending(frame);
    let composed = swap_engine.compose_hardware_frame();
    assert_eq!(composed.len(), 2);
    assert_eq!(composed[0].x, 0.0);
    assert_eq!(composed[1].x, 1.0);
}

#[test]
fn test_parity_a_to_b_with_transition() {
    // Both backends: A→B with transition points.
    let transition_fn = || -> TransitionFn {
        Box::new(|from: &LaserPoint, to: &LaserPoint| {
            TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
        })
    };

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]);

    // FIFO: A plays, then A→B transition directly, then B
    let mut fifo_engine = PresentationEngine::new(transition_fn());
    fifo_engine.set_pending(frame_a.clone());
    fifo_engine.set_pending(frame_b.clone());
    // A drawable = [1.0]. At seam: A→B transition (no stale self-loop), then B.
    let mut buffer = vec![LaserPoint::default(); 4];
    fifo_engine.fill_chunk(&mut buffer, 4);
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[0].intensity, 65535);
    assert_eq!(buffer[1].intensity, 0); // A→B transition
    assert_eq!(buffer[2].x, 5.0);
    assert_eq!(buffer[2].intensity, 65535);
    assert_eq!(buffer[3].intensity, 0); // B→B self-loop transition

    // FrameSwap: compose A→B = [transition | B]
    let mut swap_engine = PresentationEngine::new(transition_fn());
    swap_engine.set_pending(frame_a);
    let _ = swap_engine.compose_hardware_frame(); // promote A
    swap_engine.set_pending(frame_b);
    let composed = swap_engine.compose_hardware_frame();
    // [trans(1.0→5.0) | 5.0]
    assert_eq!(composed[0].x, 1.0);
    assert_eq!(composed[0].intensity, 0);
    assert_eq!(composed[1].x, 5.0);
    assert_eq!(composed[1].intensity, 65535);
}

#[test]
fn test_parity_a_to_b_with_coalesce() {
    // Both backends: A→B with Coalesce.
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    let frame_b = make_frame(vec![
        make_point(0.0, 0.0), // ≈ A.last
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]);

    // FIFO: A plays, then B with first point skipped
    let mut fifo_engine = make_engine_coalesce();
    fifo_engine.set_pending(frame_a.clone());
    fifo_engine.set_pending(frame_b.clone());
    let mut buffer = vec![LaserPoint::default(); 4];
    fifo_engine.fill_chunk(&mut buffer, 4);
    assert_eq!(buffer[0].x, 0.0); // frame_a
    assert_eq!(buffer[1].x, 1.0); // frame_b[1] — B[0] skipped

    // FrameSwap: A→B with Coalesce → B[0] skipped (≈ A.last), B[1..] emitted
    let mut swap_engine = make_engine_coalesce();
    swap_engine.set_pending(frame_a);
    let _ = swap_engine.compose_hardware_frame(); // promote A
    swap_engine.set_pending(frame_b);
    let composed = swap_engine.compose_hardware_frame();
    assert_eq!(composed.len(), 2);
    assert_eq!(composed[0].x, 1.0);
    assert_eq!(composed[1].x, 2.0);
}

#[test]
fn test_parity_a_to_c_skip() {
    // Both backends: A→C where B is overwritten.
    let transition_fn = || -> TransitionFn {
        Box::new(|from: &LaserPoint, to: &LaserPoint| {
            TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
        })
    };

    let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
    let frame_b = make_frame(vec![make_point(5.0, 0.0)]); // will be skipped
    let frame_c = make_frame(vec![make_point(9.0, 0.0)]);

    // FIFO: A plays, then A→C transition directly (no stale self-loop), then C.
    let mut fifo_engine = PresentationEngine::new(transition_fn());
    fifo_engine.set_pending(frame_a.clone());
    fifo_engine.set_pending(frame_b.clone());
    fifo_engine.set_pending(frame_c.clone()); // overwrites B
    let mut buffer = vec![LaserPoint::default(); 4];
    fifo_engine.fill_chunk(&mut buffer, 4);
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[1].intensity, 0); // A→C transition (B skipped, no stale self-loop)
    assert_eq!(buffer[2].x, 9.0);
    assert_eq!(buffer[3].intensity, 0); // C→C self-loop transition

    // FrameSwap
    let mut swap_engine = PresentationEngine::new(transition_fn());
    swap_engine.set_pending(frame_a);
    let _ = swap_engine.compose_hardware_frame();
    swap_engine.set_pending(frame_b);
    swap_engine.set_pending(frame_c); // overwrites B
    let composed = swap_engine.compose_hardware_frame();
    // trans(A→C) + C = [blanked(1.0, 9.0), 9.0]
    assert_eq!(composed[0].x, 1.0);
    assert_eq!(composed[0].intensity, 0);
    assert_eq!(composed[1].x, 9.0);
}

// =========================================================================
// FrameSession tests
// =========================================================================

use crate::backend::{BackendKind, DacBackend, FifoBackend, FrameSwapBackend};
use crate::error::Result as DacResult;
use crate::types::RunExit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone)]
struct RecordedFilterCall {
    points: Vec<LaserPoint>,
    ctx: OutputFilterContext,
}

#[derive(Default)]
struct FilterObservations {
    calls: AtomicUsize,
    resets: Mutex<Vec<OutputResetReason>>,
    invocations: Mutex<Vec<RecordedFilterCall>>,
}

struct RecordingFilter {
    observations: Arc<FilterObservations>,
    intensity_tag: Option<u16>,
}

impl RecordingFilter {
    fn new() -> (Self, Arc<FilterObservations>) {
        let observations = Arc::new(FilterObservations::default());
        (
            Self {
                observations: observations.clone(),
                intensity_tag: None,
            },
            observations,
        )
    }

    fn with_intensity_tag(tag: u16) -> (Self, Arc<FilterObservations>) {
        let observations = Arc::new(FilterObservations::default());
        (
            Self {
                observations: observations.clone(),
                intensity_tag: Some(tag),
            },
            observations,
        )
    }
}

impl OutputFilter for RecordingFilter {
    fn reset(&mut self, reason: OutputResetReason) {
        self.observations.resets.lock().unwrap().push(reason);
    }

    fn filter(&mut self, points: &mut [LaserPoint], ctx: &OutputFilterContext) {
        if let Some(tag) = self.intensity_tag {
            for point in points.iter_mut() {
                point.intensity = tag;
            }
        }
        self.observations.calls.fetch_add(1, Ordering::SeqCst);
        self.observations
            .invocations
            .lock()
            .unwrap()
            .push(RecordedFilterCall {
                points: points.to_vec(),
                ctx: *ctx,
            });
    }
}

struct StampingFilter {
    observations: Arc<FilterObservations>,
    next_stamp: u16,
}

impl StampingFilter {
    fn new() -> (Self, Arc<FilterObservations>) {
        let observations = Arc::new(FilterObservations::default());
        (
            Self {
                observations: observations.clone(),
                next_stamp: 1,
            },
            observations,
        )
    }
}

impl OutputFilter for StampingFilter {
    fn reset(&mut self, reason: OutputResetReason) {
        self.observations.resets.lock().unwrap().push(reason);
    }

    fn filter(&mut self, points: &mut [LaserPoint], ctx: &OutputFilterContext) {
        let stamp = self.next_stamp;
        self.next_stamp = self.next_stamp.saturating_add(1);
        for point in points.iter_mut() {
            point.intensity = stamp;
        }
        self.observations.calls.fetch_add(1, Ordering::SeqCst);
        self.observations
            .invocations
            .lock()
            .unwrap()
            .push(RecordedFilterCall {
                points: points.to_vec(),
                ctx: *ctx,
            });
    }
}

fn wait_for_filter_calls(
    observations: &Arc<FilterObservations>,
    min_len: usize,
    timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if observations.calls.load(Ordering::SeqCst) >= min_len {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!(
        "timed out waiting for {} filter calls, got {}",
        min_len,
        observations.calls.load(Ordering::SeqCst)
    );
}

fn wait_for_filter_reset(
    observations: &Arc<FilterObservations>,
    reason: OutputResetReason,
    timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if observations
            .resets
            .lock()
            .unwrap()
            .iter()
            .copied()
            .any(|reset| reset == reason)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for reset {reason:?}");
}

/// Minimal FIFO test backend for FrameSession tests.
struct FifoTestBackend {
    connected: bool,
    write_count: Arc<AtomicUsize>,
    points_written: Arc<AtomicUsize>,
    shutter_open: Arc<AtomicBool>,
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl FifoTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            points_written: Arc::new(AtomicUsize::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
            writes: Arc::new(Mutex::new(Vec::new())),
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
        self.writes.lock().unwrap().push(points.to_vec());
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
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
    frame_capacity: usize,
}

impl FrameSwapTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            write_count: Arc::new(AtomicUsize::new(0)),
            last_frame_size: Arc::new(AtomicUsize::new(0)),
            shutter_open: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            writes: Arc::new(Mutex::new(Vec::new())),
            frame_capacity: 4095,
        }
    }

    fn new_with_capacity(frame_capacity: usize) -> Self {
        Self {
            frame_capacity,
            ..Self::new()
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
        self.frame_capacity
    }
    fn is_ready_for_frame(&mut self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    fn write_frame(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.writes.lock().unwrap().push(points.to_vec());
        self.write_count.fetch_add(1, Ordering::SeqCst);
        self.last_frame_size.store(points.len(), Ordering::SeqCst);
        Ok(crate::backend::WriteOutcome::Written)
    }
}

struct RetryFifoTestBackend {
    connected: bool,
    caps: crate::types::DacCapabilities,
    shutter_open: Arc<AtomicBool>,
    block_next_writes: Arc<AtomicUsize>,
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl RetryFifoTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            caps: crate::types::DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 20,
                output_model: crate::types::OutputModel::NetworkFifo,
            },
            shutter_open: Arc::new(AtomicBool::new(false)),
            block_next_writes: Arc::new(AtomicUsize::new(0)),
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DacBackend for RetryFifoTestBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("RetryFifoTest".into())
    }
    fn caps(&self) -> &crate::types::DacCapabilities {
        &self.caps
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

impl FifoBackend for RetryFifoTestBackend {
    fn try_write_points(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.writes.lock().unwrap().push(points.to_vec());
        let remaining = self.block_next_writes.load(Ordering::SeqCst);
        if remaining > 0 {
            self.block_next_writes
                .store(remaining - 1, Ordering::SeqCst);
            return Ok(crate::backend::WriteOutcome::WouldBlock);
        }
        Ok(crate::backend::WriteOutcome::Written)
    }

    fn queued_points(&self) -> Option<u64> {
        Some(0)
    }
}

struct DisconnectAfterNFrameSwapBackend {
    connected: bool,
    fail_after: usize,
    write_count: AtomicUsize,
}

impl DisconnectAfterNFrameSwapBackend {
    fn new(fail_after: usize) -> Self {
        Self {
            connected: false,
            fail_after,
            write_count: AtomicUsize::new(0),
        }
    }
}

impl DacBackend for DisconnectAfterNFrameSwapBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("FilterReconnectFrameSwap".into())
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
    fn set_shutter(&mut self, _: bool) -> DacResult<()> {
        Ok(())
    }
}

impl FrameSwapBackend for DisconnectAfterNFrameSwapBackend {
    fn frame_capacity(&self) -> usize {
        4095
    }
    fn is_ready_for_frame(&mut self) -> bool {
        true
    }
    fn write_frame(
        &mut self,
        _pps: u32,
        _points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        let count = self.write_count.fetch_add(1, Ordering::SeqCst);
        if count >= self.fail_after {
            self.connected = false;
            Err(crate::error::Error::disconnected("simulated disconnect"))
        } else {
            Ok(crate::backend::WriteOutcome::Written)
        }
    }
}

struct ReconnectFrameSwapBackend {
    connected: bool,
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl ReconnectFrameSwapBackend {
    fn new(writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>) -> Self {
        Self {
            connected: false,
            writes,
        }
    }
}

impl DacBackend for ReconnectFrameSwapBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("FilterReconnectFrameSwap".into())
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
    fn set_shutter(&mut self, _: bool) -> DacResult<()> {
        Ok(())
    }
}

impl FrameSwapBackend for ReconnectFrameSwapBackend {
    fn frame_capacity(&self) -> usize {
        4095
    }
    fn is_ready_for_frame(&mut self) -> bool {
        true
    }
    fn write_frame(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.writes.lock().unwrap().push(points.to_vec());
        Ok(crate::backend::WriteOutcome::Written)
    }
}

struct FilterReconnectFrameSwapDiscoverer {
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl crate::discovery::ExternalDiscoverer for FilterReconnectFrameSwapDiscoverer {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("FilterReconnectFrameSwap".into())
    }

    fn scan(&mut self) -> Vec<crate::discovery::ExternalDevice> {
        let mut device = crate::discovery::ExternalDevice::new(());
        device.ip_address = Some("10.0.0.77".parse().unwrap());
        device.hardware_name = Some("Filter Reconnect FrameSwap".into());
        vec![device]
    }

    fn connect(&mut self, _opaque_data: Box<dyn std::any::Any + Send>) -> DacResult<BackendKind> {
        Ok(BackendKind::FrameSwap(Box::new(
            ReconnectFrameSwapBackend::new(self.writes.clone()),
        )))
    }
}

struct DelayedReconnectFrameSwapDiscoverer {
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
    empty_scans_remaining: Arc<AtomicUsize>,
}

impl crate::discovery::ExternalDiscoverer for DelayedReconnectFrameSwapDiscoverer {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("DelayedReconnectFrameSwap".into())
    }

    fn scan(&mut self) -> Vec<crate::discovery::ExternalDevice> {
        if self.empty_scans_remaining.load(Ordering::SeqCst) > 0 {
            self.empty_scans_remaining.fetch_sub(1, Ordering::SeqCst);
            return vec![];
        }

        let mut device = crate::discovery::ExternalDevice::new(());
        device.ip_address = Some("10.0.0.88".parse().unwrap());
        device.hardware_name = Some("Delayed Reconnect FrameSwap".into());
        vec![device]
    }

    fn connect(&mut self, _opaque_data: Box<dyn std::any::Any + Send>) -> DacResult<BackendKind> {
        Ok(BackendKind::FrameSwap(Box::new(
            ReconnectFrameSwapBackend::new(self.writes.clone()),
        )))
    }
}

struct RetryFrameSwapTestBackend {
    connected: bool,
    caps: crate::types::DacCapabilities,
    shutter_open: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    block_next_writes: Arc<AtomicUsize>,
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl RetryFrameSwapTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            caps: crate::types::DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 4095,
                output_model: crate::types::OutputModel::UsbFrameSwap,
            },
            shutter_open: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            block_next_writes: Arc::new(AtomicUsize::new(0)),
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DacBackend for RetryFrameSwapTestBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("RetryFrameSwapTest".into())
    }
    fn caps(&self) -> &crate::types::DacCapabilities {
        &self.caps
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

impl FrameSwapBackend for RetryFrameSwapTestBackend {
    fn frame_capacity(&self) -> usize {
        self.caps.max_points_per_chunk
    }
    fn is_ready_for_frame(&mut self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    fn write_frame(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.writes.lock().unwrap().push(points.to_vec());
        let remaining = self.block_next_writes.load(Ordering::SeqCst);
        if remaining > 0 {
            self.block_next_writes
                .store(remaining - 1, Ordering::SeqCst);
            return Ok(crate::backend::WriteOutcome::WouldBlock);
        }
        Ok(crate::backend::WriteOutcome::Written)
    }
}

struct RetryUdpTimedTestBackend {
    connected: bool,
    caps: crate::types::DacCapabilities,
    shutter_open: Arc<AtomicBool>,
    block_next_writes: Arc<AtomicUsize>,
    writes: Arc<Mutex<Vec<Vec<LaserPoint>>>>,
}

impl RetryUdpTimedTestBackend {
    fn new() -> Self {
        Self {
            connected: false,
            caps: crate::types::DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 3,
                output_model: crate::types::OutputModel::UdpTimed,
            },
            shutter_open: Arc::new(AtomicBool::new(false)),
            block_next_writes: Arc::new(AtomicUsize::new(0)),
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DacBackend for RetryUdpTimedTestBackend {
    fn dac_type(&self) -> crate::types::DacType {
        crate::types::DacType::Custom("RetryUdpTimedTest".into())
    }
    fn caps(&self) -> &crate::types::DacCapabilities {
        &self.caps
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

impl FifoBackend for RetryUdpTimedTestBackend {
    fn try_write_points(
        &mut self,
        _pps: u32,
        points: &[LaserPoint],
    ) -> DacResult<crate::backend::WriteOutcome> {
        self.writes.lock().unwrap().push(points.to_vec());
        let remaining = self.block_next_writes.load(Ordering::SeqCst);
        if remaining > 0 {
            self.block_next_writes
                .store(remaining - 1, Ordering::SeqCst);
            return Ok(crate::backend::WriteOutcome::WouldBlock);
        }
        Ok(crate::backend::WriteOutcome::Written)
    }
}

fn wait_for_writes(
    writes: &Arc<Mutex<Vec<Vec<LaserPoint>>>>,
    min_len: usize,
    timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if writes.lock().unwrap().len() >= min_len {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!(
        "timed out waiting for {} writes, got {}",
        min_len,
        writes.lock().unwrap().len()
    );
}

fn wait_for_loop_activity_after(
    metrics: &FrameSessionMetrics,
    after: std::time::Instant,
    timeout: std::time::Duration,
) -> std::time::Instant {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(activity) = metrics.last_loop_activity() {
            if activity > after {
                return activity;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for loop activity after {after:?}");
}

fn wait_for_write_success(
    metrics: &FrameSessionMetrics,
    timeout: std::time::Duration,
) -> std::time::Instant {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(write_success) = metrics.last_write_success() {
            return write_success;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for write success");
}

fn wait_for_write_success_after(
    metrics: &FrameSessionMetrics,
    after: std::time::Instant,
    timeout: std::time::Duration,
) -> std::time::Instant {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(write_success) = metrics.last_write_success() {
            if write_success > after {
                return write_success;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for write success after {after:?}");
}

fn encode_transition() -> TransitionFn {
    Box::new(|from: &LaserPoint, to: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
    })
}

fn is_encoded_frame_swap_transition(points: &[LaserPoint], from_x: f32, to_x: f32) -> bool {
    points.len() == 2
        && points[0].x == from_x
        && points[0].y == to_x
        && points[0].intensity == 0
        && points[1].x == to_x
        && points[1].intensity == 65535
}

fn is_transition_bearing_udp_chunk(points: &[LaserPoint]) -> bool {
    points.iter().any(|p| p.intensity == 0) && points.iter().any(|p| p.intensity != 0)
}

#[test]
fn test_frame_session_fifo_submit_frame_writes_points() {
    let backend = FifoTestBackend::new();
    let points_written = backend.points_written.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(30000).with_startup_blank(std::time::Duration::ZERO);
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

    let config = FrameSessionConfig::new(30000).with_startup_blank(std::time::Duration::ZERO);
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
fn test_frame_session_frame_swap_retries_same_inflight_frame_on_wouldblock() {
    let backend = RetryFrameSwapTestBackend::new();
    let writes = backend.writes.clone();
    let ready = backend.ready.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(encode_transition())
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_writes(&writes, 1, std::time::Duration::from_millis(200));

    ready.store(false, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let before = writes.lock().unwrap().len();
    block_next_writes.store(1, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(5.0, 0.0)]));
    ready.store(true, Ordering::SeqCst);
    wait_for_writes(&writes, before + 2, std::time::Duration::from_millis(200));

    let writes = writes.lock().unwrap();
    let first = &writes[before];
    let second = &writes[before + 1];
    assert_eq!(
        first, second,
        "inflight A→B frame should be retried verbatim"
    );
    assert!(
        is_encoded_frame_swap_transition(first, 1.0, 5.0),
        "expected encoded A→B frame, got {:?}",
        first
    );

    drop(writes);
    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for UDP-timed session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_frame_swap_inflight_frame_stays_sticky_until_accepted() {
    let backend = RetryFrameSwapTestBackend::new();
    let writes = backend.writes.clone();
    let ready = backend.ready.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(encode_transition())
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_writes(&writes, 1, std::time::Duration::from_millis(200));

    ready.store(false, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let before = writes.lock().unwrap().len();
    block_next_writes.store(3, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(5.0, 0.0)]));
    ready.store(true, Ordering::SeqCst);
    wait_for_writes(&writes, before + 1, std::time::Duration::from_millis(200));
    session.send_frame(Frame::new(vec![make_point(9.0, 0.0)]));
    wait_for_writes(&writes, before + 5, std::time::Duration::from_millis(300));

    let writes = writes.lock().unwrap();
    for attempt in &writes[before..before + 4] {
        assert!(
            is_encoded_frame_swap_transition(attempt, 1.0, 5.0),
            "expected sticky A→B frame, got {:?}",
            attempt
        );
    }

    let next = &writes[before + 4];
    assert!(
        is_encoded_frame_swap_transition(next, 5.0, 9.0),
        "expected B→C after A→B was accepted, got {:?}",
        next
    );

    drop(writes);
    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for UDP-timed session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_udp_timed_retries_same_transition_chunk_on_wouldblock() {
    let backend = RetryUdpTimedTestBackend::new();
    let writes = backend.writes.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(encode_transition())
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0);
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_writes(&writes, 1, std::time::Duration::from_millis(200));

    let before = writes.lock().unwrap().len();
    block_next_writes.store(1, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(5.0, 0.0)]));
    wait_for_writes(&writes, before + 8, std::time::Duration::from_millis(400));

    let writes = writes.lock().unwrap();
    let retry_idx = writes[before..]
        .windows(2)
        .position(|window| window[0] == window[1] && is_transition_bearing_udp_chunk(&window[0]))
        .unwrap_or_else(|| {
            panic!(
                "expected a retried transition-bearing UDP chunk, got {:?}",
                &writes[before..]
            )
        });
    let first = &writes[before + retry_idx];
    assert!(
        is_transition_bearing_udp_chunk(first),
        "expected a transition-bearing UDP chunk, got {:?}",
        first
    );
    if before + retry_idx + 2 < writes.len() {
        assert_ne!(
            first,
            &writes[before + retry_idx + 2],
            "retry should preserve one specific chunk, not collapse the whole output into a constant pattern"
        );
    }

    drop(writes);
    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for UDP-timed session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_fifo_output_filter_skips_keepalives_and_sees_color_delay() {
    let backend = RetryUdpTimedTestBackend::new();
    let writes = backend.writes.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));
    let (filter, observations) = RecordingFilter::new();

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(1)
        .with_output_filter(Box::new(filter));
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    wait_for_writes(&writes, 1, std::time::Duration::from_millis(200));
    assert_eq!(
        observations.calls.load(Ordering::SeqCst),
        0,
        "pre-first-frame FIFO keepalives must not invoke the output filter"
    );

    session.send_frame(Frame::new(vec![
        LaserPoint::new(0.0, 0.0, 100, 200, 300, 400),
        LaserPoint::new(0.5, 0.0, 500, 600, 700, 800),
        LaserPoint::new(1.0, 0.0, 900, 1000, 1100, 1200),
    ]));
    wait_for_filter_calls(&observations, 1, std::time::Duration::from_millis(200));

    let call = observations.invocations.lock().unwrap()[0].clone();
    assert_eq!(call.ctx.kind, PresentedSliceKind::FifoChunk);
    assert!(!call.ctx.is_cyclic);
    assert_eq!(call.points.len(), 3);
    assert_eq!(call.points[0].r, 0);
    assert_eq!(call.points[0].g, 0);
    assert_eq!(call.points[0].b, 0);
    assert_eq!(call.points[0].intensity, 0);
    assert_eq!(call.points[1].r, 100);
    assert_eq!(call.points[1].g, 200);
    assert_eq!(call.points[1].b, 300);
    assert_eq!(call.points[1].intensity, 400);
    assert_eq!(call.points[2].r, 500);
    assert_eq!(call.points[2].g, 600);
    assert_eq!(call.points[2].b, 700);
    assert_eq!(call.points[2].intensity, 800);
    assert!(
        writes
            .lock()
            .unwrap()
            .iter()
            .any(|write| *write == call.points),
        "backend should receive the same filtered FIFO chunk"
    );

    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for UDP-timed session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_frame_swap_output_filter_sees_post_clamp_cyclic_frame() {
    let backend = FrameSwapTestBackend::new_with_capacity(3);
    let writes = backend.writes.clone();
    let write_count = backend.write_count.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));
    let (filter, observations) = RecordingFilter::with_intensity_tag(1234);

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![
                LaserPoint::blanked(10.0, 0.0),
                LaserPoint::blanked(20.0, 0.0),
            ])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0)
        .with_output_filter(Box::new(filter));
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_filter_calls(&observations, 1, std::time::Duration::from_millis(200));

    let before_writes = write_count.load(Ordering::SeqCst);
    session.send_frame(Frame::new(vec![
        make_point(30.0, 0.0),
        make_point(40.0, 0.0),
    ]));
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let call = loop {
        if let Some(call) = observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .find(|call| call.points.len() == 3 && call.points[1].x == 30.0)
            .cloned()
        {
            break call;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the clamped A->B frame"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    while write_count.load(Ordering::SeqCst) <= before_writes {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    assert_eq!(call.ctx.kind, PresentedSliceKind::FrameSwapFrame);
    assert!(call.ctx.is_cyclic);
    assert_eq!(call.points.len(), 3);
    assert_eq!(
        call.points[0].x, 20.0,
        "clamp should trim the first transition point"
    );
    assert_eq!(call.points[1].x, 30.0);
    assert_eq!(call.points[2].x, 40.0);
    assert!(call.points.iter().all(|point| point.intensity == 1234));
    assert!(
        writes
            .lock()
            .unwrap()
            .iter()
            .any(|write| *write == call.points),
        "backend should receive the filtered frame-swap buffer verbatim"
    );

    session.control().stop().unwrap();
    session.join().unwrap();
}

#[test]
fn test_frame_session_output_filter_resets_on_arm_and_disarm_and_sees_blanking() {
    let backend = FrameSwapTestBackend::new();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));
    let (filter, observations) = RecordingFilter::new();

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::from_millis(2))
        .with_color_delay_points(0)
        .with_output_filter(Box::new(filter));
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.send_frame(Frame::new(vec![
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
        make_point(3.0, 0.0),
    ]));
    wait_for_filter_calls(&observations, 1, std::time::Duration::from_millis(200));
    assert_eq!(
        observations.resets.lock().unwrap().as_slice(),
        &[OutputResetReason::SessionStart]
    );

    let before_arm = observations.calls.load(Ordering::SeqCst);
    session.control().arm().unwrap();
    wait_for_filter_reset(
        &observations,
        OutputResetReason::Arm,
        std::time::Duration::from_millis(200),
    );
    let armed_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let armed = loop {
        if let Some(call) = observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .skip(before_arm)
            .find(|call| {
                call.points.len() >= 3
                    && call.points[0].intensity == 0
                    && call.points[1].intensity == 0
                    && call.points[2].intensity != 0
            })
            .cloned()
        {
            break call;
        }
        assert!(
            std::time::Instant::now() < armed_deadline,
            "timed out waiting for post-arm startup blanking output"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    assert_eq!(armed.points[0].intensity, 0);
    assert_eq!(armed.points[1].intensity, 0);
    assert_eq!(armed.points[2].intensity, 65535);

    let before_disarm = observations.calls.load(Ordering::SeqCst);
    session.control().disarm().unwrap();
    wait_for_filter_reset(
        &observations,
        OutputResetReason::Disarm,
        std::time::Duration::from_millis(200),
    );
    let disarmed_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let disarmed = loop {
        if let Some(call) = observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .skip(before_disarm)
            .find(|call| call.points.iter().all(|point| point.intensity == 0))
            .cloned()
        {
            break call;
        }
        assert!(
            std::time::Instant::now() < disarmed_deadline,
            "timed out waiting for disarmed blank output"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    assert!(disarmed.points.iter().all(|point| point.intensity == 0));

    let before_rearm = observations.calls.load(Ordering::SeqCst);
    session.control().arm().unwrap();
    let rearmed_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let rearmed = loop {
        if let Some(call) = observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .skip(before_rearm)
            .find(|call| {
                call.points.len() >= 3
                    && call.points[0].intensity == 0
                    && call.points[1].intensity == 0
                    && call.points[2].intensity != 0
            })
            .cloned()
        {
            break call;
        }
        assert!(
            std::time::Instant::now() < rearmed_deadline,
            "timed out waiting for post-rearm startup blanking output"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    assert_eq!(rearmed.points[0].intensity, 0);
    assert_eq!(rearmed.points[1].intensity, 0);
    assert_eq!(rearmed.points[2].intensity, 65535);
    assert_eq!(
        observations.resets.lock().unwrap().as_slice(),
        &[
            OutputResetReason::SessionStart,
            OutputResetReason::Arm,
            OutputResetReason::Disarm,
            OutputResetReason::Arm,
        ]
    );

    session.control().stop().unwrap();
    session.join().unwrap();
}

#[test]
fn test_frame_session_frame_swap_output_filter_not_rerun_for_retry() {
    let backend = RetryFrameSwapTestBackend::new();
    let writes = backend.writes.clone();
    let ready = backend.ready.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));
    let (filter, observations) = RecordingFilter::new();

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(encode_transition())
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0)
        .with_output_filter(Box::new(filter));
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_writes(&writes, 1, std::time::Duration::from_millis(200));
    wait_for_filter_calls(&observations, 1, std::time::Duration::from_millis(200));

    ready.store(false, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let before_writes = writes.lock().unwrap().len();
    block_next_writes.store(1, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(5.0, 0.0)]));
    ready.store(true, Ordering::SeqCst);
    wait_for_writes(
        &writes,
        before_writes + 2,
        std::time::Duration::from_millis(200),
    );

    let retried_frame = {
        let writes = writes.lock().unwrap();
        writes[before_writes].clone()
    };
    assert_eq!(
        observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.points == retried_frame)
            .count(),
        1,
        "retrying the same frame-swap write must not rerun the filter for that frame"
    );

    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for frame-swap session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_fifo_output_filter_not_rerun_for_inner_retry() {
    let backend = RetryFifoTestBackend::new();
    let writes = backend.writes.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));
    let (filter, observations) = StampingFilter::new();

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0)
        .with_output_filter(Box::new(filter));
    let session = FrameSession::start(backend_kind, config, None).unwrap();

    session.control().arm().unwrap();
    let before_writes = writes.lock().unwrap().len();
    block_next_writes.store(1, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    wait_for_writes(
        &writes,
        before_writes + 2,
        std::time::Duration::from_millis(200),
    );

    let retried_chunk = {
        let writes = writes.lock().unwrap();
        writes[before_writes..]
            .windows(2)
            .find(|window| {
                window[0] == window[1] && window[0].iter().any(|point| point.intensity != 0)
            })
            .map(|window| window[0].clone())
            .expect("expected a duplicated visible FIFO chunk")
    };
    let stamp = retried_chunk[0].intensity;
    assert_eq!(
        observations
            .invocations
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.points[0].intensity == stamp)
            .count(),
        1,
        "inner WouldBlock retry must reuse the already-filtered FIFO chunk"
    );
    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for FIFO session, got {exit:?}"
    );
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
fn test_frame_session_metrics_available_and_disconnect_after_stop() {
    let backend = FifoTestBackend::new();
    let backend_kind = BackendKind::Fifo(Box::new(backend));
    let config = FrameSessionConfig::new(30_000);
    let session = FrameSession::start(backend_kind, config, None).unwrap();
    let metrics = session.metrics();

    assert!(metrics.connected());
    assert!(metrics.last_loop_activity().is_some());
    let write_success = wait_for_write_success(&metrics, std::time::Duration::from_millis(200));
    assert!(write_success <= metrics.last_write_success().unwrap());

    session.control().stop().unwrap();
    session.join().unwrap();

    assert!(!metrics.connected());
}

#[test]
fn test_frame_session_fifo_metrics_advance_while_sleeping_with_healthy_buffer() {
    let backend = FifoTestBackend::new();
    let backend_kind = BackendKind::Fifo(Box::new(backend));
    let config = FrameSessionConfig::new(30).with_startup_blank(std::time::Duration::ZERO);
    let session = FrameSession::start(backend_kind, config, None).unwrap();
    let metrics = session.metrics();

    let first_write = wait_for_write_success(&metrics, std::time::Duration::from_millis(200));
    let later_activity =
        wait_for_loop_activity_after(&metrics, first_write, std::time::Duration::from_millis(200));

    assert!(later_activity > first_write);

    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for FIFO session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_udp_timed_metrics_advance_while_retrying_same_chunk() {
    let backend = RetryUdpTimedTestBackend::new();
    let writes = backend.writes.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::Fifo(Box::new(backend));

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0);
    let session = FrameSession::start(backend_kind, config, None).unwrap();
    let metrics = session.metrics();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    let baseline_write = wait_for_write_success(&metrics, std::time::Duration::from_millis(200));
    let before_writes = writes.lock().unwrap().len();
    let before_activity = metrics.last_loop_activity().unwrap();

    block_next_writes.store(20, Ordering::SeqCst);
    wait_for_writes(
        &writes,
        before_writes + 2,
        std::time::Duration::from_millis(200),
    );

    assert!(
        metrics.last_loop_activity().unwrap() > before_activity,
        "loop activity should advance while retrying a WouldBlock UDP chunk"
    );
    assert_eq!(
        metrics.last_write_success(),
        Some(baseline_write),
        "write-success timestamp must not advance on WouldBlock retries"
    );

    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for UDP-timed session, got {exit:?}"
    );
}

#[test]
fn test_frame_session_frame_swap_metrics_advance_while_waiting_for_readiness() {
    let backend = RetryFrameSwapTestBackend::new();
    let ready = backend.ready.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));
    let config = FrameSessionConfig::new(30_000).with_startup_blank(std::time::Duration::ZERO);
    ready.store(false, Ordering::SeqCst);

    let session = FrameSession::start(backend_kind, config, None).unwrap();
    let metrics = session.metrics();
    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));

    let before = metrics.last_loop_activity().unwrap();
    let after =
        wait_for_loop_activity_after(&metrics, before, std::time::Duration::from_millis(200));

    assert!(after > before);
    assert_eq!(metrics.last_write_success(), None);

    session.control().stop().unwrap();
    session.join().unwrap();
}

#[test]
fn test_frame_session_metrics_write_success_only_advances_on_successful_write() {
    let backend = RetryFrameSwapTestBackend::new();
    let writes = backend.writes.clone();
    let ready = backend.ready.clone();
    let block_next_writes = backend.block_next_writes.clone();
    let backend_kind = BackendKind::FrameSwap(Box::new(backend));

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(encode_transition())
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0);
    let session = FrameSession::start(backend_kind, config, None).unwrap();
    let metrics = session.metrics();

    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));
    let first_write = wait_for_write_success(&metrics, std::time::Duration::from_millis(200));
    assert!(first_write <= metrics.last_write_success().unwrap());

    ready.store(false, Ordering::SeqCst);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let baseline_write = metrics.last_write_success().unwrap();
    let before_writes = writes.lock().unwrap().len();
    block_next_writes.store(3, Ordering::SeqCst);
    session.send_frame(Frame::new(vec![make_point(5.0, 0.0)]));
    ready.store(true, Ordering::SeqCst);

    wait_for_writes(
        &writes,
        before_writes + 2,
        std::time::Duration::from_millis(200),
    );
    assert_eq!(
        metrics.last_write_success(),
        Some(baseline_write),
        "WouldBlock retries must not look like successful writes"
    );

    let next_write = wait_for_write_success_after(
        &metrics,
        baseline_write,
        std::time::Duration::from_millis(200),
    );
    assert!(next_write > baseline_write);

    session.control().stop().unwrap();
    let exit = session.join();
    assert!(
        matches!(
            exit,
            Ok(RunExit::Stopped) | Err(crate::error::Error::Stopped)
        ),
        "expected clean stop for frame-swap session, got {exit:?}"
    );
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
// Frame-swap padding invariants
// =========================================================================
//
// Hardware frame-swap DACs loop a fixed-size frame buffer. Two tempting
// strategies for filling remaining capacity are WRONG:
//
//   1. Cycling [transition | frame] to fill capacity reintroduces a
//      frame.last → transition.start galvo jump inside the hardware loop.
//   2. Padding short frames with held blank points collapses brightness
//      because the content duty cycle shrinks to content_len / capacity.
//
// The correct rule: compose_hardware_frame returns exactly
// transition_len + frame_len points. The hardware loops at that natural
// length.

#[test]
fn test_compose_hardware_frame_natural_length_no_padding() {
    // A 5-point frame with a 2-point transition should produce exactly 7
    // points — not padded to any larger capacity.
    let mut engine = make_engine(); // 2-point transition
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(0.2, 0.0),
        make_point(0.4, 0.0),
        make_point(0.6, 0.0),
        make_point(0.8, 0.0),
    ]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // Self-loop: transition(last→first) + frame = 2 + 5 = 7
    assert_eq!(
        composed.len(),
        7,
        "frame should be at natural length, not padded"
    );
}

#[test]
fn test_compose_hardware_frame_content_not_cycled() {
    // Verify that frame points appear exactly once — no cycling to fill
    // capacity. If the composed frame were cycled, we'd see frame points
    // repeating beyond position [transition_len + frame_len].
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        TransitionPlan::Transition(vec![LaserPoint::blanked(from.x, to.x)])
    }));
    let frame = make_frame(vec![
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
        make_point(3.0, 0.0),
    ]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // 1 transition + 3 frame = 4 total, no more
    assert_eq!(composed.len(), 4);
    // Frame content appears exactly once
    assert_eq!(composed[1].x, 1.0);
    assert_eq!(composed[2].x, 2.0);
    assert_eq!(composed[3].x, 3.0);
}

#[test]
fn test_compose_hardware_frame_short_frame_preserves_duty_cycle() {
    // A 2-point frame must NOT be padded with blank points. Every
    // non-transition point should carry the authored intensity.
    let mut engine = make_engine(); // 2-point transition
    let frame = make_frame(vec![make_point(0.5, 0.0), make_point(-0.5, 0.0)]);
    engine.set_pending(frame);

    let composed = engine.compose_hardware_frame();
    // 2 transition (blanked) + 2 frame (lit) = 4 total
    assert_eq!(composed.len(), 4, "no extra padding points");

    // Transition points are blanked
    assert_eq!(composed[0].intensity, 0);
    assert_eq!(composed[1].intensity, 0);
    // Frame points preserve full intensity
    assert_eq!(composed[2].intensity, 65535);
    assert_eq!(composed[3].intensity, 65535);
    // Content duty cycle = 2/4 = 50%, not collapsed by padding
}

#[test]
fn test_compose_hardware_frame_a_to_b_no_cycling_artifact() {
    // After A→B transition, the composed frame must be [transition(A→B) | B].
    // If it were cycled to fill capacity, cycling back to transition.start
    // would produce a B.last → A.last galvo jump (the "from" of the transition).
    let mut engine = PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        // Encode from/to into transition so we can verify no repetition
        TransitionPlan::Transition(vec![
            LaserPoint::blanked(from.x, from.y),
            LaserPoint::blanked(to.x, to.y),
        ])
    }));

    let frame_a = make_frame(vec![make_point(-1.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let frame_b = make_frame(vec![make_point(1.0, 0.0), make_point(1.0, 1.0)]);
    engine.set_pending(frame_b);
    let composed = engine.compose_hardware_frame();

    // Expected: [trans_from(-1,0), trans_to(1,0), B(1,0), B(1,1)] = 4 points
    assert_eq!(composed.len(), 4, "no cycling beyond transition + frame");
    // Last point is B.last
    assert_eq!(composed[3].x, 1.0);
    assert_eq!(composed[3].y, 1.0);
    // If cycled, there would be a 5th point jumping back to trans_from(-1,0).
    // The length assertion above proves this doesn't happen.
}

// =========================================================================
// Reconnect-related tests
// =========================================================================

#[test]
fn test_frame_session_output_filter_resets_on_reconnect_and_replays_last_frame() {
    use crate::stream::Dac;
    use crate::types::{DacInfo, DacType, ReconnectConfig};

    let reconnect_writes = Arc::new(Mutex::new(Vec::new()));
    let reconnect_writes_factory = reconnect_writes.clone();
    let initial_backend = DisconnectAfterNFrameSwapBackend::new(1);
    let info = DacInfo {
        id: "filterreconnectframeswap:10.0.0.77".to_string(),
        name: "Filter Reconnect FrameSwap".to_string(),
        kind: DacType::Custom("FilterReconnectFrameSwap".to_string()),
        caps: initial_backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::FrameSwap(Box::new(initial_backend)))
        .with_discovery_factory(move || {
            let mut discovery =
                crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::none());
            discovery.register(Box::new(FilterReconnectFrameSwapDiscoverer {
                writes: reconnect_writes_factory.clone(),
            }));
            discovery
        });

    let (filter, observations) = RecordingFilter::new();
    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0)
        .with_output_filter(Box::new(filter))
        .with_reconnect(
            ReconnectConfig::new()
                .max_retries(3)
                .backoff(std::time::Duration::from_millis(20)),
        );

    let (session, _info) = device.start_frame_session(config).unwrap();
    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]));

    wait_for_filter_reset(
        &observations,
        OutputResetReason::Reconnect,
        std::time::Duration::from_millis(500),
    );
    wait_for_filter_calls(&observations, 2, std::time::Duration::from_millis(500));

    let resets = observations.resets.lock().unwrap().clone();
    assert_eq!(resets[0], OutputResetReason::SessionStart);
    assert!(resets.contains(&OutputResetReason::Arm));
    assert!(resets.contains(&OutputResetReason::Reconnect));

    let invocations = observations.invocations.lock().unwrap().clone();
    let first = &invocations[0].points;
    assert!(
        invocations[1..].iter().any(|call| call.points == *first),
        "replayed last frame should flow through the filter again after reconnect"
    );
    assert!(
        !reconnect_writes.lock().unwrap().is_empty(),
        "reconnected backend should receive replayed output"
    );

    drop(session);
}

#[test]
fn test_frame_session_metrics_advance_during_reconnect_backoff() {
    use crate::stream::Dac;
    use crate::types::{DacInfo, DacType, ReconnectConfig};

    let reconnect_writes = Arc::new(Mutex::new(Vec::new()));
    let reconnect_writes_factory = reconnect_writes.clone();
    let empty_scans_remaining = Arc::new(AtomicUsize::new(3));
    let empty_scans_remaining_factory = empty_scans_remaining.clone();
    let initial_backend = DisconnectAfterNFrameSwapBackend::new(0);
    let info = DacInfo {
        id: "delayedreconnectframeswap:10.0.0.88".to_string(),
        name: "Delayed Reconnect FrameSwap".to_string(),
        kind: DacType::Custom("DelayedReconnectFrameSwap".to_string()),
        caps: initial_backend.caps().clone(),
    };
    let device = Dac::new(info, BackendKind::FrameSwap(Box::new(initial_backend)))
        .with_discovery_factory(move || {
            let mut discovery =
                crate::discovery::DacDiscovery::new(crate::types::EnabledDacTypes::none());
            discovery.register(Box::new(DelayedReconnectFrameSwapDiscoverer {
                writes: reconnect_writes_factory.clone(),
                empty_scans_remaining: empty_scans_remaining_factory.clone(),
            }));
            discovery
        });

    let config = FrameSessionConfig::new(1_000)
        .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
            TransitionPlan::Transition(vec![])
        }))
        .with_startup_blank(std::time::Duration::ZERO)
        .with_color_delay_points(0)
        .with_reconnect(
            ReconnectConfig::new()
                .max_retries(10)
                .backoff(std::time::Duration::from_millis(20)),
        );

    let (session, _info) = device.start_frame_session(config).unwrap();
    let metrics = session.metrics();
    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![make_point(1.0, 0.0)]));

    let disconnect_deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    while metrics.connected() {
        assert!(
            std::time::Instant::now() < disconnect_deadline,
            "timed out waiting for reconnect state"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let before = metrics.last_loop_activity().unwrap();
    let progress_deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        let activity = metrics.last_loop_activity().unwrap();
        if activity > before {
            break;
        }
        assert!(
            !metrics.connected(),
            "reconnect should still be in progress while checking liveness"
        );
        assert!(
            std::time::Instant::now() < progress_deadline,
            "timed out waiting for reconnect liveness progress"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    drop(session);
}

#[test]
fn test_engine_reset_clears_state() {
    use super::engine::PresentationEngine;

    let mut engine = PresentationEngine::new(default_transition(30_000));

    // Set a frame and advance cursor
    let frame = Frame::new(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
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

    let mut engine = PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![])
    }));

    // Play a frame
    let frame = Frame::new(vec![make_point(1.0, 0.0)]);
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
fn test_frame_fifo_buffer_estimation_matches_shared_scheduler_helper() {
    // Regression guard for the FIFO FrameSession path: its buffered-point
    // estimate should remain the shared conservative helper.
    assert_eq!(
        crate::scheduler::conservative_buffered_points(500, None),
        500
    );
    assert_eq!(
        crate::scheduler::conservative_buffered_points(500, Some(250)),
        250
    );
    assert_eq!(
        crate::scheduler::conservative_buffered_points(500, Some(900)),
        500
    );
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

    struct MinimalBackend {
        caps: DacCapabilities,
        connected: bool,
    }
    impl crate::backend::DacBackend for MinimalBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("Test".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> crate::backend::Result<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> crate::backend::Result<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn stop(&mut self) -> crate::backend::Result<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _: bool) -> crate::backend::Result<()> {
            Ok(())
        }
    }
    impl crate::backend::FifoBackend for MinimalBackend {
        fn try_write_points(
            &mut self,
            _: u32,
            _: &[LaserPoint],
        ) -> crate::backend::Result<crate::backend::WriteOutcome> {
            Ok(crate::backend::WriteOutcome::Written)
        }
        fn queued_points(&self) -> Option<u64> {
            None
        }
    }

    let backend = MinimalBackend {
        caps: caps.clone(),
        connected: false,
    };
    let info = DacInfo::new("test", "Test", DacType::Custom("Test".into()), caps);
    let mut device = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    device.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "test".to_string(),
        discovery_factory: None,
    });

    // PPS 500 is below pps_min=1000 — should be rejected even with reconnect
    let config = FrameSessionConfig::new(500).with_reconnect(crate::types::ReconnectConfig::new());
    let result = device.start_frame_session(config);
    assert!(result.is_err());
}
