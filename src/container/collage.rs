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

/// Shared body of [`compose`] and [`compose_prepared`]: clamp the edge, reject
/// fewer than 2 images, then overlay the cell `cell_for` renders for every
/// layout rect. `None` from `cell_for` aborts the composite.
fn compose_tiles<I>(
    images: &[I],
    edge: u32,
    cell_for: impl Fn(&I, u32, u32) -> Option<RgbaImage>,
) -> Option<RgbaImage> {
    if images.len() < 2 {
        return None;
    }
    let n = images.len().min(4);
    let images = &images[..n];
    let edge = edge.clamp(MIN_EDGE, MAX_EDGE);

    let mut out = RgbaImage::from_pixel(edge, edge, Rgba([0, 0, 0, 0]));
    for (&(x, y, w, h), img) in layout(n, edge).iter().zip(images) {
        let cell = cell_for(img, w, h)?;
        image::imageops::overlay(&mut out, &cell, x as i64, y as i64);
    }
    Some(out)
}

/// Compose 2-4 decoded images into a square contact-sheet tile of `edge` x `edge`
/// pixels. Layouts: 2 = side-by-side halves; 3 = one large left column + two
/// stacked right cells; 4 = 2x2 grid. Each cell is filled center-crop (cover
/// fit). Returns None if fewer than 2 images (caller uses the single-cover path).
#[cfg(test)]
pub fn compose(images: &[DynamicImage], edge: u32) -> Option<RgbaImage> {
    compose_tiles(images, edge, |img, w, h| Some(fit_cell(img, w, h)))
}

/// A bounded rendering of one decoded cover. `alternate_square` exists only for
/// the mixed case where the source can cover a square cell but must letterbox in
/// a tall cell; the full-resolution source can still be dropped immediately.
pub struct PreparedSheetImage {
    image: RgbaImage,
    alternate_square: Option<RgbaImage>,
    original: (u32, u32),
    mode: PreparedMode,
}

#[derive(Clone, Copy)]
enum PreparedMode {
    /// The original can cover every cell shape at no more than 2x enlargement.
    Cover,
    /// The original must letterbox in every cell shape.
    Letterbox,
    /// It covers a square cell but letterboxes in a tall one.
    Mixed,
}

fn can_cover(sw: u32, sh: u32, w: u32, h: u32) -> bool {
    sw > 0 && sh > 0 && (w as f32 / sw as f32).max(h as f32 / sh as f32) <= MAX_UPSCALE
}

/// Render one freshly-decoded cover into a bounded representation.
///
/// Classifying the source while its original dimensions are still available is
/// important: tiny or extremely narrow sources retain the historical 2x-upscale
/// limit and letterboxing. A crop-only intermediate lost that decision and could
/// turn a tiny panorama into a filled/cropped cell.
///
/// Common sources require one resize pass, not two: either one aspect-bounded
/// cover crop or one contained letterbox source. Only the mixed case retains a
/// second, square-cropped variant. Every retained bitmap is bounded by `edge`, so
/// four full-resolution images can never accumulate in memory.
pub fn prepare_for_sheet(img: &DynamicImage, edge: u32) -> PreparedSheetImage {
    let edge = edge.clamp(MIN_EDGE, MAX_EDGE);
    let half = split(edge).0;
    let original = (img.width(), img.height());
    let tall_cover = can_cover(original.0, original.1, half, edge);
    let square_cover = can_cover(original.0, original.1, half, half);

    if tall_cover {
        // Covering the taller/more-demanding cell implies it can cover square
        // cells too. Keep one centered crop spanning the full [1:2, 1:1]
        // aspect range; later cells only crop this bounded representation.
        PreparedSheetImage {
            image: bounded_cover_source(img, edge),
            alternate_square: None,
            original,
            mode: PreparedMode::Cover,
        }
    } else if square_cover {
        // Preserve the full aspect for tall-cell letterboxing, plus one bounded
        // square crop for the cells the original can legitimately cover.
        PreparedSheetImage {
            image: bounded_contained_source(img, edge),
            alternate_square: Some(fit_cover(img, half, half)),
            original,
            mode: PreparedMode::Mixed,
        }
    } else {
        PreparedSheetImage {
            image: bounded_contained_source(img, edge),
            alternate_square: None,
            original,
            mode: PreparedMode::Letterbox,
        }
    }
}

/// `(width, height)` of `img`, or `None` when either edge is zero.
fn nonzero_dims(img: &DynamicImage) -> Option<(u32, u32)> {
    let (sw, sh) = (img.width(), img.height());
    (sw > 0 && sh > 0).then_some((sw, sh))
}

/// Center-crop the source only to the union of sheet cell aspects ([1:2, 1:1])
/// and shrink that region to the final edge. Used when every later fit is a
/// cover fit, so no letterboxed content can be discarded.
fn bounded_cover_source(img: &DynamicImage, edge: u32) -> RgbaImage {
    let Some((sw, sh)) = nonzero_dims(img) else {
        return RgbaImage::new(0, 0);
    };
    let (cw, ch) = if sw > sh {
        (sh, sh)
    } else if (sw as u64) * 2 < sh as u64 {
        (sw, sw.saturating_mul(2).min(sh))
    } else {
        (sw, sh)
    };
    let cx = (sw - cw) / 2;
    let cy = (sh - ch) / 2;
    let scale = (edge as f64 / cw as f64)
        .min(edge as f64 / ch as f64)
        .min(1.0);
    let w = ((cw as f64 * scale).round() as u32).max(1);
    let h = ((ch as f64 * scale).round() as u32).max(1);
    let view = image::imageops::crop_imm(img, cx, cy, cw, ch);
    image::imageops::resize(&*view, w, h, FilterType::Triangle)
}

/// Shrink the full source proportionally into the final edge. This preserves
/// all content needed by a later letterbox fit while bounding retained memory.
fn bounded_contained_source(img: &DynamicImage, edge: u32) -> RgbaImage {
    let Some((sw, sh)) = nonzero_dims(img) else {
        return RgbaImage::new(0, 0);
    };
    let scale = (edge as f64 / sw as f64)
        .min(edge as f64 / sh as f64)
        .min(1.0);
    let w = ((sw as f64 * scale).round() as u32).max(1);
    let h = ((sh as f64 * scale).round() as u32).max(1);
    image::imageops::resize(img, w, h, FilterType::Triangle)
}

/// Compose covers already reduced by [`prepare_for_sheet`]. This is the
/// memory-bounded production path.
pub fn compose_prepared(images: &[PreparedSheetImage], edge: u32) -> Option<RgbaImage> {
    compose_tiles(images, edge, |img, w, h| {
        Some(match img.mode {
            PreparedMode::Cover => fit_cover(&img.image, w, h),
            PreparedMode::Letterbox => fit_letterbox_prepared(&img.image, img.original, w, h),
            PreparedMode::Mixed if h > w => fit_letterbox_prepared(&img.image, img.original, w, h),
            PreparedMode::Mixed => {
                let square = img.alternate_square.as_ref()?;
                image::imageops::resize(square, w, h, FilterType::Triangle)
            }
        })
    })
}

/// Center-crop and resize without applying the native-size upscale decision.
/// The caller has already classified this prepared source using the ORIGINAL
/// dimensions, before it was reduced.
fn fit_cover<I>(img: &I, w: u32, h: u32) -> RgbaImage
where
    I: image::GenericImageView<Pixel = Rgba<u8>>,
{
    let (sw, sh) = (img.width(), img.height());
    if w == 0 || h == 0 || sw == 0 || sh == 0 {
        return RgbaImage::new(w, h);
    }
    let (cw, ch) = crop_size_for_aspect(sw, sh, w, h);
    let cx = (sw - cw) / 2;
    let cy = (sh - ch) / 2;
    let cropped = image::imageops::crop_imm(img, cx, cy, cw, ch);
    image::imageops::resize(&*cropped, w, h, FilterType::Triangle)
}

/// Resize `img` to `new_w` x `new_h` and center the result over the transparent
/// `cell` whose size is `w` x `h`.
fn overlay_centered<I>(cell: &mut RgbaImage, img: &I, new_w: u32, new_h: u32, w: u32, h: u32)
where
    I: image::GenericImageView<Pixel = Rgba<u8>>,
{
    let scaled = image::imageops::resize(img, new_w, new_h, FilterType::Triangle);
    let ox = ((w - new_w) / 2) as i64;
    let oy = ((h - new_h) / 2) as i64;
    image::imageops::overlay(cell, &scaled, ox, oy);
}

/// Letterbox a bounded source using its ORIGINAL dimensions for the same 2x
/// decision and output geometry as [`fit_cell`].
fn fit_letterbox_prepared(img: &RgbaImage, (sw, sh): (u32, u32), w: u32, h: u32) -> RgbaImage {
    let mut cell = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    if w == 0 || h == 0 || sw == 0 || sh == 0 || img.width() == 0 || img.height() == 0 {
        return cell;
    }
    let scale = ((w as f32 / sw as f32).min(h as f32 / sh as f32)).min(MAX_UPSCALE);
    let new_w = ((sw as f32 * scale).round() as u32).clamp(1, w);
    let new_h = ((sh as f32 * scale).round() as u32).clamp(1, h);
    overlay_centered(&mut cell, img, new_w, new_h, w, h);
    cell
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
            vec![
                (0, 0, w1, h1),
                (rx, 0, w2, h1),
                (0, ry, w1, h2),
                (rx, ry, w2, h2),
            ]
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
#[cfg(test)]
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
        let cropped = image::imageops::crop_imm(img, cx, cy, cw, ch);
        return image::imageops::resize(&*cropped, w, h, FilterType::Triangle);
    }

    // Source too small to cover without excessive upscale: contain-fit it
    // instead, capped at MAX_UPSCALE native size, and letterbox the rest.
    let contain_scale = ((w as f32 / sw as f32).min(h as f32 / sh as f32)).min(MAX_UPSCALE);
    let new_w = ((sw as f32 * contain_scale).round() as u32).clamp(1, w);
    let new_h = ((sh as f32 * contain_scale).round() as u32).clamp(1, h);
    overlay_centered(&mut cell, img, new_w, new_h, w, h);
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
        let imgs = [
            solid(64, 64, [255, 0, 0, 255]),
            solid(64, 64, [0, 255, 0, 255]),
        ];
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
        assert_eq!(
            cell.get_pixel(50, 50)[3],
            255,
            "center holds the letterboxed image"
        );
    }

    #[test]
    fn edge_is_clamped() {
        let imgs = [solid(64, 64, [1, 2, 3, 255]), solid(64, 64, [4, 5, 6, 255])];
        assert_eq!(
            compose(&imgs, 4).expect("clamped low").dimensions(),
            (MIN_EDGE, MIN_EDGE)
        );
        assert_eq!(
            compose(&imgs, 5000).expect("clamped high").dimensions(),
            (MAX_EDGE, MAX_EDGE)
        );
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

    #[test]
    fn prepared_sheet_sources_are_bounded() {
        let wide = solid(4000, 400, [1, 2, 3, 255]);
        let tall = solid(400, 4000, [4, 5, 6, 255]);
        let wide = prepare_for_sheet(&wide, 256);
        let tall = prepare_for_sheet(&tall, 256);

        assert!(matches!(wide.mode, PreparedMode::Cover));
        assert_eq!(wide.image.dimensions(), (256, 256));
        assert!(wide.alternate_square.is_none());
        assert!(matches!(tall.mode, PreparedMode::Cover));
        assert_eq!(tall.image.dimensions(), (128, 256));
        assert!(tall.alternate_square.is_none());
    }

    #[test]
    fn prepared_tiny_panorama_keeps_letterboxing() {
        let tiny = solid(100, 4, [10, 20, 30, 255]);
        let prepared = prepare_for_sheet(&tiny, 100);
        let sheet = compose_prepared(&[prepared, prepare_for_sheet(&tiny, 100)], 100)
            .expect("two prepared images compose");
        // The original fit_cell refuses to magnify the 4px height beyond 2x,
        // leaving transparent space above and below. Preparing must not crop
        // that source into a filled tall cell.
        assert_eq!(sheet.get_pixel(10, 0)[3], 0);
        assert_eq!(sheet.get_pixel(10, 50)[3], 255);
    }
}
