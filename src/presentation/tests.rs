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

    // Near points still get blanking (at least blank-at-source + travel + blank-at-dest)
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
    // Even very close points get blanking
    let result = default_transition(&make_point(0.0, 0.0), &make_point(0.01, 0.0));
    let pts = match result {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("should produce Transition"),
    };
    assert!(
        pts.len() >= 3,
        "even tiny distance should produce blanking, got {}",
        pts.len()
    );

    // Farther points get more
    let result = default_transition(&make_point(0.0, 0.0), &make_point(1.0, 0.0));
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
    let result = match default_transition(&from, &to) {
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
fn test_default_transition_has_blank_travel_blank_structure() {
    let from = make_point(0.0, 0.0);
    let to = make_point(1.0, 0.0);
    let result = match default_transition(&from, &to) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };

    // First point: blank at source
    assert_eq!(result[0].x, 0.0, "should start blanked at from");
    assert_eq!(result[0].intensity, 0);

    // Last point: blank at destination
    let last = result.last().unwrap();
    assert_eq!(last.x, 1.0, "should end blanked at to");
    assert_eq!(last.intensity, 0);

    // Middle points should be between from and to
    let mid_idx = result.len() / 2;
    assert!(
        result[mid_idx].x > 0.0 && result[mid_idx].x < 1.0,
        "middle point should be between from and to, got {}",
        result[mid_idx].x
    );
}

#[test]
fn test_default_transition_same_point_still_blanks() {
    let p = make_point(0.5, -0.3);
    let result = match default_transition(&p, &p) {
        TransitionPlan::Transition(pts) => pts,
        _ => panic!("expected Transition"),
    };
    // Even same point gets blanking (blank at source + travel + blank at dest)
    assert!(
        result.len() >= 3,
        "same point should still produce blanking, got {}",
        result.len()
    );
    for pt in &result {
        assert_eq!(pt.intensity, 0);
    }
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

    // frame_a drawable: [frame(0.0) | self-loop-trans(2)] = 3 points
    // Then A→B transition (2 points), then frame_b
    let mut buffer = vec![LaserPoint::default(); 8];
    let n = engine.fill_chunk(&mut buffer, 8);
    assert_eq!(n, 8);

    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].intensity, 0); // frame_a self-loop
    assert_eq!(buffer[2].intensity, 0);
    // Cursor wraps → pending detected → A→B transition
    assert_eq!(buffer[3].intensity, 0);
    assert_eq!(buffer[4].intensity, 0);
    assert_eq!(buffer[5].x, 1.0);
    assert_eq!(buffer[5].intensity, 65535);
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
// FIFO Coalesce tests
// =========================================================================

#[test]
fn test_fifo_self_loop_coalesce_omits_last_point() {
    // Coalesce means last ≈ first, so self-loop drawable omits the last point.
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

    assert_eq!(buffer[0].x, 0.0);
    // C = [0.0, 3.0], self-loop coalesces → drawable = [0.0] (only 1 point).
    // A→C coalesce with drawable.len()==1 → cursor stays at 0, no skip.
    assert_eq!(buffer[1].x, 0.0);
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
    // FrameSwap A→B with Coalesce: no transition prefix, just B's points.
    let mut engine = make_engine_coalesce();
    let frame_a = make_frame(vec![make_point(0.0, 0.0)]);
    engine.set_pending(frame_a);
    let _ = engine.compose_hardware_frame(); // promote A

    let frame_b = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
    engine.set_pending(frame_b);

    let composed = engine.compose_hardware_frame();
    // Coalesce: no transition prefix, just B's points
    assert_eq!(composed.len(), 2);
    assert_eq!(composed[0].x, 0.0);
    assert_eq!(composed[1].x, 1.0);
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
    // Both should omit the last base point.
    let frame = make_frame(vec![
        make_point(0.0, 0.0),
        make_point(1.0, 0.0),
        make_point(2.0, 0.0),
    ]);

    // FIFO
    let mut fifo_engine = make_engine_coalesce();
    fifo_engine.set_pending(frame.clone());
    let mut buffer = vec![LaserPoint::default(); 4];
    fifo_engine.fill_chunk(&mut buffer, 4);
    // Drawable = [0.0, 1.0] (last omitted), cycling: 0.0, 1.0, 0.0, 1.0
    assert_eq!(buffer[0].x, 0.0);
    assert_eq!(buffer[1].x, 1.0);
    assert_eq!(buffer[2].x, 0.0);
    assert_eq!(buffer[3].x, 1.0);

    // FrameSwap
    let mut swap_engine = make_engine_coalesce();
    swap_engine.set_pending(frame);
    let composed = swap_engine.compose_hardware_frame();
    // Coalesce: last omitted → [0.0, 1.0]
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

    // FIFO: A plays, then transition, then B
    let mut fifo_engine = PresentationEngine::new(transition_fn());
    fifo_engine.set_pending(frame_a.clone());
    fifo_engine.set_pending(frame_b.clone());
    // A drawable = [1.0, self-loop-trans(blanked)]. At wrap: A→B trans, then B.
    let mut buffer = vec![LaserPoint::default(); 6];
    fifo_engine.fill_chunk(&mut buffer, 6);
    // buffer: 1.0(lit), self-trans(blank), A→B trans(blank), 5.0(lit), ...
    assert_eq!(buffer[0].x, 1.0);
    assert_eq!(buffer[0].intensity, 65535);
    assert_eq!(buffer[1].intensity, 0); // self-loop transition
    assert_eq!(buffer[2].intensity, 0); // A→B transition
    assert_eq!(buffer[3].x, 5.0);
    assert_eq!(buffer[3].intensity, 65535);

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

    // FrameSwap: A→B with Coalesce → just B's points (no transition prefix)
    let mut swap_engine = make_engine_coalesce();
    swap_engine.set_pending(frame_a);
    let _ = swap_engine.compose_hardware_frame(); // promote A
    swap_engine.set_pending(frame_b);
    let composed = swap_engine.compose_hardware_frame();
    // Coalesce: no prefix, just B's 3 points
    assert_eq!(composed.len(), 3);
    assert_eq!(composed[0].x, 0.0);
    assert_eq!(composed[1].x, 1.0);
    assert_eq!(composed[2].x, 2.0);
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

    // FIFO
    let mut fifo_engine = PresentationEngine::new(transition_fn());
    fifo_engine.set_pending(frame_a.clone());
    fifo_engine.set_pending(frame_b.clone());
    fifo_engine.set_pending(frame_c.clone()); // overwrites B
    let mut buffer = vec![LaserPoint::default(); 5];
    fifo_engine.fill_chunk(&mut buffer, 5);
    assert_eq!(buffer[0].x, 1.0);
    // A drawable: [1.0, self-trans]. At wrap: pending=C, trans(1.0→9.0).
    assert_eq!(buffer[1].intensity, 0);
    assert_eq!(buffer[2].intensity, 0);
    assert_eq!(buffer[3].x, 9.0);

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
        self.last_frame_size.store(points.len(), Ordering::SeqCst);
        Ok(crate::backend::WriteOutcome::Written)
    }
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
fn test_engine_reset_clears_state() {
    use super::engine::PresentationEngine;

    let mut engine = PresentationEngine::new(Box::new(default_transition));

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
