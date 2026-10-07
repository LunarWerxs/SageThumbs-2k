#![cfg(test)]

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

/// A memory DC over a `w`x`h` top-down 32bpp DIB, its grey pixels painted from `shade(x, y)`,
/// so a test can draw a shape through the real GDI path and read back exactly what it did.
struct Canvas {
    dc: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Canvas {
    unsafe fn new(w: i32, h: i32, shade: impl Fn(i32, i32) -> u8) -> Self {
        let (bmp, bits) = st2k_base::safety::create_dib_section(w, h).expect("dib section");
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        let c = Canvas {
            dc,
            bmp,
            old,
            bits: bits.cast(),
            w,
            h,
        };
        let px = std::slice::from_raw_parts_mut(c.bits, (w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = shade(x, y);
                let o = ((y * w + x) * 4) as usize;
                px[o..o + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        c
    }

    /// The canvas's pixels as they are now (GDI flushed first).
    unsafe fn pixels(&self) -> Vec<u8> {
        let _ = GdiFlush();
        std::slice::from_raw_parts(self.bits, (self.w * self.h * 4) as usize).to_vec()
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Fine stripes in both directions: an edge every few pixels, so any blur shows.
fn stripes(x: i32, y: i32) -> u8 {
    if (x / 3 + y / 5) % 2 == 0 {
        230
    } else {
        20
    }
}

fn blur(r: RECT, radius: i32) -> Shape {
    Shape::Blur {
        r,
        radius,
        cache: BlurCache::default(),
    }
}

/// One hard vertical edge, dark on the left half of a 256 px canvas and light on the right.
fn edge(x: i32, _y: i32) -> u8 {
    if x < 128 {
        20
    } else {
        230
    }
}

/// The steepest step of the blue channel between neighbours along row `y`, from `x0` to
/// `x1`: the sharpness of the edge. A blur of sigma s flattens a step of height h to a slope
/// of about h / (2.5 s), so it falls steadily as the blur gets stronger.
fn steepest_step(px: &[u8], w: i32, y: i32, x0: i32, x1: i32) -> u8 {
    let at = |x: i32| px[((y * w + x) * 4) as usize];
    (x0..x1 - 1)
        .map(|x| at(x + 1).abs_diff(at(x)))
        .max()
        .unwrap_or(0)
}

/// The Blur tool changes the pixels inside its rect and not one pixel outside it, also when
/// drawn with the offset `compose` uses to bake shapes into the cropped output.
#[test]
fn blur_changes_only_the_pixels_inside_its_rect() {
    unsafe {
        let (w, h) = (64, 48);
        let inside = RECT {
            left: 16,
            top: 12,
            right: 48,
            bottom: 36,
        };
        // Stored in screen space, drawn shifted back by (8, 4), as compose() does.
        let stored = RECT {
            left: inside.left + 8,
            top: inside.top + 4,
            right: inside.right + 8,
            bottom: inside.bottom + 4,
        };
        let c = Canvas::new(w, h, stripes);
        let before = c.pixels();
        draw_shape(c.dc, -8, -4, &blur(stored, 6));
        let after = c.pixels();
        let mut changed_inside = 0;
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                let differs = before[o..o + 4] != after[o..o + 4];
                let is_inside =
                    x >= inside.left && x < inside.right && y >= inside.top && y < inside.bottom;
                if is_inside {
                    changed_inside += usize::from(differs);
                } else {
                    assert!(!differs, "pixel ({x}, {y}) outside the blur changed");
                }
            }
        }
        let area = ((inside.right - inside.left) * (inside.bottom - inside.top)) as usize;
        assert!(
            changed_inside * 2 > area,
            "only {changed_inside} of {area} pixels inside the blur changed"
        );
    }
}

/// A higher strength blurs more: the hard edge inside the rect gets softer at each step up,
/// across the setting's range (2 is its minimum, 16 above its default of 12).
#[test]
fn a_stronger_blur_is_blurrier() {
    unsafe {
        let (w, h) = (256, 24);
        // The rect spans the edge with flat margins wider than the strongest blur reaches,
        // so its own borders add no slope of their own.
        let r = RECT {
            left: 8,
            top: 0,
            right: 248,
            bottom: h,
        };
        let mut last = steepest_step(&Canvas::new(w, h, edge).pixels(), w, h / 2, 8, 248);
        assert_eq!(last, 210, "the unblurred edge is one full step");
        for radius in [2, 6, 16] {
            let c = Canvas::new(w, h, edge);
            draw_shape(c.dc, 0, 0, &blur(r, radius));
            let step = steepest_step(&c.pixels(), w, h / 2, 8, 248);
            assert!(
                step < last,
                "radius {radius} left a step of {step}, not less than {last}"
            );
            last = step;
        }
    }
}

/// The blur cache must never paint a stale result: the same Blur shape drawn over different
/// pixels (an annotation underneath changed, or the blur was moved) is blurred afresh, and
/// over the same pixels again gives the same picture.
#[test]
fn a_cached_blur_follows_the_pixels_under_it() {
    unsafe {
        let r = RECT {
            left: 4,
            top: 4,
            right: 28,
            bottom: 20,
        };
        let shape = blur(r, 5);
        let first = Canvas::new(32, 24, stripes);
        draw_shape(first.dc, 0, 0, &shape);
        let other = Canvas::new(32, 24, |x, y| ((x * 7 + y * 3) % 256) as u8);
        draw_shape(other.dc, 0, 0, &shape);
        let fresh = Canvas::new(32, 24, |x, y| ((x * 7 + y * 3) % 256) as u8);
        draw_shape(fresh.dc, 0, 0, &blur(r, 5));
        assert_eq!(
            other.pixels(),
            fresh.pixels(),
            "the cached shape painted its old result over new pixels"
        );
        let again = Canvas::new(32, 24, stripes);
        draw_shape(again.dc, 0, 0, &shape);
        assert_eq!(again.pixels(), first.pixels());
    }
}

/// A blur rect can be one pixel wide or tall, far narrower than the strongest radius. The
/// sliding window repeats the border pixel, so a one-colour block of any size stays exactly
/// that colour and its alpha is never touched.
#[test]
fn the_strongest_blur_on_a_tiny_block_keeps_its_colour() {
    let radius = st2k_base::settings::SHOT_BLUR_MAX as usize;
    for (w, h) in [(1, 1), (2, 2), (1, 7), (7, 1), (3, 2)] {
        let mut px = [40u8, 120, 200, 77].repeat(w * h);
        box_blur3(&mut px, w, h, radius);
        assert!(
            px.chunks(4).all(|p| p == [40, 120, 200, 77]),
            "{w}x{h}: {px:?}"
        );
    }
}
