//! The `image`-crate tier: decode under the allocation budget, with the RAW-preview order threaded through.

use super::*;

/// Decode a standalone image file (the non-container path of `decode_full`).
#[cfg(test)]
pub(super) fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_image_with_raw_order(bytes, RawPreviewOrder::AfterExternal, None)
}

pub(super) fn decode_image_with_raw_order(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // Gzip-wrapped vector formats: `.svgz` (gzipped SVG) and `.emz` (gzipped
    // EMF/WMF metafile). The `image`/resvg tiers can't see through gzip and
    // ImageMagick has no EMZ coder, so inflate once (bounded) and decode the
    // inner bytes. We decode the inflated bytes inline — never re-entering on a
    // gzip magic — so a gzip-in-gzip payload can't recurse.
    let (svg_img, inner) = decode_svg_if_svg(bytes);
    if let Some(img) = svg_img {
        return Ok(img); // vector; no EXIF orientation
    }
    // 3D meshes (STL/OBJ/PLY): sniffed and RENDERED up front, mirroring the SVG shape —
    // these are geometry, not pixels, so no raster tier can touch them. Runs only in the
    // isolated hosts and the CLI (this prelude); `decode_menu_preview` deliberately skips
    // it, like video/PDF/magick — a 2M-triangle rasterization has no place in-process
    // inside explorer.exe, so the classic-menu tile stays caption-only for meshes.
    if let Some(img) = decode_mesh_sniffed(bytes) {
        return Ok(img); // rendered; no EXIF to apply
    }
    // `inner` is only `Some` when `bytes` was gzip-wrapped and inflated but wasn't SVG
    // (e.g. `.emz`) — decode THAT, so a gzip-in-gzip payload still can't recurse.
    if let Some(inner) = inner {
        return Ok(apply_exif_orientation(
            decode_any_with_wic_target(&inner, raw_preview, true, wic_thumbnail_cx)?,
            &inner,
        ));
    }
    Ok(apply_exif_orientation(
        decode_any_with_wic_target(bytes, raw_preview, true, wic_thumbnail_cx)?,
        bytes,
    ))
}

pub(super) fn decode_with_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_with_image_alloc(bytes, MAX_ALLOC)
}

/// As [`decode_with_image_alloc`] but with the embedded ICC profile left UN-applied,
/// returned alongside the decoded image instead. Lets a thumbnail caller reduce the
/// image first and run the (otherwise identical) colour transform on the small result
/// rather than the full-resolution one — see [`try_image_tier`].
pub(super) fn decode_with_image_alloc_raw(
    bytes: &[u8],
    max_alloc: u64,
) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    use image::ImageDecoder;
    use std::io::Cursor;
    // CMYK JPEGs: the image crate converts CMYK→RGB naively (ignoring the embedded CMYK
    // ICC) → wrong colors. Intercept + color-manage the raw CMYK ourselves; on any miss
    // fall through to the image crate's existing conversion (never worse than today).
    // This path is already fully colour-managed (CMYK has no separate reduce-first
    // optimization), so it reports no ICC left to apply.
    if is_cmyk_jpeg(bytes) {
        if let Some(img) = decode_cmyk_jpeg(bytes) {
            return Ok((img, None));
        }
    }
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::from(E_FAIL))?;
    // Explicit limits enforced during a single decode pass: reject oversized
    // dimensions and cap the decode allocation (no separate dimensions parse).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(max_alloc);
    // Decode via the decoder (not `reader.decode()`) so we can read the embedded ICC
    // profile and color-manage to sRGB before the pixels hit the resize/DIB path.
    let mut decoder = reader.into_decoder().map_err(|_| Error::from(E_FAIL))?;
    decoder
        .set_limits(limits)
        .map_err(|_| Error::from(E_FAIL))?;
    // `set_limits` above only guarantees MAX_DIM: `ImageDecoder::set_limits`'s DEFAULT
    // impl (used by decoders that don't override it, e.g. the HDR/Radiance codec) checks
    // dimensions only and never enforces `max_alloc` against the output buffer it is
    // about to materialize. A 16384x16384 frame is dimension-legal at MAX_DIM but, at
    // Rgb32F's 12 bytes/px, ~3.2 GiB — 6x this call's own budget. Check the buffer size
    // ourselves, from the header alone (before `from_decoder` allocates it), so every
    // decoder gets the same allocation ceiling regardless of whether it opted in.
    let (w, h) = decoder.dimensions();
    let bpp = u64::from(decoder.color_type().bytes_per_pixel());
    if exceeds_alloc_budget(w, h, bpp, max_alloc) {
        return Err(Error::from(E_FAIL));
    }
    // The TIFF decoder answers `None` for a profile the file plainly carries (see
    // `color::tiff_icc`), so the container is read directly when the decoder has nothing.
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .or_else(|| color::tiff_icc(bytes));
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    // An HDR PNG (a `cICP` chunk saying PQ or HLG) becomes display-linear float here, so
    // the float arm of the caller tone-maps it exactly like EXR/Radiance. `cICP` outranks
    // `iCCP` by specification, hence the dropped profile. SDR `cICP` values and every
    // non-PNG input fall through unchanged (see `cicp.rs`).
    if let Some(linear) = png_cicp(bytes).and_then(|c| cicp_hdr_to_linear(&img, &c)) {
        return Ok((linear, None));
    }
    Ok((img, icc))
}

/// Whether a `(w, h)` frame at `bytes_per_pixel` would allocate more than `max_alloc` —
/// pulled out of [`decode_with_image_alloc`] so the header-only bomb check is unit-testable
/// without decoding gigabytes of real pixel data (see the call site for why the check is
/// needed at all: not every decoder's `set_limits` enforces this itself).
pub(super) fn exceeds_alloc_budget(w: u32, h: u32, bytes_per_pixel: u64, max_alloc: u64) -> bool {
    u64::from(w)
        .saturating_mul(u64::from(h))
        .saturating_mul(bytes_per_pixel)
        > max_alloc
}

/// As [`decode_with_image`] but with an explicit allocation budget. Dimensions
/// are still bounded by [`limits::MAX_DIM`]; only the alloc ceiling varies (the
/// PSD-composite re-decode of OUR own bounded PNG passes a larger one).
pub(super) fn decode_with_image_alloc(bytes: &[u8], max_alloc: u64) -> Result<DynamicImage> {
    let (img, icc) = decode_with_image_alloc_raw(bytes, max_alloc)?;
    Ok(apply_icc_to_srgb(img, icc))
}
