use laser_dac::LaserPoint;

/// Closed circle with every 11th point blanked.
///
/// Geometrically a best case: consecutive points are ~0.0126 apart at 500
/// points and the path closes on itself, so a frame's last→first seam is
/// near-zero distance. Useful for isolating per-point cost, but see
/// [`artwork_points`] for content whose seams and blanking match real work.
#[allow(dead_code)]
pub fn laser_points(n: usize, phase: f32) -> Vec<LaserPoint> {
    (0..n)
        .map(|i| {
            let t = (i as f32 / n.max(1) as f32) * std::f32::consts::TAU + phase;
            let intensity = if i % 11 == 0 { 0 } else { 65535 };
            LaserPoint::new(
                t.cos(),
                t.sin(),
                ((i * 977) & 0xffff) as u16,
                ((i * 1597) & 0xffff) as u16,
                ((i * 2371) & 0xffff) as u16,
                intensity,
            )
        })
        .collect()
}

/// A regular polygon outline placed somewhere in the field.
struct Shape {
    cx: f32,
    cy: f32,
    radius: f32,
    vertices: usize,
    color: (u16, u16, u16),
}

/// Disjoint shapes placed far enough apart that the jumps between them are long.
const SHAPES: [Shape; 3] = [
    Shape {
        cx: -0.55,
        cy: 0.45,
        radius: 0.30,
        vertices: 4,
        color: (65535, 0, 0),
    },
    Shape {
        cx: 0.60,
        cy: -0.35,
        radius: 0.22,
        vertices: 3,
        color: (0, 65535, 0),
    },
    Shape {
        cx: -0.10,
        cy: -0.65,
        radius: 0.18,
        vertices: 5,
        color: (0, 16384, 65535),
    },
];

/// Points per blanked jump between two shapes.
const BLANK_JUMP_POINTS: usize = 6;

/// Fraction of each outline that is traced, leaving every shape open.
const OUTLINE_SPAN: f32 = 0.9;

/// Deterministic multi-shape artwork: three disjoint open polygons separated by
/// blanked jumps.
///
/// This models authored content more closely than [`laser_points`] in the ways
/// that drive presentation cost:
///
/// - shapes are disjoint, so the jumps between them are long and blanked,
/// - outlines are straight edges with corners rather than a smooth circle,
/// - blanked points arrive in runs rather than every 11th point,
/// - the path is open and ends far from where it starts, so a frame's
///   last→first seam drives a real `default_transition` rather than a
///   near-zero-distance one.
///
/// Always returns exactly `n` points. `phase` rotates every outline, which is
/// enough to make two frames differ without changing their cost profile.
#[allow(dead_code)]
pub fn artwork_points(n: usize, phase: f32) -> Vec<LaserPoint> {
    let jumps = (SHAPES.len() - 1) * BLANK_JUMP_POINTS;
    // Too small to lay out as artwork; fall back rather than emit a degenerate
    // frame. Benchmarks use frame sizes far above this.
    if n <= jumps + SHAPES.len() {
        return laser_points(n, phase);
    }

    let lit_total = n - jumps;
    let base = lit_total / SHAPES.len();
    let extra = lit_total % SHAPES.len();

    let mut points: Vec<LaserPoint> = Vec::with_capacity(n);
    for (index, shape) in SHAPES.iter().enumerate() {
        let (start_x, start_y) = polygon_point(shape, 0.0, phase);

        if index > 0 {
            let from = *points.last().expect("earlier shape emitted points");
            for step in 1..=BLANK_JUMP_POINTS {
                let t = step as f32 / BLANK_JUMP_POINTS as f32;
                points.push(LaserPoint::new(
                    from.x + (start_x - from.x) * t,
                    from.y + (start_y - from.y) * t,
                    0,
                    0,
                    0,
                    0,
                ));
            }
        }

        let (r, g, b) = shape.color;
        let lit = base + usize::from(index < extra);
        for i in 0..lit {
            let u = (i as f32 / lit.max(1) as f32) * OUTLINE_SPAN;
            let (x, y) = polygon_point(shape, u, phase);
            points.push(LaserPoint::new(x, y, r, g, b, 65535));
        }
    }

    debug_assert_eq!(points.len(), n);
    points
}

/// Position at `u` in `[0, 1)` along a regular polygon's perimeter, walking
/// vertex to vertex so the path has straight edges and corners.
fn polygon_point(shape: &Shape, u: f32, phase: f32) -> (f32, f32) {
    let scaled = u * shape.vertices as f32;
    let edge = scaled.floor();
    let frac = scaled - edge;

    let vertex = |k: f32| {
        let angle = (k / shape.vertices as f32) * std::f32::consts::TAU + phase;
        (
            shape.cx + shape.radius * angle.cos(),
            shape.cy + shape.radius * angle.sin(),
        )
    };
    let (x0, y0) = vertex(edge);
    let (x1, y1) = vertex(edge + 1.0);

    (x0 + (x1 - x0) * frac, y0 + (y1 - y0) * frac)
}

#[allow(dead_code)]
pub fn stereo_samples(n: usize) -> Vec<(f32, f32)> {
    (0..n)
        .map(|i| {
            let t = i as f32 * 0.013;
            (t.sin() * 0.9, (t * 0.73).cos() * 0.9)
        })
        .collect()
}
