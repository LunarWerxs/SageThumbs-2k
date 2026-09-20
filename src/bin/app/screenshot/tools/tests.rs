use super::*;

fn point(x: i32, y: i32) -> POINT {
    POINT { x, y }
}

fn length(a: POINT, b: POINT) -> f64 {
    (b.x as f64 - a.x as f64).hypot(b.y as f64 - a.y as f64)
}

#[test]
fn snap_45_keeps_zero_and_exact_eight_way_directions() {
    let a = point(37, -19);
    assert_eq!(snap_endpoint_45(a, a), a);

    for (dx, dy) in [
        (20, 0),
        (20, 20),
        (0, 20),
        (-20, 20),
        (-20, 0),
        (-20, -20),
        (0, -20),
        (20, -20),
    ] {
        let b = point(a.x + dx, a.y + dy);
        assert_eq!(snap_endpoint_45(a, b), b);
    }
}

#[test]
fn snap_45_chooses_the_nearest_octant_in_every_quadrant() {
    let a = point(100, 100);
    for raw in [
        point(140, 112),
        point(140, 88),
        point(60, 112),
        point(60, 88),
    ] {
        let b = snap_endpoint_45(a, raw);
        assert_eq!(b.y, a.y, "{raw:?} should snap horizontally");
    }
    for raw in [
        point(112, 140),
        point(88, 140),
        point(112, 60),
        point(88, 60),
    ] {
        let b = snap_endpoint_45(a, raw);
        assert_eq!(b.x, a.x, "{raw:?} should snap vertically");
    }
    for raw in [
        point(130, 125),
        point(70, 125),
        point(70, 75),
        point(130, 75),
    ] {
        let b = snap_endpoint_45(a, raw);
        assert_eq!(
            (b.x - a.x).abs(),
            (b.y - a.y).abs(),
            "{raw:?} should snap diagonally"
        );
    }
}

#[test]
fn snap_45_switches_at_the_half_octant_boundaries() {
    let a = point(0, 0);

    // tan(22.5°) is about 0.41421356: one side is horizontal,
    // the other is diagonal.
    let below_22 = snap_endpoint_45(a, point(1000, 414));
    let above_22 = snap_endpoint_45(a, point(1000, 415));
    assert_eq!(below_22.y, 0);
    assert_eq!(above_22.x.abs(), above_22.y.abs());

    // The matching 67.5° boundary separates diagonal from vertical.
    let below_67 = snap_endpoint_45(a, point(415, 1000));
    let above_67 = snap_endpoint_45(a, point(414, 1000));
    assert_eq!(below_67.x.abs(), below_67.y.abs());
    assert_eq!(above_67.x, 0);
}

#[test]
fn snap_45_preserves_drag_length_to_rounding_tolerance() {
    let a = point(-73, 211);
    for raw in [
        point(492, 329),
        point(-188, 804),
        point(-601, -97),
        point(171, -380),
    ] {
        let snapped = snap_endpoint_45(a, raw);
        assert!(
            (length(a, raw) - length(a, snapped)).abs() <= 1.0,
            "{raw:?} -> {snapped:?} changed drag length too much"
        );
    }
}

#[test]
fn drag_endpoint_only_constrains_shifted_lines_and_arrows() {
    let a = point(20, 30);
    let raw = point(120, 58);
    let snapped = snap_endpoint_45(a, raw);

    assert_eq!(drag_endpoint(Tool::Line, a, raw, true), snapped);
    assert_eq!(drag_endpoint(Tool::Arrow, a, raw, true), snapped);
    assert_eq!(drag_endpoint(Tool::Line, a, raw, false), raw);
    assert_eq!(drag_endpoint(Tool::Rect, a, raw, true), raw);
    assert_eq!(drag_endpoint(Tool::Pen, a, raw, true), raw);
}
