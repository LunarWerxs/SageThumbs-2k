//! Image watermark overlay for the Convert dialog: scale a mark image to a
//! fraction of the target's shorter edge, fade it to the chosen opacity, and
//! alpha-blend it onto a corner (or the centre) with a small margin.
//!
//! Text watermarks are out of scope here (no glyph rasterizer in this build) -
//! this only ever overlays another IMAGE onto the target.

use image::DynamicImage;

/// Where the mark lands on the target image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

/// A watermark chosen in the Convert dialog. `path` is read and decoded fresh
/// for every file the batch converts (the same bounded reader/decoder the
/// source image goes through), so a mark that fails to read fails that one
/// file's conversion like any other error - never a silent skip.
#[derive(Clone)]
pub struct Watermark {
    pub path: String,
    pub corner: Corner,
    /// Percent of the target's shorter edge the mark's longer edge is scaled
    /// to (1..=100).
    pub scale_pct: u8,
    /// Percent opacity applied on top of the mark's own alpha (0..=100).
    pub opacity_pct: u8,
}

/// Gap between the mark and the target's edge, as a fraction of the target's
/// shorter edge. Not user-configurable - a fixed, small breathing margin.
const MARGIN_FRACTION: f64 = 0.03;

/// The mark's on-canvas size for `scale_pct`% of the target's shorter edge:
/// aspect-preserving, and bounded so it can never come out larger than the
/// target itself (even when the source mark is much bigger) and never
/// collapses to zero (even at a tiny target or an out-of-range `scale_pct`).
fn scaled_dims(
    target_w: u32,
    target_h: u32,
    mark_w: u32,
    mark_h: u32,
    scale_pct: u8,
) -> (u32, u32) {
    if target_w == 0 || target_h == 0 || mark_w == 0 || mark_h == 0 {
        return (0, 0);
    }
    let shorter = target_w.min(target_h) as f64;
    let pct = scale_pct.clamp(1, 100) as f64 / 100.0;
    let target_edge = (shorter * pct).round().max(1.0);
    let mark_longer = mark_w.max(mark_h) as f64;
    let scale = target_edge / mark_longer;
    let w = ((mark_w as f64 * scale).round() as u32).clamp(1, target_w);
    let h = ((mark_h as f64 * scale).round() as u32).clamp(1, target_h);
    (w, h)
}

/// Alpha-blend `mark` onto `img` at `corner`, scaled to `scale_pct` of `img`'s
/// shorter edge and faded to `opacity_pct` of its own alpha. A no-op if either
/// image is zero-sized, or if `opacity_pct` is 0 (nothing would be visible).
/// Never allocates past `img`'s own pixel count - the mark is downscaled to
/// fit before the overlay, never the canvas grown to fit the mark.
pub fn apply(
    img: &mut DynamicImage,
    mark: &DynamicImage,
    corner: Corner,
    scale_pct: u8,
    opacity_pct: u8,
) {
    let (tw, th) = (img.width(), img.height());
    let (mark_w, mark_h) = (mark.width(), mark.height());
    let (scaled_w, scaled_h) = scaled_dims(tw, th, mark_w, mark_h, scale_pct);
    if scaled_w == 0 || scaled_h == 0 || opacity_pct == 0 {
        return;
    }

    let mut mark_rgba = mark
        .resize_exact(scaled_w, scaled_h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    if opacity_pct < 100 {
        let factor = opacity_pct.clamp(0, 100) as u32;
        for p in mark_rgba.pixels_mut() {
            p.0[3] = ((p.0[3] as u32 * factor) / 100) as u8;
        }
    }

    let margin_x = ((tw as f64) * MARGIN_FRACTION).round() as i64;
    let margin_y = ((th as f64) * MARGIN_FRACTION).round() as i64;
    let (tw, th) = (tw as i64, th as i64);
    let (mw, mh) = (scaled_w as i64, scaled_h as i64);
    let (x, y) = match corner {
        Corner::TopLeft => (margin_x, margin_y),
        Corner::TopRight => (tw - mw - margin_x, margin_y),
        Corner::BottomLeft => (margin_x, th - mh - margin_y),
        Corner::BottomRight => (tw - mw - margin_x, th - mh - margin_y),
        Corner::Center => ((tw - mw) / 2, (th - mh) / 2),
    };
    // Belt-and-braces: `scaled_dims` already guarantees the mark fits, so this
    // never actually clips - it just protects against a future change to the
    // sizing math letting a corner placement wander off-canvas.
    let x = x.clamp(0, (tw - mw).max(0));
    let y = y.clamp(0, (th - mh).max(0));

    let mut base = img.to_rgba8();
    image::imageops::overlay(&mut base, &mark_rgba, x, y);
    *img = DynamicImage::ImageRgba8(base);
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    const BG: Rgba<u8> = Rgba([0, 0, 0, 255]);
    const MARK: Rgba<u8> = Rgba([255, 255, 255, 255]);

    fn solid(w: u32, h: u32, color: Rgba<u8>) -> DynamicImage {
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(w, h, color))
    }

    #[test]
    fn scaled_dims_bounds_a_mark_larger_than_the_target() {
        // A 1000x1000 mark at 100% of a 64x64 target's shorter edge must still
        // fit inside the target, never exceeding it, never collapsing to zero.
        let (w, h) = scaled_dims(64, 64, 1000, 1000, 100);
        assert!(w > 0 && h > 0, "must not collapse to zero: {w}x{h}");
        assert!(w <= 64 && h <= 64, "must not exceed the target: {w}x{h}");
        assert_eq!((w, h), (64, 64));

        // Aspect is preserved for a non-square mark, still bounded to the target.
        let (w, h) = scaled_dims(64, 64, 2000, 1000, 50);
        assert!(w <= 64 && h <= 64);
        assert_eq!((w, h), (32, 16));
    }

    #[test]
    fn apply_places_the_mark_at_each_corner() {
        // scale_pct = 25 of a 64px shorter edge is 16 - exactly the mark's own
        // size, so it lands unscaled and the expected pixel math below is exact.
        let cases = [
            (Corner::TopLeft, (2i64, 2i64)),
            (Corner::TopRight, (46, 2)),
            (Corner::BottomLeft, (2, 46)),
            (Corner::BottomRight, (46, 46)),
            (Corner::Center, (24, 24)),
        ];
        for (corner, (x, y)) in cases {
            let mut img = solid(64, 64, BG);
            let mark = solid(16, 16, MARK);
            apply(&mut img, &mark, corner, 25, 100);
            let out = img.to_rgba8();
            let (x, y) = (x as u32, y as u32);
            assert_eq!(*out.get_pixel(x, y), MARK, "{corner:?} inner pixel");
            assert_eq!(
                *out.get_pixel(x + 15, y + 15),
                MARK,
                "{corner:?} far corner pixel"
            );
            // A pixel just outside the mark's footprint, on the same row, stays
            // untouched - the mark never bleeds past its own placed box.
            let outside = if x > 0 { x - 1 } else { x + 16 };
            assert_eq!(*out.get_pixel(outside, y), BG, "{corner:?} outside pixel");
        }
    }

    #[test]
    fn apply_honors_opacity_0_50_100() {
        let mark = solid(16, 16, MARK);

        let mut none = solid(64, 64, BG);
        apply(&mut none, &mark, Corner::TopLeft, 25, 0);
        assert_eq!(
            *none.to_rgba8().get_pixel(2, 2),
            BG,
            "0% must not change the pixel"
        );

        let mut full = solid(64, 64, BG);
        apply(&mut full, &mark, Corner::TopLeft, 25, 100);
        assert_eq!(
            *full.to_rgba8().get_pixel(2, 2),
            MARK,
            "100% is the mark's own color"
        );

        let mut half = solid(64, 64, BG);
        apply(&mut half, &mark, Corner::TopLeft, 25, 50);
        let p = *half.to_rgba8().get_pixel(2, 2);
        assert!(
            p.0[0] > 0 && p.0[0] < 255,
            "50% should blend strictly between background and mark, got {p:?}"
        );
    }
}
