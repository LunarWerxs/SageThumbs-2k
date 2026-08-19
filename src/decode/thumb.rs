//! Turning a decoded image into the tile the caller asked for.
//!
//! Fit-to-box, EXIF orientation, the pixel-art upscale rule, the fully-transparent
//! watchdog, the archive contact sheet, and the embedded-EXIF-thumbnail shortcut that
//! lets a small request skip a full multi-megapixel decode.

use super::*;

/// Decode + fit-to-box. When `use_embedded` is set and the request is small,
/// try the image's own embedded (EXIF) thumbnail first — much faster for big
/// photos — falling back to a full decode if there's no usable embedded one.
pub fn decode_thumbnail_opts(bytes: &[u8], cx: u32, use_embedded: bool) -> Result<Decoded> {
    let cx = cx.max(1);

    let img = if use_embedded && cx <= crate::settings::EMBEDDED_MAX_REQUEST {
        match embedded_thumbnail(bytes) {
            Some(t) => {
                crate::safety::log_debug("decode: used embedded EXIF thumbnail");
                t
            }
            None => decode_preview_thumbnail(bytes, cx)?,
        }
    } else {
        decode_preview_thumbnail(bytes, cx)?
    };

    let mut decoded = fit_to_box(img, cx);
    // Watchdog: a fully-transparent thumbnail is invisible. When the RGB planes are
    // ALSO empty it's a decode that "succeeded" into nothing — fail it so Explorer
    // shows the file's icon instead of caching a blank tile the user can't clear
    // without nuking the thumbnail cache. But when real RGB content IS present
    // (DDS texture maps, render passes — formats whose alpha channel isn't
    // transparency), show that content opaque instead: every image viewer renders
    // these files fine, so a default icon would read as "broken".
    //
    // This leg is ALSO what issue #17 reported as "transparent PNG thumbnails have a solid
    // black background": a PNG saved with its alpha zeroed but its colour still stored shows
    // that hidden colour, and exporters typically leave it black. It is deliberately still
    // here for every format, because the alternative is worse — gating it to non-PNG was
    // tried and reverted, since it turned a visible (if ugly) thumbnail into no thumbnail at
    // all, and a tile you can recognise beats a generic icon even when its backdrop is wrong.
    // A competing product renders the same files opaque too, which is the same call.
    //
    // Known consequence, not a bug to "fix" by rejecting: `ThumbChecker` cannot help these
    // files. `checkerpx::compose_under` runs after this and early-outs on an all-opaque
    // buffer — and it could not do better anyway, since compositing a checkerboard under an
    // image whose every pixel is transparent yields a bare checkerboard with the picture gone.
    if is_fully_transparent(&decoded.rgba) {
        if decoded
            .rgba
            .chunks_exact(4)
            .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        {
            crate::safety::log_debug(
                "decode: all-transparent but has RGB content — forcing opaque",
            );
            for px in decoded.rgba.chunks_exact_mut(4) {
                px[3] = 255;
            }
        } else {
            crate::safety::log_debug(
                "decode: thumbnail was fully transparent — rejecting as blank",
            );
            return Err(Error::from(E_FAIL));
        }
    }
    Ok(decoded)
}

/// Preview decode for the thumbnail provider. Unlike [`decode_preview`], this threads the
/// requested edge into WIC so formats handled only by OS codecs (HEIC/AVIF/RAW/JPEG 2000)
/// can scale before their RGBA pixels enter this process. Full-fidelity callers deliberately
/// continue through [`decode_preview`] with no target size.
pub(super) fn decode_preview_thumbnail(bytes: &[u8], cx: u32) -> Result<DynamicImage> {
    // Keep this entry's container/PDF/video behavior identical to `decode_preview`; only the
    // final WIC raster source receives the target edge. Transparent PSDs retain the preview
    // path's real-composite exception so their alpha is not flattened into the baked JPEG.
    if bytes.starts_with(b"8BPS") && crate::container::psd_has_alpha(bytes) {
        match decode_psd_composite(bytes) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!(
                "transparent PSD composite failed ({e}); using baked preview"
            )),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::BeforeExternal, Some(cx.max(1)))
}

/// True when every pixel is fully transparent (alpha 0) — i.e. nothing visible.
pub(super) fn is_fully_transparent(rgba: &[u8]) -> bool {
    !rgba.is_empty() && rgba.chunks_exact(4).all(|px| px[3] == 0)
}

/// Sources at or below this size (longest edge) are treated as pixel-art / icons and
/// integer-upscaled with Nearest so they stay crisp. Kept small on purpose: nearest-
/// upscaling a *small photo* would look blocky, so anything bigger is left native.
pub(super) const NEAREST_UPSCALE_MAX: u32 = 64;

/// Most a mid-size source is allowed to be enlarged by to fill the requested box.
///
/// Beyond this the source simply does not carry the detail: enlarging a 64 px cover 16× into a
/// 1024 px tile produces a soft rectangle that is worse than an honestly small one, and costs
/// the memory of a full-size buffer to do it. Within it — which is where the real cases sit,
/// e.g. Photoshop's baked 128/160/256 px preview resource against Explorer's 256 px request —
/// the enlargement is slight and the tile matches its neighbours.
pub(super) const MAX_UPSCALE_FACTOR: u32 = 4;

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

/// The box average itself: output pixel `(ox, oy)` is the mean of source block
/// `[ox*k, ox*k+k) x [oy*k, oy*k+k)`, clipped at the right and bottom edges so a size that is
/// not a multiple of `k` keeps its last partial block instead of being cropped. Rounded, not
/// truncated, so a flat area round-trips to its own colour.
///
/// One body per sample type rather than a generic, because the accumulator width is the whole
/// question: `k` is bounded only by the image dimension, so 16-bit samples need 64 bits to add
/// up a large block without wrapping, and paying that on the 8-bit path (which is the hot one)
/// would be a waste.
macro_rules! box_reduce {
    ($name:ident, $t:ty, $acc:ty) => {
        fn $name(src: &[$t], w: usize, h: usize, ch: usize, k: usize) -> Vec<$t> {
            let nw = w.div_ceil(k);
            let nh = h.div_ceil(k);
            let mut out = vec![0 as $t; nw * nh * ch];
            for oy in 0..nh {
                let y0 = oy * k;
                let y1 = (y0 + k).min(h);
                for ox in 0..nw {
                    let x0 = ox * k;
                    let x1 = (x0 + k).min(w);
                    let mut acc = [0 as $acc; 4];
                    for y in y0..y1 {
                        let row = &src[(y * w + x0) * ch..(y * w + x1) * ch];
                        for px in row.chunks_exact(ch) {
                            for (a, v) in acc.iter_mut().zip(px) {
                                *a += *v as $acc;
                            }
                        }
                    }
                    let n = ((x1 - x0) * (y1 - y0)) as $acc;
                    let d = (oy * nw + ox) * ch;
                    for (o, a) in out[d..d + ch].iter_mut().zip(acc) {
                        *o = ((a + n / 2) / n) as $t;
                    }
                }
            }
            out
        }
    };
}

box_reduce!(box_reduce_u8, u8, u32);
box_reduce!(box_reduce_u16, u16, u64);

/// The float twin of [`box_reduce`]. Written out rather than folded into the macro because the
/// mean is a plain division here: there is no rounding term, and clamping a linear-light HDR
/// value to an integer range is exactly what must NOT happen before the tone map runs.
fn box_reduce_f32(src: &[f32], w: usize, h: usize, ch: usize, k: usize) -> Vec<f32> {
    let nw = w.div_ceil(k);
    let nh = h.div_ceil(k);
    let mut out = vec![0f32; nw * nh * ch];
    for oy in 0..nh {
        let y0 = oy * k;
        let y1 = (y0 + k).min(h);
        for ox in 0..nw {
            let x0 = ox * k;
            let x1 = (x0 + k).min(w);
            let mut acc = [0f32; 4];
            for y in y0..y1 {
                let row = &src[(y * w + x0) * ch..(y * w + x1) * ch];
                for px in row.chunks_exact(ch) {
                    for (a, v) in acc.iter_mut().zip(px) {
                        *a += *v;
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1) as f32;
            let d = (oy * nw + ox) * ch;
            for (o, a) in out[d..d + ch].iter_mut().zip(acc) {
                *o = a / n;
            }
        }
    }
    out
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
pub(super) fn fit_to_box(img: DynamicImage, cx: u32) -> Decoded {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let img = if w > cx || h > cx {
        pre_reduce(img, cx).resize(cx, cx, FilterType::Lanczos3)
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

/// Decode a JPEG's embedded EXIF thumbnail (if any), applying the file's EXIF
/// orientation so it matches the full image. Best-effort: any malformation or
/// absence yields None and the caller does a full decode.
pub(super) fn embedded_thumbnail(bytes: &[u8]) -> Option<DynamicImage> {
    let jpeg = exif_thumbnail_jpeg(bytes)?;
    let img = decode_with_image(jpeg).ok()?;
    Some(apply_exif_orientation(img, bytes))
}

/// Find the embedded thumbnail JPEG inside a JPEG's APP1/"Exif\0\0" segment and
/// return a slice of `bytes` covering that thumbnail's own JPEG stream.
pub(super) fn exif_thumbnail_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(0..2)? != [0xFF, 0xD8] {
        return None; // not a JPEG → no EXIF thumbnail to find
    }
    let mut i = 2usize;
    loop {
        // Each marker is 0xFF <marker> <len-hi> <len-lo> ...
        if *bytes.get(i)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(i + 1)?;
        if marker == 0xD9 || marker == 0xDA {
            return None; // EOI / start-of-scan: past the metadata headers
        }
        let seg_len = u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?]) as usize;
        if seg_len < 2 {
            return None;
        }
        let body_start = i + 4;
        let seg_end = i + 2 + seg_len;
        if seg_end > bytes.len() {
            return None;
        }
        // Match the "Exif\0\0" id ONLY within this segment's own body — never
        // read past seg_end. Confining it here also guarantees body_start+6 <=
        // seg_end whenever it matches, so the slice below can't be start>end
        // (which would panic — and under panic=abort that aborts the host).
        if marker == 0xE1 && bytes.get(body_start..seg_end)?.starts_with(b"Exif\0\0") {
            return tiff_thumbnail(bytes.get(body_start + 6..seg_end)?);
        }
        i = seg_end;
    }
}

#[inline]
pub(super) fn r16(b: &[u8], off: usize, le: bool) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(if le {
        u16::from_le_bytes([s[0], s[1]])
    } else {
        u16::from_be_bytes([s[0], s[1]])
    })
}

#[inline]
pub(super) fn r32(b: &[u8], off: usize, le: bool) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(if le {
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        u32::from_be_bytes([s[0], s[1], s[2], s[3]])
    })
}

/// Walk the TIFF block (IFD0 → IFD1) for the thumbnail offset (0x0201) and
/// length (0x0202), returning the embedded JPEG slice. All offsets are relative
/// to the TIFF header (`tiff[0]`). Fully bounds-checked — never panics.
pub(super) fn tiff_thumbnail(tiff: &[u8]) -> Option<&[u8]> {
    let le = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if r16(tiff, 2, le)? != 42 {
        return None;
    }
    let ifd0 = r32(tiff, 4, le)? as usize;
    // IFD1 pointer follows IFD0's entries.
    let n0 = r16(tiff, ifd0, le)? as usize;
    let ifd1 = r32(tiff, ifd0 + 2 + n0 * 12, le)? as usize;
    if ifd1 == 0 {
        return None;
    }

    let n1 = r16(tiff, ifd1, le)? as usize;
    let (mut off, mut len) = (None, None);
    for e in 0..n1 {
        let entry = ifd1 + 2 + e * 12;
        match r16(tiff, entry, le)? {
            0x0201 => off = Some(r32(tiff, entry + 8, le)? as usize), // JPEGInterchangeFormat
            0x0202 => len = Some(r32(tiff, entry + 8, le)? as usize), // …Length
            _ => {}
        }
    }
    let (off, len) = (off?, len?);
    let end = off.checked_add(len)?;
    let thumb = tiff.get(off..end)?;
    // Sanity: a real embedded thumbnail is itself a JPEG.
    if thumb.get(0..2)? == [0xFF, 0xD8] {
        Some(thumb)
    } else {
        None
    }
}

/// Map the 8 EXIF orientation values onto `image` transforms. Phone JPEGs
/// commonly use value 6 (rotate 90° CW). `rotate90` here is clockwise.
pub(super) fn apply_exif_orientation(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    match exif_orientation(bytes) {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.rotate90().fliph(),
        Some(6) => img.rotate90(),
        Some(7) => img.rotate270().fliph(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

pub(super) fn exif_orientation(bytes: &[u8]) -> Option<u32> {
    // Magic-gate before handing the bytes to `exif::Reader`: it only reads EXIF from
    // JPEG / TIFF / PNG / WebP / HEIF, returning an error (→ None) for anything else.
    // Skipping the reader setup for the formats it can't read (GIF/BMP/ICO/QOI/TGA/
    // PNM/DDS/…) is behavior-identical and saves a parse attempt on every such
    // thumbnail. (PNG/WebP/HEIF stay in — they CAN carry an EXIF orientation.)
    if !has_exif_container(bytes) {
        return None;
    }
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

/// True if `bytes` is one of the containers `exif::Reader` can read (JPEG, TIFF,
/// PNG, WebP, HEIF/HEIC/AVIF) — the only formats that can carry an EXIF orientation.
pub(super) fn has_exif_container(b: &[u8]) -> bool {
    b.len() >= 12
        && (b.starts_with(&[0xFF, 0xD8])                       // JPEG
            || b.starts_with(b"II*\0")                         // TIFF little-endian
            || b.starts_with(b"MM\0*")                         // TIFF big-endian
            || b.starts_with(&[0x89, b'P', b'N', b'G'])        // PNG (eXIf chunk)
            || (b.starts_with(b"RIFF") && &b[8..12] == b"WEBP") // WebP
            || &b[4..8] == b"ftyp") // ISOBMFF: HEIF/HEIC/AVIF
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    /// A deterministic photographic source: smooth large-scale structure plus per-pixel noise.
    /// Flat or purely smooth content would let ANY reduction look correct; the high-frequency
    /// half is what separates a filter that antialiases from one that does not.
    fn photographic(w: u32, h: u32) -> DynamicImage {
        let mut buf = image::RgbImage::new(w, h);
        let mut state = 0x9E37_79B9u32;
        for (x, y, p) in buf.enumerate_pixels_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as i32 - 128;
            use std::f32::consts::TAU;
            let sx = ((x as f32 / w as f32) * TAU).sin();
            let sy = ((y as f32 / h as f32) * TAU * 1.5).cos();
            let base = 128.0 + 90.0 * sx * sy;
            let v = |o: i32| (base as i32 + o + noise / 3).clamp(0, 255) as u8;
            *p = image::Rgb([v(0), v(20), v(-25)]);
        }
        DynamicImage::ImageRgb8(buf)
    }

    /// The pre-reduction is a SPEED change, and a speed change that quietly altered every
    /// thumbnail would be a bad trade. This pins how far it can move the picture: the two-pass
    /// result is compared against the single-pass filter it replaces, on content chosen to be
    /// hostile to a box filter.
    #[test]
    fn the_pre_reduction_barely_moves_the_picture() {
        // 2048 is the comfortable case (a 5x step, 1.6x left over) and 768 is the softest
        // admitted (a 2x step, 1.5x left over, so the box filter does the largest share of
        // the work it ever will).
        let (mut mean, mut worst) = (0.0f64, 0u32);
        for edge in [2048u32, 768] {
            let img = photographic(edge, edge * 3 / 4);
            let one_pass = img.resize(256, 256, FilterType::Lanczos3).to_rgba8();
            let two_pass = pre_reduce(img, 256)
                .resize(256, 256, FilterType::Lanczos3)
                .to_rgba8();
            assert_eq!(one_pass.dimensions(), two_pass.dimensions());

            let (mut sum, mut w) = (0u64, 0u32);
            for (a, b) in one_pass.as_raw().iter().zip(two_pass.as_raw()) {
                let d = u32::from(a.abs_diff(*b));
                sum += u64::from(d);
                w = w.max(d);
            }
            let m = sum as f64 / one_pass.as_raw().len() as f64;
            eprintln!("pre-reduction delta at {edge} px: mean {m:.4}, worst {w}");
            mean = mean.max(m);
            worst = worst.max(w);
        }
        assert!(
            mean <= MEAN_DELTA_CEILING,
            "the pre-reduction moved the average channel by {mean:.4}, over the {MEAN_DELTA_CEILING} this is allowed to"
        );
        assert!(
            worst <= WORST_DELTA_CEILING,
            "the pre-reduction moved one channel by {worst}, over the {WORST_DELTA_CEILING} this is allowed to"
        );
    }

    /// Where the fit's time actually goes, for a size the speed corpus says matters. Banked as
    /// a measurement rather than a gate: the split between the box pass and the real filter is
    /// what decides whether [`pre_reduce`] is worth its existence, and guessing it wrong is how
    /// an "optimisation" ends up slower than what it replaced.
    ///
    /// `text
    /// cargo test --release --lib -p sagethumbs2k fit_cost_split -- --ignored --nocapture
    /// `
    #[test]
    #[ignore = "measurement, not a gate; run --release --nocapture"]
    fn fit_cost_split() {
        use std::time::Instant;
        for (w, h) in [(1279u32, 1280u32), (2048, 1536), (4000, 3000)] {
            let img = photographic(w, h);
            let best = |mut f: Box<dyn FnMut()>| {
                let mut best = f64::MAX;
                for _ in 0..5 {
                    let t = Instant::now();
                    f();
                    best = best.min(t.elapsed().as_secs_f64() * 1000.0);
                }
                best
            };
            let a = img.clone();
            let one = best(Box::new(move || {
                let _ = a.resize(256, 256, FilterType::Lanczos3);
            }));
            let b = img.clone();
            let boxed = best(Box::new(move || {
                let _ = pre_reduce(b.clone(), 256);
            }));
            let c = img.clone();
            let two = best(Box::new(move || {
                let _ = pre_reduce(c.clone(), 256).resize(256, 256, FilterType::Lanczos3);
            }));
            println!(
                "{w}x{h}: single-pass {one:.1} ms | box {boxed:.1} ms | box+filter {two:.1} ms"
            );
        }
    }

    /// The float arms exist so the caller can reduce BEFORE tone-mapping, which is only correct
    /// if the reduction stays in linear light and in full float range. A path that clamped to
    /// [0,1], or that dropped to 8 bits on the way, would quietly destroy exactly the highlight
    /// detail an HDR file is kept for - and it would do it invisibly, since the tone map that
    /// runs afterwards compresses the range anyway.
    #[test]
    fn float_buffers_are_reduced_in_linear_light_and_full_range() {
        let mut buf = image::ImageBuffer::<image::Rgb<f32>, Vec<f32>>::new(1024, 1024);
        for (x, _y, p) in buf.enumerate_pixels_mut() {
            // Red ramps far ABOVE 1.0 (a real Radiance sun is thousands); green is a constant
            // well above white; blue stays sub-unit so a clamp in either direction shows up.
            *p = image::Rgb([x as f32 * 10.0, 4096.0, 0.25]);
        }
        let reduced = pre_reduce(DynamicImage::ImageRgb32F(buf), 256);
        assert_eq!((reduced.width(), reduced.height()), (512, 512));
        let DynamicImage::ImageRgb32F(out) = reduced else {
            panic!("a float image must stay float through the reduction");
        };
        // 1024 / (256 * 3 / 2) = 2, so output column 1 is the mean of source columns 2 and 3:
        // (20 + 30) / 2 = 25. Arithmetic mean, in linear light, not clamped.
        let px = out.get_pixel(1, 0).0;
        assert!(
            (px[0] - 25.0).abs() < 1e-3,
            "red must be the linear mean of 20 and 30, got {}",
            px[0]
        );
        assert!(
            (px[1] - 4096.0).abs() < 1e-3,
            "a constant far above 1.0 must survive unclamped, got {}",
            px[1]
        );
        assert!(
            (px[2] - 0.25).abs() < 1e-6,
            "a sub-unit constant must survive exactly, got {}",
            px[2]
        );
    }

    /// A reduction must not upscale, must not fire when there is nothing to win, and must
    /// leave at least [`PRE_REDUCE_GAP`] times over for the real filter.
    #[test]
    fn the_pre_reduction_fires_only_where_it_pays() {
        let big = photographic(2048, 1536);
        let reduced = pre_reduce(big, 256);
        assert_eq!(
            (reduced.width(), reduced.height()),
            (410, 308),
            "2048 px at a 256 px ask reduces by 5, leaving a 1.6x gap"
        );
        assert!(reduced.width() * 2 >= 256 * PRE_REDUCE_GAP_HALVES);

        // The softest case admitted: 768 px steps by 2, leaving exactly the 1.5x gap and no
        // more. This is the boundary  also
        // measures, so the softest result this can produce is a pinned one.
        let worst_case = pre_reduce(photographic(768, 768), 256);
        assert_eq!((worst_case.width(), worst_case.height()), (384, 384));

        // Short of a full second step the image is untouched, which keeps the band where the
        // single-pass filter is still cheap on the single-pass filter.
        for edge in [256u32, 400, 511, 767] {
            let img = photographic(edge, edge);
            let same = pre_reduce(img, 256);
            assert_eq!(
                (same.width(), same.height()),
                (edge, edge),
                "{edge} px at a 256 px ask has no whole-number step worth taking"
            );
        }
    }

    /// 16-bit is the worst case for the single-pass filter and the one every scanner and most
    /// PNG/TIFF writers produce, so its buffer must be reduced too, in its own precision.
    #[test]
    fn sixteen_bit_buffers_are_reduced_in_sixteen_bit() {
        let mut buf = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::new(1024, 1024);
        for (x, _y, p) in buf.enumerate_pixels_mut() {
            // A ramp far above 8-bit resolution: neighbouring columns differ by 64, which is
            // a quarter of one 8-bit step, so a path that dropped to 8 bits would flatten it.
            *p = image::Rgb([(x * 64) as u16, 30_000, 65_535 - (x * 64) as u16]);
        }
        let reduced = pre_reduce(DynamicImage::ImageRgb16(buf), 256);
        assert_eq!((reduced.width(), reduced.height()), (512, 512));
        let DynamicImage::ImageRgb16(out) = reduced else {
            panic!("a 16-bit image must stay 16-bit through the reduction");
        };
        // Output column 1 covers source columns 2 and 3, whose red is 128 and 192.
        assert_eq!(out.get_pixel(1, 0).0[0], 160);
        assert_eq!(out.get_pixel(1, 0).0[1], 30_000);
    }
}
