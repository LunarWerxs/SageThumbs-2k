//! Encoding one image to one native target, with the per-format variants (lossy WebP, PNG compression level, PNM flavour).

use super::*;

/// Encode `img` to `path` as `format`, honoring the user's saved JPEG quality /
/// PNG compression settings (Options). WebP stays lossless (the quick verbs have
/// no quality knob).
pub(super) fn encode_to(
    img: &DynamicImage,
    format: ImageFormat,
    target_ext: &str,
    path: &Path,
) -> Result<()> {
    encode_to_opts(
        img,
        format,
        crate::settings::jpeg_quality(),
        crate::settings::png_level(),
        None,
        target_ext,
        path,
    )
}

/// Encode with EXPLICIT JPEG quality / PNG level (the Convert… dialog passes its
/// slider values; the verbs pass the saved settings). `webp_quality = Some(q)`
/// selects lossy WebP (libwebp) at quality `q`; `None` keeps WebP lossless (the
/// pure-Rust image encoder). ICO is capped to 256px.
pub(super) fn encode_to_opts(
    img: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    target_ext: &str,
    path: &Path,
) -> Result<()> {
    let file = std::fs::File::create(path)
        .map_err(|e| Error::new(E_FAIL, format!("create {}: {e}", path.display())))?;
    let mut w = std::io::BufWriter::new(file);
    // ICO frames are at most 256×256; downscale (preserving aspect) to fit.
    let resized;
    let img = if matches!(format, ImageFormat::Ico) && (img.width() > 256 || img.height() > 256) {
        resized = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
        &resized
    } else {
        img
    };
    encode_variant(
        &mut w,
        img,
        format,
        jpeg_quality,
        png_level,
        webp_quality,
        target_ext,
    )?;
    // Flush the buffered tail explicitly: BufWriter::drop discards flush errors,
    // so a disk-full on the final block would otherwise let the caller rename a
    // TRUNCATED temp file over the destination (breaking the atomic-write promise).
    w.flush()
        .map_err(|e| Error::new(E_FAIL, format!("flush {}: {e}", path.display())))?;
    Ok(())
}

/// Write `img` in `format` through the encoder each format needs, honoring the
/// explicit JPEG quality, PNG level and lossy-WebP selector.
pub(super) fn encode_variant(
    w: &mut std::io::BufWriter<std::fs::File>,
    img: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    target_ext: &str,
) -> Result<()> {
    // Only the (optional) lossy-WebP arm consults this; without that feature, WebP
    // is encoded losslessly via `image` and the quality is irrelevant.
    #[cfg(not(feature = "webp-lossy"))]
    let _ = webp_quality;
    let fail = |e: &dyn std::fmt::Display| Error::new(E_FAIL, format!("encode {format:?}: {e}"));
    match format {
        ImageFormat::Jpeg => img
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                w,
                jpeg_quality,
            ))
            .map_err(|e| fail(&e)),
        // Lossy WebP via libwebp (image-webp only encodes lossless). Smaller
        // files for photos; alpha is preserved. Optional: without `webp-lossy`,
        // WebP falls through to the lossless `other` arm (the `image` encoder).
        #[cfg(feature = "webp-lossy")]
        ImageFormat::WebP if webp_quality.is_some() => encode_lossy_webp(w, img, webp_quality),
        ImageFormat::Png => encode_png_variant(w, img, png_level),
        ImageFormat::OpenExr => encode_exr_bounded(w, img).map_err(|e| fail(&e)),
        ImageFormat::Hdr => encode_hdr_bounded(w, img).map_err(|e| fail(&e)),
        ImageFormat::Farbfeld => encode_farbfeld_streaming(w, img).map_err(|e| fail(&e)),
        ImageFormat::Pnm => encode_pnm_variant(w, img, target_ext),
        other => img.write_to(w, other).map_err(|e| fail(&e)),
    }
}

/// Encode lossy WebP via libwebp (`image-webp` only encodes lossless). libwebp rejects
/// edges > 16383; `encode()` looks infallible but `.unwrap()`s internally and the worker
/// thread has no `catch_unwind` (panic=abort), so an oversized image would abort the
/// whole batch — fail this one file cleanly instead.
#[cfg(feature = "webp-lossy")]
pub(super) fn encode_lossy_webp(
    w: &mut std::io::BufWriter<std::fs::File>,
    img: &DynamicImage,
    webp_quality: Option<u8>,
) -> Result<()> {
    let quality = match webp_quality {
        Some(quality) => quality,
        None => return Err(Error::new(E_FAIL, "lossy webp: no quality given")),
    };
    let (pw, ph) = (img.width(), img.height());
    if pw == 0 || ph == 0 || pw > 16383 || ph > 16383 {
        return Err(Error::new(
            E_FAIL,
            format!("lossy webp: {pw}x{ph} is outside libwebp's 16383 px limit"),
        ));
    }
    let rgba = img.to_rgba8();
    let mem = webp::Encoder::from_rgba(rgba.as_raw(), pw, ph).encode(quality.clamp(1, 100) as f32);
    w.write_all(&mem)
        .map_err(|e| Error::new(E_FAIL, format!("write lossy webp: {e}")))
}

/// PNG: `image`'s encoder takes a coarse Fast/Default/Best level, not the legacy 0-9
/// zlib scale, so map onto it.
pub(super) fn encode_png_variant(
    w: &mut std::io::BufWriter<std::fs::File>,
    img: &DynamicImage,
    png_level: u32,
) -> Result<()> {
    let ct = match png_level {
        0..=2 => image::codecs::png::CompressionType::Fast,
        7..=9 => image::codecs::png::CompressionType::Best,
        _ => image::codecs::png::CompressionType::Default,
    };
    img.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
        w,
        ct,
        image::codecs::png::FilterType::Adaptive,
    ))
    .map_err(|e| Error::new(E_FAIL, format!("encode png: {e}")))
}

/// PNM subtype by target extension: PAM/PPM get their own streaming encoders; anything
/// else preserves the prior dynamic behavior for PBM/PGM/general-PNM transforms, whose
/// subtype depends on their pixel type.
pub(super) fn encode_pnm_variant(
    w: &mut std::io::BufWriter<std::fs::File>,
    img: &DynamicImage,
    target_ext: &str,
) -> Result<()> {
    let is_pam = target_ext.eq_ignore_ascii_case("pam");
    let is_ppm = target_ext.eq_ignore_ascii_case("ppm");
    if is_pam {
        encode_pam_streaming(w, img).map_err(|e| Error::new(E_FAIL, format!("encode pam: {e}")))
    } else if is_ppm {
        encode_ppm_streaming(w, img).map_err(|e| Error::new(E_FAIL, format!("encode ppm: {e}")))
    } else {
        img.write_to(w, ImageFormat::Pnm)
            .map_err(|e| Error::new(E_FAIL, format!("encode pnm: {e}")))
    }
}
