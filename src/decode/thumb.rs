//! Turning a decoded image into the tile the caller asked for.
//!
//! Fit-to-box, EXIF orientation, the pixel-art upscale rule and the own-picture cap that
//! keeps it to stand-ins, the fully-transparent watchdog, the archive contact sheet, and the
//! embedded-EXIF-thumbnail shortcut that lets a small request skip a full multi-megapixel
//! decode.

use super::*;
// Not `exif`: a child by that name would shadow the `exif` crate for everything under it.
mod exifthumb;
pub(crate) use exifthumb::exif_orientation;
#[cfg(test)]
pub(super) use exifthumb::exif_thumbnail_jpeg;
#[cfg(test)]
use exifthumb::tiff_ifd0_orientation;
pub(super) use exifthumb::{apply_exif_orientation, embedded_thumbnail};

/// Decode + fit-to-box. When `use_embedded` is set and the request is small,
/// try the image's own embedded (EXIF) thumbnail first — much faster for big
/// photos — falling back to a full decode if there's no usable embedded one.
pub fn decode_thumbnail_opts(bytes: &[u8], cx: u32, use_embedded: bool) -> Result<Decoded> {
    let cx = cx.max(1);
    let img = embedded_or_preview(bytes, cx, use_embedded)?;
    let box_edge = tile_box_edge(bytes, &img, cx);
    let mut decoded = fit_to_box(img, box_edge);
    resolve_transparency(&mut decoded)?;
    Ok(decoded)
}

/// Pick the decode source for [`decode_thumbnail_opts`]: the embedded (EXIF) thumbnail when the
/// caller asked for it and the request is small enough, else a full preview decode.
fn embedded_or_preview(bytes: &[u8], cx: u32, use_embedded: bool) -> Result<DynamicImage> {
    if use_embedded && cx <= crate::settings::EMBEDDED_MAX_REQUEST {
        match embedded_thumbnail(bytes) {
            Some(t) => {
                crate::safety::log_debug("decode: used embedded EXIF thumbnail");
                Ok(t)
            }
            None => decode_preview_thumbnail(bytes, cx),
        }
    } else {
        decode_preview_thumbnail(bytes, cx)
    }
}

/// The box edge [`decode_thumbnail_opts`] fits to.
///
/// A tile is never larger than the picture it shows. `fit_to_box` fills the box from an
/// undersized source because that source normally STANDS IN for something larger (a
/// Photoshop file's baked preview, a book's cover, a decode scaled toward the request;
/// issue #25), and a stand-in drawn at its own size misstates the file. The file's own
/// picture is no stand-in: Windows draws a 32 px PNG at 32 px in the middle of the cell,
/// and so does this, since a desktop of small pictures blown up to their tiles is what an
/// uninstall note of 2026-09-15 called "modified". Icons stay scalable, see
/// [`is_the_files_own_picture`]. The probe only runs when the decode is smaller than the
/// request, so a picture that has to shrink pays nothing.
fn tile_box_edge(bytes: &[u8], img: &DynamicImage, cx: u32) -> u32 {
    let long = img.width().max(img.height());
    if long < cx && is_the_files_own_picture(bytes, img) {
        long
    } else {
        cx
    }
}

/// Watchdog: a fully-transparent thumbnail is invisible. When the RGB planes are
/// ALSO empty it's a decode that "succeeded" into nothing — fail it so Explorer
/// shows the file's icon instead of caching a blank tile the user can't clear
/// without nuking the thumbnail cache. But when real RGB content IS present
/// (DDS texture maps, render passes — formats whose alpha channel isn't
/// transparency), show that content opaque instead: every image viewer renders
/// these files fine, so a default icon would read as "broken".
///
/// This leg is ALSO what issue #17 reported as "transparent PNG thumbnails have a solid
/// black background": a PNG saved with its alpha zeroed but its colour still stored shows
/// that hidden colour, and exporters typically leave it black. It is deliberately still
/// here for every format, because the alternative is worse — gating it to non-PNG was
/// tried and reverted, since it turned a visible (if ugly) thumbnail into no thumbnail at
/// all, and a tile you can recognise beats a generic icon even when its backdrop is wrong.
/// A competing product renders the same files opaque too, which is the same call.
///
/// Known consequence, not a bug to "fix" by rejecting: `ThumbChecker` cannot help these
/// files. `checkerpx::compose_under` runs after this and early-outs on an all-opaque
/// buffer — and it could not do better anyway, since compositing a checkerboard under an
/// image whose every pixel is transparent yields a bare checkerboard with the picture gone.
fn resolve_transparency(decoded: &mut Decoded) -> Result<()> {
    if !is_fully_transparent(&decoded.rgba) {
        return Ok(());
    }
    if decoded
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
    {
        crate::safety::log_debug("decode: all-transparent but has RGB content — forcing opaque");
        let (chunks, _) = decoded.rgba.as_chunks_mut::<4>();
        for px in chunks {
            px[3] = 255;
        }
        Ok(())
    } else {
        crate::safety::log_debug("decode: thumbnail was fully transparent — rejecting as blank");
        Err(Error::from(E_FAIL))
    }
}

/// Preview decode for the thumbnail provider. Unlike [`decode_preview`], this threads the
/// requested edge into WIC so formats handled only by OS codecs (HEIC/AVIF/RAW/JPEG 2000)
/// can scale before their RGBA pixels enter this process. Full-fidelity callers deliberately
/// continue through [`decode_preview`] with no target size.
pub(super) fn decode_preview_thumbnail(bytes: &[u8], cx: u32) -> Result<DynamicImage> {
    // Keep this entry's container/PDF/video behavior identical to `decode_preview`; only the
    // final WIC raster source receives the target edge. PSDs are the one exception, for the
    // two reasons in `psd_composite_wanted`.
    let cx = cx.max(1);
    if bytes.starts_with(b"8BPS") && psd_composite_wanted(bytes, cx) {
        match decode_psd_composite(bytes, Fidelity::Tile) {
            Ok(img) => match composite_beats_baked_preview(img, bytes) {
                CompositeVerdict::UseComposite(img) => return Ok(img),
                // Reuse the decode `composite_beats_baked_preview` already did to answer
                // its own question, instead of falling through to the normal PSD path
                // below and decoding the same baked JPEG a second time (G145a).
                CompositeVerdict::UseBakedPreview(preview) => return Ok(preview),
            },
            Err(e) => crate::safety::log_debugf!("PSD composite failed ({e}); using baked preview"),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::BeforeExternal, Some(cx))
}

/// Verdict from [`composite_beats_baked_preview`]: which already-decoded image the caller
/// should use. Both variants carry a ready `DynamicImage` — the baked-preview arm exists so
/// the caller never has to decode it a second time through the normal PSD path.
enum CompositeVerdict {
    UseComposite(DynamicImage),
    UseBakedPreview(DynamicImage),
}

/// Is the composite we just decoded actually WORTH having, next to the preview it would
/// replace?
///
/// **A successful decode is not evidence of a picture, and this is measured rather than
/// feared.** Handed a PSD whose merged image-data section is absent or empty, the bundled
/// ImageMagick does not fail — it reports success and returns a frame of solid black. That
/// was reproduced directly against `magick "file.psd[0]"`, and it is the same shape as the
/// three bugs `scripts\check-render-sanity.ps1` was built for, and as the Kodak `.dcr`
/// placeholder that made [`super::reduced_ifd0_serves`] require content as well as size.
///
/// It matters here precisely BECAUSE of the issue-#33 fix above: an opaque PSD never reached
/// the composite at thumbnail sizes before, so this failure mode is one the fix introduces
/// and therefore one the fix has to close. Trading Photoshop's real 160 px preview for a
/// black rectangle would be a straight regression on any document whose merged data the
/// writer left out — the "Maximize Compatibility" checkbox is exactly that choice.
///
/// The flat-composite branch is the only one that pays for a second decode, and it is rare.
/// A genuinely single-colour document lands there and keeps its composite, because its baked
/// preview is that same single colour and has nothing more to offer.
///
/// Takes `composite` by value and hands it back inside the verdict (G145a): the caller no
/// longer needs to hold its own copy just to return it, and when the baked preview wins,
/// THAT decode — already paid for here to answer this very question — comes back too,
/// instead of the caller decoding the identical baked JPEG a second time through the
/// normal PSD path.
fn composite_beats_baked_preview(composite: DynamicImage, bytes: &[u8]) -> CompositeVerdict {
    // `REDUCED_IFD0_MIN_SD` is reused deliberately rather than copied to a new name: it is
    // calibrated for a different format but answers the identical question — does this
    // bitmap hold a picture, or is it a rectangle of one colour? A flat fill scores 0.00.
    if luma_sd(&composite) >= REDUCED_IFD0_MIN_SD {
        return CompositeVerdict::UseComposite(composite);
    }
    match crate::container::psd_baked_preview(bytes).and_then(|p| decode_with_image(&p).ok()) {
        Some(preview) if luma_sd(&preview) >= REDUCED_IFD0_MIN_SD => {
            CompositeVerdict::UseBakedPreview(preview)
        }
        // Either the baked preview is itself flat, or there's nothing to fall back to —
        // either way a flat composite still beats no thumbnail.
        _ => CompositeVerdict::UseComposite(composite),
    }
}

/// Should a PSD/PSB thumbnail render the real merged composite rather than the ~160 px JPEG
/// Photoshop bakes into image resource 1036?
///
/// Two independent reasons, and the second one is issue #33:
///
/// 1. **The document is transparent.** The baked preview is a JPEG, which has no alpha, so a
///    background-removed document would thumbnail against flat white. Predates #33.
/// 2. **The preview is too small for what was asked for.** It is a fixed ~160 px whatever the
///    canvas is, so it answers an icon view honestly and a 2048 px preview pane not at all —
///    [`fit_to_box`] will not enlarge it that far, so the pane got a 160 px bitmap centred in
///    it and stayed blurry no matter how long the user waited. This mirrors the guard the RAW
///    path already applies (`super::reduced_ifd0_serves`): use the file's own preview when it
///    is genuinely big enough for the request, and render properly when it is not.
///
/// A PSD with no measurable baked preview answers `false` and takes the unchanged path: there
/// is nothing for a composite to be better *than*, and the ordinary tiers already reach
/// ImageMagick for it.
///
/// The composite is best-effort in both cases. When it fails — no ImageMagick on a compact
/// install, or a document magick cannot open — the caller falls straight through to the baked
/// preview, so nothing that produced a thumbnail before can stop producing one.
fn psd_composite_wanted(bytes: &[u8], cx: u32) -> bool {
    crate::container::psd_has_alpha(bytes)
        || crate::container::psd_preview_long_edge(bytes)
            .is_some_and(|edge| !embedded_preview_serves(edge, cx))
}

/// True when every pixel is fully transparent (alpha 0) — i.e. nothing visible.
pub(super) fn is_fully_transparent(rgba: &[u8]) -> bool {
    !rgba.is_empty() && rgba.as_chunks::<4>().0.iter().all(|px| px[3] == 0)
}

/// Are `img`'s pixels the file's own picture, rather than a stand-in for a larger one?
///
/// Decided by size: the decode is the picture when its edges are exactly what the file's
/// header declares (`declared_dimensions`, the same header-only probe `decode_full_for_output`
/// uses to refuse a preview standing in for the picture), compared as long and short edge so
/// an EXIF rotation applied on the way does not read as a mismatch. Everything a tier hands
/// back INSTEAD of the picture (a baked preview, a container's cover, a WIC decode scaled toward
/// the request, an EXIF thumbnail) has some other size, and a header no reader can parse
/// answers `None`, so an unknown format keeps filling the tile as it always did rather than
/// being guessed at.
///
/// An icon file is never "the picture": an `.ico` carries several sizes and is drawn at
/// whatever size the view asks for, which is how Windows draws one too, so it keeps the
/// pixel-art enlargement.
fn is_the_files_own_picture(bytes: &[u8], img: &DynamicImage) -> bool {
    if image::guess_format(bytes).is_ok_and(|f| f == image::ImageFormat::Ico) {
        return false;
    }
    let edges = |w: u32, h: u32| (w.max(h), w.min(h));
    super::declared_dimensions(bytes).map(|(w, h)| edges(w, h))
        == Some(edges(img.width(), img.height()))
}

/// Sources at or below this size (longest edge) are treated as pixel-art / icons and
/// integer-upscaled with Nearest so they stay crisp. Kept small on purpose: nearest-
/// upscaling a *small photo* would look blocky, so anything bigger is left native. Reached
/// only by a stand-in or an icon: a file's own small picture is capped at its size before
/// [`fit_to_box`] is asked (see [`decode_thumbnail_opts`]).
pub(super) const NEAREST_UPSCALE_MAX: u32 = 64;

/// Most a mid-size source is allowed to be enlarged by to fill the requested box.
///
/// Beyond this the source simply does not carry the detail: enlarging a 64 px cover 16× into a
/// 1024 px tile produces a soft rectangle that is worse than an honestly small one, and costs
/// the memory of a full-size buffer to do it. Within it — which is where the real cases sit,
/// e.g. Photoshop's baked 128/160/256 px preview resource against Explorer's 256 px request —
/// the enlargement is slight and the tile matches its neighbours.
pub(super) const MAX_UPSCALE_FACTOR: u32 = 4;

/// May a container's baked-in preview, whose long edge is `preview_edge`, answer a request for
/// a `cx`-px tile — or must the caller go and render the real picture instead?
///
/// **The predicate is [`fit_to_box`]'s own, deliberately, and that is the whole design.** The
/// enlargement branch there fills the box only while `cx <= long * MAX_UPSCALE_FACTOR`; past
/// that it hands the small bitmap back untouched, and Explorer centres it in the cell rather
/// than scaling it. So a preview outside this range does not merely produce a soft tile, it
/// produces one that is *the wrong size as well*, which is the shape of issue #25 all over
/// again. Asking the same question the fit step will ask is what makes the two agree: when
/// this says yes, the caller gets back exactly the `cx`-px tile it asked for.
///
/// Two things it is NOT:
///
/// * **Not a content test.** The RAW twin ([`super::reduced_ifd0_serves`]) also requires the
///   preview to contain a picture, because a Kodak `.dcr` ships a blank one and being large is
///   not the same as having something in it. That failure mode belongs to camera firmware
///   writing a placeholder; Photoshop's resource 1036 is the document, and the decoded pixels
///   are not in hand at the point this is asked anyway. If a blank baked preview is ever
///   reported, the answer is to add the same `luma_sd` floor here, not to widen this.
/// * **Not for full-fidelity callers.** They pass no target at all and never reach this —
///   Convert and Resize want the real image at real resolution, whatever its size.
pub fn embedded_preview_serves(preview_edge: u32, cx: u32) -> bool {
    preview_edge > 0 && cx <= preview_edge.saturating_mul(MAX_UPSCALE_FACTOR)
}

/// How much reduction is left for the real filter after [`pre_reduce`] has done the integer
/// part, in halves: 3 means the filter still gets at least a 1.5x reduction to do.
///
/// The number is a measured trade, not a convention. [`fit_cost_split`] puts the single-pass
/// filter at 118 ms on a 1.6 MP image, 209 ms at 3.1 MP and 815 ms at 12 MP, so this band is
/// worth real time; [`the_pre_reduction_barely_moves_the_picture`] puts the cost of buying it
/// at a mean channel difference of 0.96/255 with a 2x gap and 1.70/255 with a 1.5x one, on
/// content chosen to be hostile to a box filter.
///
/// 1.5 rather than 2 because of where the cliff falls. Taking a whole second step needs the
/// source at twice the gap, so a 2x gap would mean nothing under 1024 px is ever reduced, and
/// 768 to 1024 px is where an enormous share of real images sit, every one of them paying the
/// full single-pass price. Pillow's `reducing_gap` defaults to 2.0 for the same trick, but it
/// governs one explicit user request rather than every tile in a folder view.
const PRE_REDUCE_GAP_HALVES: u32 = 3;

/// How far the pre-reduction is allowed to move a thumbnail against the single-pass filter
/// it replaces, as measured by `fit_tests::the_pre_reduction_barely_moves_the_picture` on
/// noisy photographic content. Recorded rather than assumed: if a future change to either
/// pass moves the picture further than this, that is a decision to take deliberately.
#[cfg(test)]
const MEAN_DELTA_CEILING: f64 = 2.0;
#[cfg(test)]
const WORST_DELTA_CEILING: u32 = 16;

/// Shrink by a whole-number factor with a box average before the real filter runs.
///
/// A big decoded image costs about as much to REDUCE as it did to decode. Measured on the
/// 12 MP tier of the speed corpus, the fit alone was ~95 ms: TIFF decoded in 50 ms and then
/// spent 95 ms shrinking, so most of the gap to Windows was the reduction rather than the
/// codec. The cause is structural, not a bad filter choice - `image`'s resampler scales its
/// kernel support with the ratio, so a 15x reduction reads about 70 source pixels per output
/// pixel per axis, and reducing by 2x costs a fifteenth of what reducing by 15x does.
///
/// So the integer part is done first, in one cheap pass: each output pixel is the exact mean
/// of the k-by-k source block it covers, which IS the correct prefilter for a k-times
/// reduction. Lanczos then finishes the remaining (at least [`PRE_REDUCE_GAP`]-times)
/// reduction over a fraction of the data. This is the standard shrink-then-resample used by
/// Pillow (`reducing_gap`), libvips and JPEG's own DCT scaling, and the reason the gap is left
/// at all is that the box filter's stopband is poor: finishing with a real filter is what
/// keeps the result sharp rather than blocky.
///
/// Every sample type is handled: 8-bit, 16-bit, and the 32-bit linear floats an HDR decode
/// produces. The float arms matter for a reason the integer ones do not - the caller reduces
/// BEFORE tone-mapping, so the averaging happens in linear light, which is both the physically
/// correct order and what [`super::exrscale::decode_scaled`] has always done for OpenEXR.
///
/// Every integer sample type is handled, 8-bit and 16-bit alike. 16-bit is not an exotic
/// corner here: ImageMagick, scanners and most PNG/TIFF writers produce it by default, and it
/// is the WORST case, because the single-pass filter then does all that work on twice the
/// data. Float buffers are left alone; they reach this point already tone-mapped.
pub(super) fn pre_reduce(img: DynamicImage, cx: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    // The largest whole-number step that still leaves the filter its gap. Truncated, so
    // the gap is a floor and never a hope: a step is taken only when the result genuinely
    // still covers it.
    let span = (cx.max(1).saturating_mul(PRE_REDUCE_GAP_HALVES) / 2).max(1);
    let k = w.max(h) / span;
    if k < 2 {
        return img;
    }
    let (k, w, h) = (k as usize, w as usize, h as usize);
    let nw = w.div_ceil(k) as u32;
    let nh = h.div_ceil(k) as u32;
    match img {
        DynamicImage::ImageLuma8(b) => {
            let out = box_reduce_u8(b.as_raw(), w, h, 1, k);
            DynamicImage::ImageLuma8(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageLumaA8(b) => {
            let out = box_reduce_u8(b.as_raw(), w, h, 2, k);
            DynamicImage::ImageLumaA8(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgb8(b) => {
            let out = box_reduce_u8(b.as_raw(), w, h, 3, k);
            DynamicImage::ImageRgb8(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgba8(b) => {
            let out = box_reduce_u8(b.as_raw(), w, h, 4, k);
            DynamicImage::ImageRgba8(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageLuma16(b) => {
            let out = box_reduce_u16(b.as_raw(), w, h, 1, k);
            DynamicImage::ImageLuma16(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageLumaA16(b) => {
            let out = box_reduce_u16(b.as_raw(), w, h, 2, k);
            DynamicImage::ImageLumaA16(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgb16(b) => {
            let out = box_reduce_u16(b.as_raw(), w, h, 3, k);
            DynamicImage::ImageRgb16(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgba16(b) => {
            let out = box_reduce_u16(b.as_raw(), w, h, 4, k);
            DynamicImage::ImageRgba16(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgb32F(b) => {
            let out = box_reduce_f32(b.as_raw(), w, h, 3, k);
            DynamicImage::ImageRgb32F(rebuilt(b, nw, nh, out))
        }
        DynamicImage::ImageRgba32F(b) => {
            let out = box_reduce_f32(b.as_raw(), w, h, 4, k);
            DynamicImage::ImageRgba32F(rebuilt(b, nw, nh, out))
        }
        other => other,
    }
}

/// Wrap a reduced sample buffer back into an image, keeping the original if the dimensions
/// somehow do not account for it (they always do; this is the arithmetic's own safety net).
fn rebuilt<P>(
    original: image::ImageBuffer<P, Vec<P::Subpixel>>,
    w: u32,
    h: u32,
    out: Vec<P::Subpixel>,
) -> image::ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel,
{
    image::ImageBuffer::from_raw(w, h, out).unwrap_or(original)
}

/// Visit every output pixel of a box reduction in row-major order, handing `visit` the output
/// index and the source block `[x0, x1) x [y0, y1)` it averages: the `k`-by-`k` block starting
/// at `(ox*k, oy*k)`, clipped at the right and bottom edges so a size that is not a multiple of
/// `k` keeps its last partial block instead of being cropped. The index is `oy * nw + ox` for
/// an output `w/k` pixels wide, i.e. `index * channels` is the first output sample.
fn for_each_block(
    w: usize,
    h: usize,
    k: usize,
    mut visit: impl FnMut(usize, usize, usize, usize, usize),
) {
    let nw = w.div_ceil(k);
    let nh = h.div_ceil(k);
    for oy in 0..nh {
        let y0 = oy * k;
        let y1 = (y0 + k).min(h);
        for ox in 0..nw {
            let x0 = ox * k;
            let x1 = (x0 + k).min(w);
            visit(oy * nw + ox, x0, x1, y0, y1);
        }
    }
}

/// Add one source row's samples into a four-channel accumulator, `channels` at a time.
fn accumulate_row<T, A>(acc: &mut [A; 4], row: &[T], channels: usize)
where
    T: Copy + Into<A>,
    A: std::ops::AddAssign,
{
    for px in row.chunks_exact(channels) {
        for (a, v) in acc.iter_mut().zip(px) {
            *a += (*v).into();
        }
    }
}

/// The box average itself: output pixel `(ox, oy)` is the mean of source block
/// `[ox*k, ox*k+k) x [oy*k, oy*k+k)`, clipped at the right and bottom edges so a size that is
/// not a multiple of `k` keeps its last partial block instead of being cropped. Rounded, not
/// truncated, so a flat area round-trips to its own colour.
///
/// One body per sample type rather than a generic, so each instantiation keeps its own
/// concrete `$t`/`$acc` pair. Both the 8-bit and 16-bit accumulators are 64-bit: `k` is
/// bounded only by the image dimension (a small requested edge against a large source
/// drives it close to MAX_DIM), and even 8-bit samples can sum past `u32::MAX` in one
/// block at that size — see the size note beside the instantiations below.
macro_rules! box_reduce {
    ($name:ident, $t:ty, $acc:ty) => {
        fn $name(src: &[$t], w: usize, h: usize, ch: usize, k: usize) -> Vec<$t> {
            let nw = w.div_ceil(k);
            let nh = h.div_ceil(k);
            let mut out = vec![0 as $t; nw * nh * ch];
            for_each_block(w, h, k, |i, x0, x1, y0, y1| {
                let mut acc = [0 as $acc; 4];
                for y in y0..y1 {
                    let row = &src[(y * w + x0) * ch..(y * w + x1) * ch];
                    accumulate_row(&mut acc, row, ch);
                }
                let n = ((x1 - x0) * (y1 - y0)) as $acc;
                let d = i * ch;
                for (o, a) in out[d..d + ch].iter_mut().zip(acc) {
                    *o = ((a + n / 2) / n) as $t;
                }
            });
            out
        }
    };
}

// Both accumulators are u64: `k` is bounded only by the image dimension (a tiny requested
// edge against a large source drives it close to MAX_DIM), so an 8-bit block sum can reach
// ~2.7e8 samples * 255 ~= 6.9e10 — already past u32::MAX on its own, before the 16-bit twin's
// wider samples are even considered. u32 here silently wrapped (release has no overflow
// check), producing a wrong-but-plausible-looking average rather than a crash.
box_reduce!(box_reduce_u8, u8, u64);
box_reduce!(box_reduce_u16, u16, u64);

/// The float twin of [`box_reduce`]. Written out rather than folded into the macro because the
/// mean is a plain division here: there is no rounding term, and clamping a linear-light HDR
/// value to an integer range is exactly what must NOT happen before the tone map runs.
fn box_reduce_f32(src: &[f32], w: usize, h: usize, ch: usize, k: usize) -> Vec<f32> {
    let nw = w.div_ceil(k);
    let nh = h.div_ceil(k);
    let mut out = vec![0f32; nw * nh * ch];
    for_each_block(w, h, k, |i, x0, x1, y0, y1| {
        let acc = box_block_mean_f32(src, w, ch, x0, x1, y0, y1);
        let d = i * ch;
        for (o, a) in out[d..d + ch].iter_mut().zip(acc) {
            *o = a;
        }
    });
    out
}

/// Per-channel mean over the source block `x0..x1` x `y0..y1` (inclusive of `x0`/`y0`, exclusive
/// of `x1`/`y1`), the inner step of [`box_reduce_f32`]. Empty blocks divide by one, matching the
/// `max(1)` the inlined caller used.
fn box_block_mean_f32(
    src: &[f32],
    w: usize,
    ch: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
) -> [f32; 4] {
    let mut acc = [0f32; 4];
    for y in y0..y1 {
        let row = &src[(y * w + x0) * ch..(y * w + x1) * ch];
        accumulate_row(&mut acc, row, ch);
    }
    let n = ((x1 - x0) * (y1 - y0)).max(1) as f32;
    for a in &mut acc {
        *a /= n;
    }
    acc
}

/// Fit within a `cx`-by-`cx` box, preserving aspect ratio. Large images shrink with
/// Lanczos3; tiny pixel-art / icons are integer-upscaled with Nearest so they render
/// crisp instead of bilinear-smeared; mid-size images are enlarged to FILL the box with
/// Lanczos3, up to [`MAX_UPSCALE_FACTOR`].
///
/// # Why mid-size images are no longer left native (issue #25)
///
/// This used to return anything already inside the box untouched, on the assumption that
/// "Explorer scales". Explorer does not enlarge a thumbnail — it centres the bitmap it was
/// given inside the icon cell. So a source smaller than the requested `cx` drew as a SMALLER
/// TILE than its neighbours, in the same view, at the same icon size.
///
/// That is exactly what the issue reported, and Photoshop files are where it shows worst:
/// `container::psd` returns the preview resource Photoshop baked into the file, whose size
/// depends on the writing application, the file's version, and whether "Maximize
/// Compatibility" was on. So one PSD yielded a full-size tile and the PSD beside it yielded a
/// half-size one, with nothing about the two files explaining the difference to the user.
///
/// It is also the same failure the file-size cap was raised to avoid (see
/// `settings::DEFAULT_MAX_FILE_MB`): an undersized bitmap is one the shell can neither draw
/// crisply nor durably cache, so it re-extracts on every refresh.
///
/// # Why the file's own picture is still left native (2026-09-15)
///
/// The #25 reasoning is about STAND-INS: a baked preview, a cover, a scaled decode, all of
/// which are smaller than the thing they represent. A picture that simply IS small is not
/// misstated by a small tile; Windows draws it at its real size in the middle of the cell,
/// and enlarging it turned a desktop of small PNGs into blocky or soft tiles (an uninstall
/// note called them "modified"). [`decode_thumbnail_opts`] tells the two apart by size
/// ([`is_the_files_own_picture`]) and caps `cx` at the picture's own edge for the file's own
/// picture, so this function's enlargement only ever runs for a stand-in or an icon.
/// Shrink to fit inside `nw` x `nh`, preserving aspect ratio, and NEVER enlarge.
///
/// **This is the one reduction in the product.** It used to be two: the shell extension came
/// through [`fit_to_box`] (Lanczos3, with the integer box pre-pass), while `cli::thumbnail`,
/// `cli::view_png`, the right-click preview tile and the Quick preview's display cap each
/// finished with `DynamicImage::thumbnail` — a cheaper box average. Since every visual gate in
/// the repo drives the CLI, the picture they validated was not the picture Explorer drew, and
/// the gap was measured at its WORST on the corpus's own 512x384 samples: mean 4.37 and worst
/// 21 channel levels, against the +/-8 `compare-renders.py` calls a match. See
/// `fit_tests::the_gates_reduce_a_thumbnail_the_way_the_shell_extension_does`, which is now a
/// pixel-equality check rather than a tolerance.
///
/// Non-square on purpose: the right-click tile fits 220x88 and the Quick preview caps at
/// 2048x4096, so a `cx`-only entry point would have left those two on the old filter and kept
/// half the problem. `pre_reduce` is sized from the aspect-preserving OUTPUT rather than from
/// `nw`/`nh` directly, because on a non-square box only the output says what the real
/// reduction ratio is.
///
/// It does not enlarge, which is what lets the CLI share it: `fit_to_box` deliberately fills
/// the box for the shell (issue #25 — Explorer centres an undersized tile instead of scaling
/// it), and `st2k thumbnail` deliberately does not, since `--size` there is a CEILING and
/// handing back an upscaled file would be inventing pixels the user did not ask for.
pub fn reduce_to_fit(img: DynamicImage, nw: u32, nh: u32) -> DynamicImage {
    let (nw, nh) = (nw.max(1), nh.max(1));
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || (w <= nw && h <= nh) {
        return img;
    }
    let scale = (f64::from(nw) / f64::from(w)).min(f64::from(nh) / f64::from(h));
    let ow = ((f64::from(w) * scale).round() as u32).max(1);
    let oh = ((f64::from(h) * scale).round() as u32).max(1);
    pre_reduce(img, ow.max(oh)).resize(nw, nh, FilterType::Lanczos3)
}

pub(super) fn fit_to_box(img: DynamicImage, cx: u32) -> Decoded {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let img = if w > cx || h > cx {
        reduce_to_fit(img, cx, cx)
    } else if w > 0 && h > 0 && long <= NEAREST_UPSCALE_MAX && long * 2 <= cx {
        // Tiny sprite/icon: scale by the largest integer factor that fits, with Nearest
        // (integer + Nearest = perfectly crisp pixels, no blur). Checked BEFORE the general
        // enlargement below so pixel art keeps its hard edges instead of being smoothed.
        let factor = cx / long;
        img.resize_exact(w * factor, h * factor, FilterType::Nearest)
    } else if w > 0 && h > 0 && long < cx && cx <= long.saturating_mul(MAX_UPSCALE_FACTOR) {
        // Mid-size: enlarge to fill the box so the tile is the size the shell asked for.
        // `resize` preserves aspect ratio, so the long edge lands exactly on `cx`.
        img.resize(cx, cx, FilterType::Lanczos3)
    } else {
        img
    };
    // Move the buffer out when it's already RGBA8 (the WIC tier always is, and the
    // no-upscale path keeps the decoded buffer) instead of cloning it via to_rgba8().
    match img {
        DynamicImage::ImageRgba8(buf) => Decoded {
            width: buf.width(),
            height: buf.height(),
            rgba: buf.into_raw(),
        },
        other => {
            let rgba = other.to_rgba8();
            Decoded {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
            }
        }
    }
}

/// Fit an already-decoded image (e.g. a Media Foundation video frame, which doesn't come
/// from the byte-based `decode_*` path) into a `cx`-by-`cx` thumbnail. Public so the
/// thumbnail provider's video branch can reuse the same resize → `Decoded` step.
pub fn thumbnail_from_image(img: DynamicImage, cx: u32) -> Decoded {
    fit_to_box(img, cx.max(1))
}

/// Compose a generic archive's picked images (.zip/.rar/.7z contact sheet) into one
/// `cx`-square thumbnail. Each cover decodes through the CHEAP tiers only (`image`
/// crate → WIC → TGA — archive members are ordinary JPEG/PNG/WebP files; no
/// subprocess, no video/PDF); one that fails to decode is dropped rather than
/// failing the sheet. A single survivor degrades to the normal aspect-preserving
/// single-cover fit, so the tile never shows a mostly-empty grid.
pub fn thumbnail_from_covers(covers: &[Vec<u8>], cx: u32) -> Result<Decoded> {
    let edge = cx.max(1);
    if covers.len() == 1 {
        return decode_cover(&covers[0]).map(|img| fit_to_box(img, edge));
    }

    // Decode one cover at a time and immediately reduce it to the largest region
    // any collage cell can use. Only these bounded (<= edge-square) intermediates
    // remain in the Vec; the full-resolution image drops before the next decode.
    let mut imgs: Vec<(usize, crate::container::collage::PreparedSheetImage)> = covers
        .iter()
        .enumerate()
        .filter_map(|(i, bytes)| {
            let img = decode_cover(bytes).ok()?;
            Some((i, crate::container::collage::prepare_for_sheet(&img, edge)))
        })
        .collect();
    match imgs.len() {
        0 => Err(Error::from(E_FAIL)),
        // Preserve the historical single-survivor aspect-fit. Re-decode only in
        // this uncommon fallback (multiple candidates were supplied but all save
        // one failed); the normal one-cover path returned above.
        1 => decode_cover(&covers[imgs[0].0]).map(|img| fit_to_box(img, edge)),
        _ => {
            let prepared: Vec<crate::container::collage::PreparedSheetImage> =
                imgs.drain(..).map(|(_, img)| img).collect();
            let sheet = crate::container::collage::compose_prepared(&prepared, edge)
                .ok_or_else(|| Error::from(E_FAIL))?;
            Ok(Decoded {
                width: sheet.width(),
                height: sheet.height(),
                rgba: sheet.into_raw(),
            })
        }
    }
}

#[cfg(test)]
mod fit_tests;
