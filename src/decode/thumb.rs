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
        img.resize(cx, cx, FilterType::Lanczos3)
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
