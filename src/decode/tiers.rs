//! The small, self-contained decode tiers: JPEG XL, the camera-RAW embedded-preview
//! carver, and headerless TGA. Each is signature- or heuristic-gated and either decodes
//! or fails fast, so [`super::decode_any_with_wic_target`] can try them in order without
//! any of them owning the dispatch.

use super::*;

/// JPEG XL signature: a bare codestream (`FF 0A`) or the ISOBMFF container's `JXL `
/// box header (`00 00 00 0C  4A 58 4C 20  0D 0A 87 0A`). A cheap gate so the decoder
/// is only ever handed actual jxl bytes.
pub(super) fn is_jxl(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x0A])
        || bytes.starts_with(&[
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ])
}

/// Decode JPEG XL via the pure-Rust `jxl-oxide` crate (its `image`-crate
/// `ImageDecoder` integration). jxl has no other tier here — the `image` crate and
/// WIC both lack it and the shipped magick drops the coder. Bomb-guarded exactly like
/// the other tiers (per-edge [`MAX_DIM`], total [`MAX_PIXELS`], [`MAX_ALLOC`] per
/// allocation). HDR jxl decodes to 32-bit float and is tone-mapped to 8-bit sRGB the
/// same way the EXR/Radiance path is. `rayon` is compiled out, so no global thread
/// pool lands inside explorer.exe.
pub(super) fn decode_jxl(bytes: &[u8]) -> Result<DynamicImage> {
    use image::ImageDecoder;
    let mut decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::from(E_FAIL))?;
    // Reject an oversized canvas before allocating the framebuffer (matches the WIC
    // tier's guard: per-edge MAX_DIM and total MAX_PIXELS).
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::from(E_FAIL));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    decoder
        .set_limits(limits)
        .map_err(|_| Error::from(E_FAIL))?;
    // COLOUR-MANAGE, exactly like the `image` and WIC tiers do (`decode.rs`, `wic.rs`).
    // Without this, a jxl whose colour encoding is not sRGB — which is most of them once
    // someone encodes from a wide-gamut source, and the whole point of modular/lossless
    // workflows — was handed to Explorer as if its numbers WERE sRGB, so the thumbnail came
    // out visibly shifted while every other viewer showed it correctly (issue #9). Must be
    // read BEFORE `from_decoder`, which consumes the decoder.
    let icc = decoder.icc_profile().ok().flatten();
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    if matches!(
        img,
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
    ) {
        // Tone-map first: the float path lands in sRGB, so managing it afterwards would
        // apply the source profile's transfer curve on top of one already applied.
        return Ok(tone_map_float(&img));
    }
    Ok(apply_icc_to_srgb(img, icc))
}

/// Smallest embedded JPEG we'll treat as a real RAW preview. A tiny ~160px EXIF
/// thumbnail is only ~5–15 KB; a "real" camera preview is hundreds of KB to several
/// MB. Below this we return None so the caller demosaics for full resolution instead
/// of converting/thumbnailing from a postage-stamp.
pub(crate) const MIN_RAW_PREVIEW: usize = 16 * 1024;

/// Last-resort floor: when no "real" preview (≥ [`MIN_RAW_PREVIEW`]) exists AND every
/// external decoder (WIC / ImageMagick) has failed or is absent — the common case on a
/// clean compact install with no Microsoft RAW Image Extension — accept even a small
/// embedded JPEG (a camera's ~160px EXIF thumbnail) so the RAW shows *something* rather
/// than a blank tile. A valid JPEG this small is still ~2–10 KB; below this is noise.
pub(super) const LENIENT_RAW_PREVIEW: usize = 2 * 1024;

/// A preview larger than this is almost certainly a FULL-resolution JPEG (tens of MP)
/// — slow to decode in pure Rust and far bigger than a thumbnail (or a convenience
/// convert) needs. We prefer the largest preview AT OR BELOW this cap — a camera's
/// screen-size "review" JPEG (~2–6 MP, decodes in ~100 ms) — and only fall back to an
/// oversized one when nothing real is under it (correctness over speed). This is what
/// keeps full-res-preview RAW (.pef/.cr2) snappy without losing those that only ship a
/// big preview.
pub(super) const PREVIEW_SOFT_MAX: usize = 1024 * 1024;

/// Decode a camera-RAW (or any container with a baked-in JPEG) by carving out its
/// LARGEST embedded JPEG preview and decoding that — instead of demosaicing the raw
/// sensor data via WIC/ImageMagick. The carved JPEG is re-decoded through the safe
/// `image` tier (bomb-guard limits apply). Returns Err when there's no real embedded
/// preview, so [`decode_any`] falls through to the WIC/magick tiers unchanged.
pub(super) fn decode_raw_preview(bytes: &[u8], thumbnail_cx: Option<u32>) -> Result<DynamicImage> {
    let jpeg = largest_embedded_jpeg(bytes, MIN_RAW_PREVIEW).ok_or_else(|| Error::from(E_FAIL))?;
    // The carved preview can be a FULL-RESOLUTION JPEG, and for some cameras it is the only
    // one: [`PREVIEW_SOFT_MAX`] above prefers a screen-size "review" JPEG, but a body that
    // ships none leaves the oversized fallback as the honest pick. Measured on a Canon 5D
    // Mark II CR2, whose only previews are 160x120 and 5616x3744 — nothing in between — the
    // full-res carve costs ~1.0 s against ~3 ms for a Nikon NEF that happens to embed a
    // 1632x1080 one. That gap is Canon's file layout, not a defect in the pick.
    //
    // What it IS, though, is a large JPEG headed for a small tile, which is exactly the
    // bargain the DCT-scaled decode exists for — asking the codec for a reduced resolution
    // level rather than every pixel. The floor inside `wic_scaled_from_bytes_if_codec_scales`
    // keeps the mid-size previews (a few hundred KB) on the pure-Rust tier where they are
    // already fast, so only the oversized carve pays the COM round trip. Any failure falls
    // through to the decode that shipped, so no RAW that rendered before can stop rendering.
    if let Some(cx) = thumbnail_cx {
        if let Some(img) = wic_scaled_from_bytes_if_codec_scales(jpeg, cx) {
            return Ok(img);
        }
    }
    decode_with_image(jpeg)
}

/// Pick the best embedded JPEG preview in `data` and return a slice of it, or None if
/// there's no real preview (≥ [`MIN_RAW_PREVIEW`]). "Best" = the largest one at or
/// below [`PREVIEW_SOFT_MAX`] (a fast, ample screen-size preview), falling back to the
/// largest overall only when nothing fits under the cap. Each candidate's true length
/// is measured by walking the JPEG marker structure to its real end-of-image
/// ([`jpeg_span_len`]), so a stray `FF D9` inside an APPn/EXIF metadata segment can't
/// truncate the pick. Bounded: the 0xFF scan is linear, and at most 64 SOI candidates
/// are examined so a hostile file can't make this loop.
pub(crate) fn largest_embedded_jpeg(data: &[u8], min_size: usize) -> Option<&[u8]> {
    // `capped` = largest preview within [MIN, SOFT_MAX] (what we prefer); `overall` =
    // largest ≥ MIN (the fallback when every real preview is oversized).
    let mut capped: Option<(usize, usize)> = None;
    let mut overall: Option<(usize, usize)> = None;
    let mut i = 0usize;
    let mut seen = 0usize;
    while i + 2 < data.len() {
        // Jump to the next 0xFF (the compiler vectorizes this) — most bytes aren't,
        // so this skips the bulk of a multi-MB RAW without touching it.
        match data[i..data.len() - 2].iter().position(|&b| b == 0xFF) {
            Some(rel) => i += rel,
            None => break,
        }
        if data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            // SOI (FF D8 FF…). Measure it; a valid JPEG is skipped whole.
            match jpeg_span(data, i) {
                // A structurally perfect JPEG we cannot decode is worse than no candidate at
                // all: picking it costs the whole tier. Canon CR2 is the case — its raw sensor
                // data is a ~20 MB LOSSLESS JPEG (SOF3) with a valid marker chain, so it wins
                // "largest embedded JPEG" over the real 3 MB display preview, and both the
                // `image` crate and WIC then reject it ("the image header is unrecognized").
                // The tier failed outright and a Canon RAW fell through to a ~900 ms WIC
                // demosaic — while the preview it wanted sat right there. Skipping the frame
                // still advances `i` by its measured span, so this costs nothing.
                Some((len, Some(sof))) if !jpeg_sof_is_decodable(sof) => {
                    i += len;
                    seen += 1;
                    if seen >= 64 {
                        break;
                    }
                    continue;
                }
                Some((len, _)) => {
                    if len >= min_size {
                        if match overall {
                            None => true,
                            Some((_, bl)) => len > bl,
                        } {
                            overall = Some((i, len));
                        }
                        if len <= PREVIEW_SOFT_MAX
                            && match capped {
                                None => true,
                                Some((_, bl)) => len > bl,
                            }
                        {
                            capped = Some((i, len));
                        }
                    }
                    i += len;
                }
                None => i += 1,
            }
            seen += 1;
            if seen >= 64 {
                break;
            }
        } else {
            i += 1;
        }
    }
    let (start, len) = capped.or(overall)?;
    data.get(start..start.checked_add(len)?)
}

/// Decode a headerless Truevision TGA (and its `.icb`/`.vda`/`.vst` aliases) when
/// the content passes a TGA header check — `image` needs the format told to it.
pub(super) fn decode_tga(bytes: &[u8]) -> Result<DynamicImage> {
    if !looks_like_tga(bytes) {
        return Err(Error::from(E_FAIL));
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Tga);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut img = reader.decode().map_err(|_| Error::from(E_FAIL))?;
    // Classic TGA gotcha: a 32-bpp file whose image-descriptor byte declares 0
    // attribute (alpha) bits carries a meaningless 4th channel — very often all
    // zero. The `image` crate maps 32-bpp straight to RGBA8 trusting that byte,
    // which renders such files fully transparent (the blank-thumbnail watchdog
    // then rejects them, and Convert/View write see-through PNGs). Honor the
    // header instead: 0 declared alpha bits ⇒ the channel is filler ⇒ opaque.
    if bytes.len() >= 18 && bytes[16] == 32 && bytes[17] & 0x0F == 0 {
        if let DynamicImage::ImageRgba8(buf) = &mut img {
            for px in buf.pixels_mut() {
                px.0[3] = 255;
            }
        }
    }
    Ok(img)
}

/// Heuristic TGA detector (the format carries no signature): the v2 footer is
/// definitive; otherwise validate the 18-byte header's fixed-range fields.
pub(super) fn looks_like_tga(b: &[u8]) -> bool {
    if b.len() >= 18 && &b[b.len() - 18..b.len() - 2] == b"TRUEVISION-XFILE" {
        return true;
    }
    if b.len() < 18 {
        return false;
    }
    let w = u16::from_le_bytes([b[12], b[13]]);
    let h = u16::from_le_bytes([b[14], b[15]]);
    b[1] <= 1 // color-map type (0 = none, 1 = present)
        && matches!(b[2], 1 | 2 | 3 | 9 | 10 | 11) // image type
        && matches!(b[16], 8 | 15 | 16 | 24 | 32) // bits per pixel
        && w > 0
        && h > 0
}
