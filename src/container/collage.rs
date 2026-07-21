//! Contact-sheet compositor: fold 2-4 decoded archive-preview images into one
//! square tile, so a multi-page container (CBZ/CB7/CBR and similar) can show
//! more than its single cover in Explorer. Speed over fidelity — this runs
//! against the thumbnail path, so filtering is cheap (`Triangle`, not
//! Lanczos3) and every dimension is bounds-checked (no panics on untrusted
//! input; the crate builds `panic = "abort"` inside Explorer).

use image::imageops::FilterType;
use image::{DynamicImage, Rgba, RgbaImage};

/// Transparent gap between cells, in physical px. No outer border.
const GUTTER: u32 = 2;

/// Cap on native-size magnification before a cell switches from cover-crop to
/// letterboxing (never blow a tiny source up past this).
const MAX_UPSCALE: f32 = 2.0;

const MIN_EDGE: u32 = 32;
const MAX_EDGE: u32 = 1024;

/// Compose 2-4 decoded images into a square contact-sheet tile of `edge` x `edge`
/// pixels. Layouts: 2 = side-by-side halves; 3 = one large left column + two
/// stacked right cells; 4 = 2x2 grid. Each cell is filled center-crop (cover
/// fit). Returns None if fewer than 2 images (caller uses the single-cover path).
pub fn compose(images: &[DynamicImage], edge: u32) -> Option<RgbaImage> {
    if images.len() < 2 {
        return None;
    }
    let n = images.len().min(4);
    let images = &images[..n];
    let edge = edge.clamp(MIN_EDGE, MAX_EDGE);

    let mut out = RgbaImage::from_pixel(edge, edge, Rgba([0, 0, 0, 0]));
    for (&(x, y, w, h), img) in layout(n, edge).iter().zip(images) {
        let cell = fit_cell(img, w, h);
        image::imageops::overlay(&mut out, &cell, x as i64, y as i64);
    }
    Some(out)
}

/// Cell rects (x, y, w, h) for an `n`-image tile of `edge` x `edge`, gutter
/// already subtracted. Rects tile `edge` exactly; the left/top cell absorbs
/// any odd-pixel remainder so nothing overlaps or falls short.
fn layout(n: usize, edge: u32) -> Vec<(u32, u32, u32, u32)> {
    let (w1, w2) = split(edge);
    let rx = w1 + GUTTER;
    match n {
        2 => vec![(0, 0, w1, edge), (rx, 0, w2, edge)],
        3 => {
            let (h1, h2) = split(edge);
            let ry = h1 + GUTTER;
            vec![(0, 0, w1, edge), (rx, 0, w2, h1), (rx, ry, w2, h2)]
        }
        4 => {
            let (h1, h2) = split(edge);
            let ry = h1 + GUTTER;
            vec![(0, 0, w1, h1), (rx, 0, w2, h1), (0, ry, w1, h2), (rx, ry, w2, h2)]
        }
        _ => Vec::new(),
    }
}

/// Split `total` into two adjacent spans separated by [`GUTTER`]; the first
/// span gets the odd-pixel remainder. `first + GUTTER + second == total`.
fn split(total: u32) -> (u32, u32) {
    let content = total.saturating_sub(GUTTER);
    let first = content - content / 2;
    let second = content / 2;
    (first, second)
}

/// Fill a `w` x `h` cell from `img`: cover-crop when the source is large
/// enough, else letterbox (contain fit, capped at [`MAX_UPSCALE`]) centered
/// over a transparent cell. Never panics on a zero-sized cell or source.
fn fit_cell(img: &DynamicImage, w: u32, h: u32) -> RgbaImage {
    let mut cell = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    let (sw, sh) = (img.width(), img.height());
    if w == 0 || h == 0 || sw == 0 || sh == 0 {
        return cell;
    }

    // Uniform scale that would let the source fully cover the cell.
    let cover_scale = (w as f32 / sw as f32).max(h as f32 / sh as f32);

    if cover_scale <= MAX_UPSCALE {
        // Crop the source to the cell's aspect ratio at native resolution
        // FIRST, then resize the (already cell-shaped) crop down — never
        // resize a huge source dimension before shrinking it.
        let (cw, ch) = crop_size_for_aspect(sw, sh, w, h);
        let cx = (sw - cw) / 2;
        let cy = (sh - ch) / 2;
        let cropped = image::imageops::crop_imm(img, cx, cy, cw, ch).to_image();
        return image::imageops::resize(&cropped, w, h, FilterType::Triangle);
    }

    // Source too small to cover without excessive upscale: contain-fit it
    // instead, capped at MAX_UPSCALE native size, and letterbox the rest.
    let contain_scale =
        ((w as f32 / sw as f32).min(h as f32 / sh as f32)).min(MAX_UPSCALE);
    let new_w = ((sw as f32 * contain_scale).round() as u32).clamp(1, w);
    let new_h = ((sh as f32 * contain_scale).round() as u32).clamp(1, h);
    let scaled = image::imageops::resize(img, new_w, new_h, FilterType::Triangle);
    let ox = ((w - new_w) / 2) as i64;
    let oy = ((h - new_h) / 2) as i64;
    image::imageops::overlay(&mut cell, &scaled, ox, oy);
    cell
}

/// Largest centered crop of a `sw` x `sh` source matching the `cw` x `ch`
/// cell's aspect ratio, clamped to the source bounds. Pure u64 math (bounded
/// by realistic decode dims; `image` crate caps inputs well under u32::MAX).
fn crop_size_for_aspect(sw: u32, sh: u32, cw: u32, ch: u32) -> (u32, u32) {
    let (sw64, sh64, cw64, ch64) = (sw as u64, sh as u64, cw as u64, ch as u64);
    // Compare source aspect to cell aspect (cross-multiplied, no division):
    // source wider than the cell crops its left/right edges (keep full
    // height); source taller crops top/bottom (keep full width).
    if sw64 * ch64 > sh64 * cw64 {
        let w = (sh64 * cw64 / ch64).clamp(1, sw64) as u32;
        (w, sh)
    } else {
        let h = (sw64 * ch64 / cw64).clamp(1, sh64) as u32;
        (sw, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(rgba)))
    }

    #[test]
    fn too_few_images_returns_none() {
        assert!(compose(&[], 100).is_none());
        assert!(compose(&[solid(10, 10, [255, 0, 0, 255])], 100).is_none());
    }

    #[test]
    fn layouts_tile_exactly_for_2_3_4() {
        for n in [2usize, 3, 4] {
            let rects = layout(n, 100);
            assert_eq!(rects.len(), n);
            for &(x, y, w, h) in &rects {
                assert!(x + w <= 100, "rect exceeds edge width");
                assert!(y + h <= 100, "rect exceeds edge height");
            }
        }
        // 2: full-height halves split by one gutter.
        let r2 = layout(2, 100);
        assert_eq!(r2[0], (0, 0, 49, 100));
        assert_eq!(r2[1], (51, 0, 49, 100));
        // 4: 2x2 grid, both axes split by one gutter.
        let r4 = layout(4, 101);
        assert_eq!(r4[0].2 + GUTTER + r4[1].2, 101); // row width
        assert_eq!(r4[0].3 + GUTTER + r4[2].3, 101); // column height
    }

    #[test]
    fn gutter_pixel_is_transparent() {
        let imgs = [solid(64, 64, [255, 0, 0, 255]), solid(64, 64, [0, 255, 0, 255])];
        let out = compose(&imgs, 100).expect("2 images compose");
        // x=49 is the gutter column between the two cells (see layout test).
        assert_eq!(out.get_pixel(49, 50)[3], 0);
        // Deep inside either cell should be fully opaque.
        assert_eq!(out.get_pixel(10, 50)[3], 255);
        assert_eq!(out.get_pixel(90, 50)[3], 255);
    }

    #[test]
    fn tiny_source_letterboxes_with_transparent_corners() {
        let tiny = solid(4, 4, [10, 20, 30, 255]);
        let cell = fit_cell(&tiny, 100, 100);
        // 4px native, capped at 2x -> an 8x8 block centered in the 100x100 cell.
        assert_eq!(cell.get_pixel(0, 0)[3], 0, "corner must stay transparent");
        assert_eq!(cell.get_pixel(99, 99)[3], 0, "corner must stay transparent");
        assert_eq!(cell.get_pixel(50, 50)[3], 255, "center holds the letterboxed image");
    }

    #[test]
    fn edge_is_clamped() {
        let imgs = [solid(64, 64, [1, 2, 3, 255]), solid(64, 64, [4, 5, 6, 255])];
        assert_eq!(compose(&imgs, 4).expect("clamped low").dimensions(), (MIN_EDGE, MIN_EDGE));
        assert_eq!(compose(&imgs, 5000).expect("clamped high").dimensions(), (MAX_EDGE, MAX_EDGE));
    }

    #[test]
    fn layout_dimensions_for_n3() {
        let rects = layout(3, 100);
        // Left column spans full height; right column is two stacked cells.
        assert_eq!(rects[0].3, 100);
        assert_eq!(rects[1].2, rects[2].2, "stacked right cells share width");
        assert_eq!(rects[1].1, 0);
        assert_eq!(rects[1].3 + GUTTER + rects[2].3, 100);
    }
}
