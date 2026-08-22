//! Decode → (optional resize / flatten) → encode primitives: the `Target` /
//! `Resize` / `ConvertOpts` descriptors, the size-capped reader, the atomic
//! encode-to-file path, and the per-file convert / transform / resize / email
//! entry points the menu actions and the CLI dispatch to.

use std::{
    io::{Seek, Write},
    path::{Path, PathBuf},
};

use image::{DynamicImage, ImageFormat};
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;

use super::menu::{EmailSize, Transform};
use crate::decode;

/// A conversion target: the image-crate format and the file extension to use.
#[derive(Clone, Copy)]
pub struct Target {
    pub format: ImageFormat,
    pub ext: &'static str,
    /// `Some(q)` selects LOSSY WebP at quality `q` (libwebp, the `webp-lossy`
    /// feature) — used by the quick "Convert into ▸ WebP" verb so it produces the
    /// small files WebP exists for. `None` keeps the pure-Rust lossless encoder.
    /// Ignored for every non-WebP format. The Convert… dialog drives its own WebP
    /// quality through [`ConvertOpts::webp_quality`], so the `Target` it builds
    /// leaves this `None`.
    pub webp_quality: Option<u8>,
}

/// JPEG quality used by the shrink-for-email presets (a sensible "looks fine in
/// an email, stays small" middle ground, independent of the saved Options value).
mod carry;
mod compress;
mod samplers;
mod slots;
mod streaming;

// Parent-hub imports: children are glob-imported PRIVATELY so the pipeline below reads
// as one flat namespace and each child's `use super::*` sees the shared types. The
// crate-facing names are re-exported explicitly.
use samplers::*;
use streaming::*;

pub use compress::compress_to_size;
pub(crate) use slots::{
    predict_unique_suffix, preserve_src_time, reserve, reserve_unique_suffix, unique_output,
    with_tmp_suffix, write_atomic, OutSlot,
};

// pub(crate): the routed CLI path (verbs::actions::helper::shrink_one) formats this
// into `--quality` instead of hard-coding "82", so the two paths can't silently desync.
pub(crate) const EMAIL_JPEG_QUALITY: u8 = 82;

/// Composite onto white and drop alpha. JPEG has no alpha channel, and a plain
/// `to_rgb8()` would expose whatever color transparent pixels happened to carry
/// (black/colored halos), so blend over white instead.
pub(crate) fn flatten_onto_white(img: &DynamicImage) -> DynamicImage {
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = src.0;
        let a = a as u32;
        let over = |c: u8| (((c as u32) * a + 255 * (255 - a) + 127) / 255) as u8;
        *dst = image::Rgb([over(r), over(g), over(b)]);
    }
    DynamicImage::ImageRgb8(rgb)
}

/// Read a file into memory, refusing anything past the shared
/// `decode::limits::MAX_INPUT_BYTES` ceiling (checked via metadata before the
/// allocation) so a multi-GB file can't be loaded wholesale. Delegates to
/// [`crate::decode::read_capped`] — the SAME DoS budget the thumbnail path uses —
/// and flattens the io error to `E_FAIL` for the verb call sites.
pub(crate) fn read_capped(path: &str) -> Result<Vec<u8>> {
    crate::decode::read_capped(path).map_err(|_| Error::from(E_FAIL))
}

/// Output extensions written through the bundled ImageMagick.
///
/// The authoritative writer list lives beside ImageMagick's explicit coder
/// mapping. Keeping this as a forwarding predicate prevents exact conversion,
/// transforms, and resizes from disagreeing about formats such as PSD or DDS.
pub(crate) fn ext_needs_magick(ext: &str) -> bool {
    decode::magick_output_supported(ext)
}

/// Map file extensions to formats this build can actually WRITE natively.
///
/// `ImageFormat::from_extension` is deliberately not used for output routing:
/// it also recognizes decoder-only formats (notably DDS/PCX), which previously
/// let a generic PNG fallback create PNG bytes under the source extension.
fn native_output_format(ext: &str) -> Option<ImageFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        "pam" | "ppm" | "pnm" => Some(ImageFormat::Pnm),
        "tiff" | "tif" => Some(ImageFormat::Tiff),
        "tga" => Some(ImageFormat::Tga),
        "bmp" => Some(ImageFormat::Bmp),
        "ico" => Some(ImageFormat::Ico),
        "hdr" => Some(ImageFormat::Hdr),
        "exr" => Some(ImageFormat::OpenExr),
        "ff" => Some(ImageFormat::Farbfeld),
        "qoi" => Some(ImageFormat::Qoi),
        _ => None,
    }
}

/// Extension an edit/resize output may truthfully keep.
///
/// Unknown and decoder-only sources fall back to PNG. This helper is shared by
/// the in-process writer and out-of-process routing so their reserved/reported
/// paths cannot drift.
pub(crate) fn edit_output_ext(source_ext: &str) -> &str {
    if ext_needs_magick(source_ext) || native_output_format(source_ext).is_some() {
        source_ext
    } else {
        "png"
    }
}

/// Decode `path` and re-encode it as `target` next to the original, choosing a
/// non-colliding name (never overwrites the source or an existing file) and
/// writing via a temp file + rename so a failed encode leaves no partial file.
/// Returns the output path on success.
pub fn convert_file(path: &str, target: Target) -> Result<std::path::PathBuf> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full_for_path(&bytes, path)?;

    let slot = unique_output(Path::new(path), target.ext);

    // Magick-only targets (AVIF/JXL): write to the same-volume temp and replace
    // the reserved placeholder only after a clean child exit, exactly like the
    // native encoders below.
    if ext_needs_magick(target.ext) {
        // The quick "Convert into ▸ AVIF/JXL" verb: magick's default quality (None) — kept
        // byte-identical to before. The Convert… dialog carries an explicit quality instead.
        write_atomic(slot.path(), |tmp| {
            decode::encode_via_magick(&img, tmp, target.ext, None)
        })?;
        preserve_src_time(Path::new(path), slot.path());
        return Ok(slot.path().to_path_buf());
    }

    let img = if matches!(target.format, ImageFormat::Jpeg) {
        flatten_onto_white(&img)
    } else {
        img
    };

    // Honor the target's WebP-quality (lossy for the quick WebP verb), and the
    // saved JPEG/PNG settings — same as `encode_to`, plus the lossy-WebP selector.
    let carried = carry::read(&bytes, &src_ext(path));
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            target.format,
            crate::settings::jpeg_quality(),
            crate::settings::png_level(),
            target.webp_quality,
            target.ext,
            tmp,
        )?;
        if let Some(m) = &carried {
            carry::apply(m, tmp, target.ext)?;
        }
        Ok(())
    })?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// A path's lowercased extension, the key both the decoder tiers and the
/// metadata carry use to decide what a file actually is.
fn src_ext(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Apply a [`Transform`] and write the result as a NEW file ("<name> (edited)")
/// next to the original — never overwrites the source (a JPEG would re-compress).
/// Keeps the source format. Returns the output path.
pub fn transform_file(path: &str, t: Transform) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let src = Path::new(path);
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();

    // LOSSLESS path for baseline JPEGs: rotate/flip the DCT coefficients directly
    // (no decode-to-pixels, no re-quantize → zero quality loss). Falls through to
    // the lossy re-encode below if the JPEG is outside the supported scope
    // (progressive, non-block-aligned dimensions, etc.).
    if matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
        let op = match t {
            Transform::Right90 => crate::jpegtran::Op::Rot90,
            Transform::Left90 => crate::jpegtran::Op::Rot270,
            Transform::Rotate180 => crate::jpegtran::Op::Rot180,
            Transform::FlipH => crate::jpegtran::Op::FlipH,
            Transform::FlipV => crate::jpegtran::Op::FlipV,
        };
        if let Some(out_bytes) = crate::jpegtran::transform(&bytes, op) {
            let out_bytes = neutralize_lossless_jpeg_orientation(out_bytes);
            let slot = reserve_unique_suffix(src, "edited", &ext);
            write_atomic(slot.path(), |tmp| {
                std::fs::write(tmp, &out_bytes).map_err(|_| Error::from(E_FAIL))
            })?;
            preserve_src_time(src, slot.path());
            return Ok(slot.path().to_path_buf());
        }
    }

    // Pixel fallback: keep the extension only when a real writer exists. Exotic
    // writable formats go through Magick; decoder-only/unknown inputs get an
    // honest PNG sibling instead of PNG bytes disguised by the source suffix.
    let img = decode::decode_full_for_path(&bytes, path)?;
    let out_img = match t {
        Transform::Right90 => img.rotate90(),
        Transform::Left90 => img.rotate270(),
        Transform::Rotate180 => img.rotate180(),
        Transform::FlipH => img.fliph(),
        Transform::FlipV => img.flipv(),
    };
    let out_ext = edit_output_ext(&ext);
    let native_format = if ext_needs_magick(out_ext) {
        None
    } else {
        Some(native_output_format(out_ext).unwrap_or(ImageFormat::Png))
    };
    let slot = reserve_unique_suffix(src, "edited", out_ext);
    // A104: this pixel fallback (progressive JPEG / PNG / TIFF / …) decodes-and-re-encodes,
    // which drops every metadata block on its own — `resize_file` below already carries EXIF/
    // XMP/IPTC through the same shape of pipeline; this branch was the one place that didn't.
    let carried = carry::read(&bytes, &ext);
    write_atomic(slot.path(), |tmp| {
        if let Some(format) = native_format {
            encode_to(&out_img, format, out_ext, tmp)?;
        } else {
            decode::encode_via_magick(&out_img, tmp, out_ext, None)?;
        }
        if let Some(m) = &carried {
            carry::apply(m, tmp, out_ext)?;
        }
        Ok(())
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// A273: after a lossless rotate/flip, reset a stale EXIF Orientation tag to 1.
///
/// `crate::jpegtran::transform` keeps APPn/EXIF segments byte-for-byte verbatim while
/// physically transforming the DCT grid (that's the whole point — zero requantize loss), so
/// a source whose Orientation was non-1 now has a tag describing the WRONG transform: an
/// orientation-aware viewer rotates the already-rotated pixels a second time. This branch
/// returns straight to the caller before `carry` is ever consulted (there is no fresh
/// re-encode here for `carry::apply` to graft onto), so nothing else neutralizes it — mirrors
/// `carry::normalize_orientation`'s bit layout exactly, just applied to the OUTPUT file's own
/// already-present segment instead of a metadata block lifted off the source.
///
/// Best-effort: any parse surprise returns `bytes` unchanged rather than risk corrupting a
/// file whose pixel transform already succeeded. A file with no EXIF, or none of the shapes
/// this recognizes, is untouched — exactly today's behavior for those cases.
fn neutralize_lossless_jpeg_orientation(bytes: Vec<u8>) -> Vec<u8> {
    use img_parts::jpeg::{markers, Jpeg, JpegSegment};
    use img_parts::Bytes;

    const EXIF_PREFIX: &[u8] = b"Exif\0\0";
    const TAG_ORIENTATION: u16 = 0x0112;

    // Same bit layout `carry::normalize_orientation` rewrites: IFD0's Orientation entry is a
    // single SHORT stored inline in the entry's own 4-byte value field, so this never changes
    // the block's length or any offset within it.
    fn normalize(tiff: &mut [u8]) {
        let le = match tiff.first_chunk::<2>() {
            Some(b"II") => true,
            Some(b"MM") => false,
            _ => return,
        };
        let u16at = |b: &[u8], o: usize| -> Option<u16> {
            let v = b.get(o..o + 2)?.first_chunk::<2>()?;
            Some(if le {
                u16::from_le_bytes(*v)
            } else {
                u16::from_be_bytes(*v)
            })
        };
        let u32at = |b: &[u8], o: usize| -> Option<u32> {
            let v = b.get(o..o + 4)?.first_chunk::<4>()?;
            Some(if le {
                u32::from_le_bytes(*v)
            } else {
                u32::from_be_bytes(*v)
            })
        };
        let Some(ifd0) = u32at(tiff, 4).map(|v| v as usize) else {
            return;
        };
        let Some(count) = u16at(tiff, ifd0) else {
            return;
        };
        for i in 0..count as usize {
            let entry = ifd0 + 2 + i * 12;
            if u16at(tiff, entry) != Some(TAG_ORIENTATION) {
                continue;
            }
            // Type 3 (SHORT), count 1 — anything else is malformed; leave it alone rather
            // than guess at a layout we do not recognise.
            if u16at(tiff, entry + 2) != Some(3) || u32at(tiff, entry + 4) != Some(1) {
                return;
            }
            let one: [u8; 2] = if le {
                1u16.to_le_bytes()
            } else {
                1u16.to_be_bytes()
            };
            if let Some(slot) = tiff.get_mut(entry + 8..entry + 10) {
                slot.copy_from_slice(&one);
            }
            return;
        }
    }

    let Ok(mut jpeg) = Jpeg::from_bytes(Bytes::from(bytes.clone())) else {
        return bytes;
    };
    let segs = jpeg.segments_mut();
    let Some(idx) = segs
        .iter()
        .position(|s| s.marker() == markers::APP1 && s.contents().starts_with(EXIF_PREFIX))
    else {
        return bytes; // no EXIF segment — nothing to neutralize
    };
    let mut tiff = segs[idx].contents()[EXIF_PREFIX.len()..].to_vec();
    normalize(&mut tiff);
    let mut new_contents = EXIF_PREFIX.to_vec();
    new_contents.extend_from_slice(&tiff);
    segs[idx] = JpegSegment::new_with_contents(markers::APP1, Bytes::from(new_contents));

    let out = jpeg.encoder().bytes();
    // Sanity re-parse, mirroring carry::apply_jpeg — never hand back something we cannot
    // read again.
    if Jpeg::from_bytes(out.clone()).is_err() {
        return bytes;
    }
    out.to_vec()
}

/// Resize via a menu preset and write a new "(resized)" file next to the source,
/// keeping the original format. Never upscales. Returns the output path.
pub fn resize_file(path: &str, r: Resize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let img = apply_resize(decode::decode_full_for_path(&bytes, path)?, r);
    let src = Path::new(path);
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let out_ext = edit_output_ext(&ext);
    let native_format = if ext_needs_magick(out_ext) {
        None
    } else {
        Some(native_output_format(out_ext).unwrap_or(ImageFormat::Png))
    };
    let slot = reserve_unique_suffix(src, "resized", out_ext);
    let carried = carry::read(&bytes, &ext);
    write_atomic(slot.path(), |tmp| {
        if let Some(format) = native_format {
            encode_to(&img, format, out_ext, tmp)?;
        } else {
            decode::encode_via_magick(&img, tmp, out_ext, None)?;
        }
        if let Some(m) = &carried {
            carry::apply(m, tmp, out_ext)?;
        }
        Ok(())
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// Encode `img` to `path` as `format`, honoring the user's saved JPEG quality /
/// PNG compression settings (Options). WebP stays lossless (the quick verbs have
/// no quality knob).
fn encode_to(img: &DynamicImage, format: ImageFormat, target_ext: &str, path: &Path) -> Result<()> {
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
fn encode_to_opts(
    img: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    target_ext: &str,
    path: &Path,
) -> Result<()> {
    // Only the (optional) lossy-WebP arm consults this; without that feature, WebP
    // is encoded losslessly via `image` and the quality is irrelevant.
    #[cfg(not(feature = "webp-lossy"))]
    let _ = webp_quality;
    let file = std::fs::File::create(path).map_err(|_| Error::from(E_FAIL))?;
    let mut w = std::io::BufWriter::new(file);
    // ICO frames are at most 256×256; downscale (preserving aspect) to fit.
    let resized;
    let img = if matches!(format, ImageFormat::Ico) && (img.width() > 256 || img.height() > 256) {
        resized = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
        &resized
    } else {
        img
    };
    let res = match format {
        ImageFormat::Jpeg => img
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut w,
                jpeg_quality,
            ))
            .map_err(|_| Error::from(E_FAIL)),
        // Lossy WebP via libwebp (image-webp only encodes lossless). Smaller
        // files for photos; alpha is preserved. Optional: without `webp-lossy`,
        // WebP falls through to the lossless `other` arm (the `image` encoder).
        #[cfg(feature = "webp-lossy")]
        ImageFormat::WebP if webp_quality.is_some() => {
            let quality = match webp_quality {
                Some(quality) => quality,
                None => return Err(Error::from(E_FAIL)),
            };
            // libwebp rejects edges > 16383. `encode()` looks infallible but
            // .unwrap()s internally, and the worker thread has no catch_unwind
            // (panic=abort) — so an oversized image would abort the whole batch.
            // Fail this one file cleanly instead.
            let (pw, ph) = (img.width(), img.height());
            if pw == 0 || ph == 0 || pw > 16383 || ph > 16383 {
                return Err(Error::from(E_FAIL));
            }
            let rgba = img.to_rgba8();
            let mem = webp::Encoder::from_rgba(rgba.as_raw(), pw, ph)
                .encode(quality.clamp(1, 100) as f32);
            w.write_all(&mem).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Png => {
            // image's PNG encoder takes a coarse Fast/Default/Best level, not
            // the legacy 0–9 zlib scale, so map onto it.
            let ct = match png_level {
                0..=2 => image::codecs::png::CompressionType::Fast,
                7..=9 => image::codecs::png::CompressionType::Best,
                _ => image::codecs::png::CompressionType::Default,
            };
            img.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
                &mut w,
                ct,
                image::codecs::png::FilterType::Adaptive,
            ))
            .map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::OpenExr => encode_exr_bounded(&mut w, img).map_err(|_| Error::from(E_FAIL)),
        ImageFormat::Hdr => encode_hdr_bounded(&mut w, img).map_err(|_| Error::from(E_FAIL)),
        ImageFormat::Farbfeld => {
            encode_farbfeld_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Pnm => {
            let is_pam = target_ext.eq_ignore_ascii_case("pam");
            let is_ppm = target_ext.eq_ignore_ascii_case("ppm");
            if is_pam {
                encode_pam_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
            } else if is_ppm {
                encode_ppm_streaming(&mut w, img).map_err(|_| Error::from(E_FAIL))
            } else {
                // Preserve the prior dynamic behavior for PBM/PGM/general-PNM
                // transforms, whose subtype depends on their pixel type.
                img.write_to(&mut w, ImageFormat::Pnm)
                    .map_err(|_| Error::from(E_FAIL))
            }
        }
        other => img.write_to(&mut w, other).map_err(|_| Error::from(E_FAIL)),
    };
    res?;
    // Flush the buffered tail explicitly: BufWriter::drop discards flush errors,
    // so a disk-full on the final block would otherwise let the caller rename a
    // TRUNCATED temp file over the destination (breaking the atomic-write promise).
    w.flush().map_err(|_| Error::from(E_FAIL))?;
    Ok(())
}

/// Resize applied by the Convert… dialog.
#[derive(Clone, Copy)]
pub enum Resize {
    None,
    /// Fit within `w`×`h` preserving aspect; never upscales (the menu presets —
    /// "Fit 1920×1080" means shrink-to-fit, not blow up a small image).
    Fit(u32, u32),
    /// Scale to fit `w`×`h` preserving aspect, UP or down — the Convert dialog's
    /// explicit "Defined size": typing dimensions bigger than the source means
    /// "make it bigger" (user feedback).
    FitUp(u32, u32),
    /// Scale by `0`% (1..=1000).
    Percent(u32),
    /// Fit inside `w`x`h` and then PAD out to exactly that canvas, centred, with
    /// the gap filled by a blurred, stretched copy of the image itself.
    ///
    /// Every other mode returns whatever aspect the source had; this is the only
    /// one that guarantees an exact output size, which is what you want when the
    /// results have to line up in a grid, a slideshow, or a store listing. XnView
    /// calls it "blurred frame".
    Pad(u32, u32),
}

/// Convert options chosen in the Convert… dialog.
#[derive(Clone, Copy)]
pub struct ConvertOpts {
    pub target: Target,
    pub jpeg_quality: u8,
    pub png_level: u32,
    /// `Some(q)` = lossy WebP at quality q; `None` = lossless WebP (ignored for
    /// non-WebP formats).
    pub webp_quality: Option<u8>,
    pub resize: Resize,
}

pub(crate) fn apply_resize(img: DynamicImage, r: Resize) -> DynamicImage {
    match r {
        Resize::None => img,
        Resize::Fit(w, h) if img.width() > w || img.height() > h => {
            img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3)
        }
        Resize::Fit(..) => img,
        // `image::resize` scales in BOTH directions (aspect preserved), which is
        // exactly the explicit-dimensions contract.
        Resize::FitUp(w, h) => {
            img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3)
        }
        Resize::Percent(p) => {
            let s = p.clamp(1, 1000) as f64 / 100.0;
            let w = ((img.width() as f64 * s).round() as u32).max(1);
            let h = ((img.height() as f64 * s).round() as u32).max(1);
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        }
        Resize::Pad(w, h) => pad_to_canvas(img, w.max(1), h.max(1)),
    }
}

/// Centre `img` on an exact `w` x `h` canvas whose background is a blurred,
/// stretched copy of the image.
///
/// The blur is done at 1/8 scale and then scaled back up rather than run at full
/// resolution: a Gaussian over a 1920x1080 canvas is slow enough to be felt in a
/// batch, and after an 8x upscale the two are indistinguishable — a blur is a
/// low-pass filter, so the detail thrown away by downscaling is detail the blur
/// was about to destroy anyway.
fn pad_to_canvas(img: DynamicImage, w: u32, h: u32) -> DynamicImage {
    use image::imageops::FilterType::{Lanczos3, Triangle};

    let fitted = img.resize(w, h, Lanczos3);
    let (small_w, small_h) = ((w / 8).max(1), (h / 8).max(1));
    let mut canvas = img
        .resize_to_fill(small_w, small_h, Triangle)
        .blur(((small_w.max(small_h) as f32) / 12.0).max(1.0))
        .resize_exact(w, h, Triangle)
        .to_rgba8();

    let x = ((w.saturating_sub(fitted.width())) / 2) as i64;
    let y = ((h.saturating_sub(fitted.height())) / 2) as i64;
    image::imageops::overlay(&mut canvas, &fitted.to_rgba8(), x, y);
    DynamicImage::ImageRgba8(canvas)
}

/// Convert `path` into `out_dir` per `opts` (the Convert… dialog path). Picks a
/// non-colliding name, writes atomically. Returns the output path.
pub fn convert_file_opts(path: &str, opts: ConvertOpts, out_dir: &Path) -> Result<PathBuf> {
    convert_file_opts_named(path, opts, out_dir, None)
}

/// [`convert_file_opts`] with an optional name tag inserted before the extension,
/// e.g. `holiday (1280x720).jpg`.
///
/// This exists for the dialog's "write every preset size" mode: without a tag the
/// three outputs would collide on one name and the collision-free reserver would
/// silently produce `holiday.jpg`, `holiday (2).jpg`, `holiday (3).jpg` — three
/// files whose names say nothing about which size is which.
pub fn convert_file_opts_named(
    path: &str,
    opts: ConvertOpts,
    out_dir: &Path,
    tag: Option<&str>,
) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let mut img = apply_resize(decode::decode_full_for_path(&bytes, path)?, opts.resize);
    if matches!(opts.target.format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let ext = opts.target.ext.to_string();
    let dir = out_dir.to_path_buf();
    let tag = tag.map(|t| format!(" ({t})")).unwrap_or_default();
    let slot = reserve(move |n| {
        let name = if n == 0 {
            format!("{stem}{tag}.{ext}")
        } else {
            format!("{stem}{tag} ({n}).{ext}")
        };
        dir.join(name)
    });
    // Same metadata carry-through the quick Convert verb does — the dialog is the
    // path people run on a folder of photos, so it is the one that matters most.
    let carried = carry::read(&bytes, &src_ext(path));
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            opts.target.format,
            opts.jpeg_quality,
            opts.png_level,
            opts.webp_quality,
            opts.target.ext,
            tmp,
        )?;
        if let Some(m) = &carried {
            carry::apply(m, tmp, opts.target.ext)?;
        }
        Ok(())
    })?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Convert `input` to the EXACT `out` path (format inferred from its extension),
/// at `quality`, with `resize`. Used by the `st2k` CLI where the caller names the
/// output file. `webp_quality = Some(q)` selects lossy WebP at quality `q` (the
/// menu's quick WebP verb routes here with `Some(80)` when the `st2k.exe` helper
/// runs the conversion out-of-process); `None` keeps WebP lossless. PNG output uses
/// the saved `settings::png_level()` (default 9) — the SAME level the in-process
/// `convert_file` uses, so a helper-routed PNG convert is byte-identical to the
/// in-process one (it used to hard-code level 6 here, diverging whenever the user's
/// PNG setting wasn't 6).
pub fn convert_to(
    input: &str,
    out: &Path,
    quality: u8,
    webp_quality: Option<u8>,
    resize: Resize,
) -> Result<()> {
    let ext = out
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| Error::from(E_FAIL))?
        .to_ascii_lowercase();
    // Route every explicitly supported Magick target through its named coder.
    if ext_needs_magick(&ext) {
        // None = magick's default quality, so the quick verb's out-of-process (`st2k convert`)
        // path stays byte-identical to its in-process twin. The Convert… dialog uses
        // `convert_to_magick_in` with an explicit quality instead.
        return convert_to_magick(input, out, resize, None);
    }
    // Validate the requested writer before touching the input. Besides avoiding
    // wasted decode work, this guarantees an unknown suffix fails even when the
    // input path is missing or hostile.
    let format = native_output_format(&ext).ok_or_else(|| Error::from(E_FAIL))?;
    let bytes = read_capped(input)?;
    let mut img = apply_resize(decode::decode_full_for_path(&bytes, input)?, resize);
    if matches!(format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    write_atomic(out, |tmp| {
        encode_to_opts(
            &img,
            format,
            quality,
            crate::settings::png_level(),
            webp_quality,
            &ext,
            tmp,
        )
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` to the EXACT `out` path via the bundled ImageMagick — for the
/// exotic Convert targets the `image` crate can't encode (PSD/DDS/JP2/…).
/// Decodes with OUR pipeline (so every input format works), applies `resize`, then
/// hands magick a PNG to write `out` through an explicit, allowlisted coder.
pub fn convert_to_magick(
    input: &str,
    out: &Path,
    resize: Resize,
    quality: Option<u8>,
) -> Result<()> {
    let target_ext = out
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| Error::from(E_FAIL))?;
    if !decode::magick_output_supported(target_ext) {
        return Err(Error::from(E_FAIL));
    }
    let bytes = read_capped(input)?;
    let img = apply_resize(decode::decode_full_for_path(&bytes, input)?, resize);
    write_atomic(out, |tmp| {
        decode::encode_via_magick(&img, tmp, target_ext, quality)
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` into `out_dir` via the bundled ImageMagick at extension `ext`,
/// picking a collision-free reserved name (race-safe under parallel batches).
/// Wraps [`convert_to_magick`] so the Convert… dialog's exotic targets carry no
/// naming logic. Returns the output path.
pub fn convert_to_magick_in(
    input: &str,
    out_dir: &Path,
    ext: &str,
    resize: Resize,
    quality: Option<u8>,
) -> Result<PathBuf> {
    convert_to_magick_in_named(input, out_dir, ext, resize, quality, None)
}

/// [`convert_to_magick_in`] with the same name tag [`convert_file_opts_named`]
/// takes, so the dialog's "write every preset size" mode names its AVIF/JXL/PSD
/// outputs the same way it names the native ones. Without it three sizes would
/// land as `photo.avif`, `photo (2).avif`, `photo (3).avif` with nothing to say
/// which is which.
#[allow(clippy::too_many_arguments)]
pub fn convert_to_magick_in_named(
    input: &str,
    out_dir: &Path,
    ext: &str,
    resize: Resize,
    quality: Option<u8>,
    tag: Option<&str>,
) -> Result<PathBuf> {
    if !decode::magick_output_supported(ext) {
        return Err(Error::from(E_FAIL));
    }
    let stem = Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let dir = out_dir.to_path_buf();
    let e = ext.to_string();
    let tag = tag.map(|t| format!(" ({t})")).unwrap_or_default();
    let slot = reserve(move |n| {
        let name = if n == 0 {
            format!("{stem}{tag}.{e}")
        } else {
            format!("{stem}{tag} ({n}).{e}")
        };
        dir.join(name)
    });
    convert_to_magick(input, slot.path(), resize, quality)?;
    Ok(slot.path().to_path_buf())
}

/// One image → a single-page PDF in `out_dir` (collision-free reserved name).
/// Wraps [`crate::topdf::combine_to_pdf`] so the Convert… dialog's PDF target
/// carries no naming logic. Returns the output path.
pub fn convert_image_to_pdf_in(input: &str, out_dir: &Path, quality: u8) -> Result<PathBuf> {
    let stem = Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let dir = out_dir.to_path_buf();
    let slot = reserve(move |n| {
        let name = if n == 0 {
            format!("{stem}.pdf")
        } else {
            format!("{stem} ({n}).pdf")
        };
        dir.join(name)
    });
    let one = [input.to_string()];
    crate::topdf::combine_to_pdf(&one, slot.path(), quality)?;
    preserve_src_time(Path::new(input), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Decode `path`, cap its longest edge to the preset, and write a small
/// "(email)" JPEG sibling (flattened onto white — JPEG has no alpha). Never
/// upscales; never touches the original. Returns the output path.
pub fn shrink_for_email(path: &str, size: EmailSize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let edge = size.max_edge();
    let img = flatten_onto_white(&apply_resize(
        decode::decode_full_for_path(&bytes, path)?,
        Resize::Fit(edge, edge),
    ));
    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "email", "jpg");
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            ImageFormat::Jpeg,
            EMAIL_JPEG_QUALITY,
            6,
            None,
            "jpg",
            tmp,
        )
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

#[cfg(test)]
mod bounded_native_encoder_tests {
    use super::*;
    use exr::prelude::{Compression, MetaData, SampleType, Vec2};
    use image::GenericImageView;
    use std::io::Cursor;

    fn hdr_fixture(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba32F(image::Rgba32FImage::from_fn(width, height, |x, y| {
            if x < width / 2 {
                image::Rgba([100_000.0, 2.0, 0.5, 0.25])
            } else {
                image::Rgba([
                    1.0 + x as f32 / width as f32,
                    0.25 + y as f32 / height as f32,
                    4.0,
                    0.75,
                ])
            }
        }))
    }

    #[test]
    fn bounded_native_encoders_have_valid_headers_roundtrip_and_sizes() {
        let img = hdr_fixture(64, 32);
        let pixel_count = u64::from(img.width()) * u64::from(img.height());

        let mut exr = Cursor::new(Vec::new());
        encode_exr_bounded(&mut exr, &img).unwrap();
        let exr = exr.into_inner();
        assert!(exr.starts_with(&[0x76, 0x2f, 0x31, 0x01]));
        assert!(
            exr.len() < (pixel_count * 16) as usize,
            "constant-heavy tiled f32 EXR should compress below its raw f32 pixels"
        );
        let metadata = MetaData::read_from_buffered(Cursor::new(&exr), true).unwrap();
        let header = &metadata.headers[0];
        assert_eq!(header.compression, Compression::PIZ);
        assert!(
            header
                .channels
                .list
                .iter()
                .all(|channel| channel.sample_type == SampleType::F32),
            "all EXR output channels must be f32"
        );
        match header.blocks {
            exr::meta::BlockDescription::Tiles(description) => {
                assert_eq!(description.tile_size, Vec2(256, 256));
            }
            _ => panic!("EXR output must use bounded tiles"),
        }
        let decoded_exr = image::load_from_memory_with_format(&exr, ImageFormat::OpenExr).unwrap();
        let decoded_exr = decoded_exr.to_rgba32f();
        let exr_pixel = decoded_exr.get_pixel(0, 0).0;
        assert!(
            (exr_pixel[0] - 100_000.0).abs() < 1.0,
            "f32 EXR value above f16::MAX was clipped"
        );
        assert!((exr_pixel[3] - 0.25).abs() < 0.01, "EXR alpha changed");

        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let hdr_header = format!(
            "#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y {} +X {}\n",
            img.height(),
            img.width()
        );
        assert!(hdr.starts_with(hdr_header.as_bytes()));
        assert_eq!(
            &hdr[hdr_header.len()..hdr_header.len() + 4],
            &[2, 2, 0, 64],
            "new Radiance per-component RLE marker is missing"
        );
        assert!(
            hdr.len() < hdr_header.len() + (pixel_count * 4) as usize,
            "constant-heavy HDR should be smaller than raw RGBE"
        );
        let decoded_hdr = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
        let decoded_hdr = decoded_hdr.to_rgb32f();
        assert!(
            decoded_hdr.get_pixel(0, 0).0[0] > 99_000.0,
            "float HDR range was clipped before RGBE encoding"
        );

        let mut farbfeld = Vec::new();
        encode_farbfeld_streaming(&mut farbfeld, &img).unwrap();
        assert!(farbfeld.starts_with(b"farbfeld"));
        assert_eq!(farbfeld.len(), 16 + (pixel_count * 8) as usize);
        let decoded_farbfeld =
            image::load_from_memory_with_format(&farbfeld, ImageFormat::Farbfeld).unwrap();
        assert_eq!(decoded_farbfeld.dimensions(), img.dimensions());

        let mut pam = Vec::new();
        encode_pam_streaming(&mut pam, &img).unwrap();
        let pam_header_end = pam
            .windows(b"ENDHDR\n".len())
            .position(|window| window == b"ENDHDR\n")
            .map(|index| index + b"ENDHDR\n".len())
            .unwrap();
        assert!(pam.starts_with(b"P7\n"));
        assert!(pam[..pam_header_end]
            .windows(b"MAXVAL 65535".len())
            .any(|window| window == b"MAXVAL 65535"));
        assert_eq!(pam.len(), pam_header_end + (pixel_count * 8) as usize);
        let decoded_pam = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
        assert_eq!(decoded_pam.dimensions(), img.dimensions());
        assert_eq!(decoded_pam.to_rgba16().get_pixel(0, 0).0[3], 16_384);

        let mut ppm = Vec::new();
        encode_ppm_streaming(&mut ppm, &img).unwrap();
        let ppm_header = format!("P6\n{} {}\n65535\n", img.width(), img.height());
        assert!(ppm.starts_with(ppm_header.as_bytes()));
        assert_eq!(ppm.len(), ppm_header.len() + (pixel_count * 6) as usize);
        let decoded_ppm = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm).unwrap();
        assert_eq!(decoded_ppm.dimensions(), img.dimensions());
    }

    #[test]
    fn hdr_short_scanlines_use_raw_compatible_fallback() {
        let img = hdr_fixture(7, 2);
        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let header = b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 7\n";
        assert!(hdr.starts_with(header));
        assert_eq!(hdr.len(), header.len() + 7 * 2 * 4);
        assert_ne!(&hdr[header.len()..header.len() + 4], &[2, 2, 0, 7]);
        let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr).unwrap();
        assert_eq!(decoded.dimensions(), (7, 2));
    }

    #[test]
    fn pam_and_ppm_preserve_16_bit_samples_and_pam_channel_models() {
        let fixtures = [
            (
                DynamicImage::ImageLuma16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Luma([0x1234]),
                )),
                1usize,
                "GRAYSCALE",
                vec![0x12, 0x34],
            ),
            (
                DynamicImage::ImageLumaA16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::LumaA([0x1234, 0xABCD]),
                )),
                2,
                "GRAYSCALE_ALPHA",
                vec![0x12, 0x34, 0xAB, 0xCD],
            ),
            (
                DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Rgb([0x1234, 0x5678, 0x9ABC]),
                )),
                3,
                "RGB",
                vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
            ),
            (
                DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
                    1,
                    1,
                    image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
                )),
                4,
                "RGB_ALPHA",
                vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
            ),
        ];

        for (img, depth, tuple_type, expected_body) in fixtures {
            let mut pam = Vec::new();
            encode_pam_streaming(&mut pam, &img).unwrap();
            let header =
                format!("P7\nWIDTH 1\nHEIGHT 1\nDEPTH {depth}\nMAXVAL 65535\nTUPLTYPE {tuple_type}\nENDHDR\n");
            assert!(pam.starts_with(header.as_bytes()));
            assert_eq!(&pam[header.len()..], expected_body);
            let decoded = image::load_from_memory_with_format(&pam, ImageFormat::Pnm).unwrap();
            assert_eq!(decoded.dimensions(), (1, 1));
        }

        let rgba = DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
            1,
            1,
            image::Rgba([0x1234, 0x5678, 0x9ABC, 0xDEF0]),
        ));
        let mut ppm = Vec::new();
        encode_ppm_streaming(&mut ppm, &rgba).unwrap();
        let header = b"P6\n1 1\n65535\n";
        assert!(ppm.starts_with(header));
        assert_eq!(&ppm[header.len()..], &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
        let decoded = image::load_from_memory_with_format(&ppm, ImageFormat::Pnm)
            .unwrap()
            .to_rgb16();
        assert_eq!(decoded.get_pixel(0, 0).0, [0x1234, 0x5678, 0x9ABC]);
    }

    #[test]
    fn hdr_rle_width_boundary_and_raw_marker_escape_are_valid() {
        let rle_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32_767,
            1,
            image::Rgb([64, 32, 16]),
        ));
        let mut rle = Vec::new();
        encode_hdr_bounded(&mut rle, &rle_img).unwrap();
        let rle_header =
            b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32767\n";
        assert_eq!(
            &rle[rle_header.len()..rle_header.len() + 4],
            &[2, 2, 127, 255]
        );
        assert_eq!(
            image::load_from_memory_with_format(&rle, ImageFormat::Hdr)
                .unwrap()
                .dimensions(),
            (32_767, 1)
        );

        let raw_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32_768,
            1,
            image::Rgb([64, 32, 16]),
        ));
        let mut raw = Vec::new();
        encode_hdr_bounded(&mut raw, &raw_img).unwrap();
        let raw_header =
            b"#?RADIANCE\n# Rust HDR encoder\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 32768\n";
        assert_eq!(raw.len(), raw_header.len() + 32_768 * 4);
        assert_ne!(
            &raw[raw_header.len()..raw_header.len() + 4],
            &[2, 2, 128, 0]
        );
        assert_eq!(
            image::load_from_memory_with_format(&raw, ImageFormat::Hdr)
                .unwrap()
                .dimensions(),
            (32_768, 1)
        );

        assert_eq!(escape_raw_rgbe_marker([1, 1, 1, 42], true), [1, 1, 2, 42]);
        assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], true), [2, 3, 7, 99]);
        assert_eq!(escape_raw_rgbe_marker([2, 2, 7, 99], false), [2, 2, 7, 99]);
    }

    #[test]
    fn float_to_integer_samples_preserve_saturation_semantics() {
        assert_eq!(f32_to_u8(f32::NAN), u8::MAX);
        assert_eq!(f32_to_u8(f32::INFINITY), u8::MAX);
        assert_eq!(f32_to_u8(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_u8(-0.25), 0);
        assert_eq!(f32_to_u8(0.5), 128);
        assert_eq!(f32_to_u8(1.25), u8::MAX);

        assert_eq!(f32_to_u16(f32::NAN), u16::MAX);
        assert_eq!(f32_to_u16(f32::INFINITY), u16::MAX);
        assert_eq!(f32_to_u16(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_u16(-0.25), 0);
        assert_eq!(f32_to_u16(0.5), 32_768);
        assert_eq!(f32_to_u16(1.25), u16::MAX);
    }

    #[test]
    fn hdr_non_finite_and_out_of_range_samples_saturate_safely() {
        assert_eq!(
            float_rgb_to_rgbe([f32::NAN, f32::NEG_INFINITY, -1.0]),
            [0, 0, 0, 0]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::INFINITY, 0.0, 0.0]),
            [255, 0, 0, 255]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::MAX, f32::MAX, f32::MAX]),
            [255, 255, 255, 255]
        );
        assert_eq!(
            float_rgb_to_rgbe([f32::from_bits(1), 0.0, 0.0]),
            [0, 0, 0, 0],
            "unrepresentable subnormal radiance should underflow to black"
        );

        let img = DynamicImage::ImageRgb32F(
            image::Rgb32FImage::from_raw(
                5,
                1,
                vec![
                    f32::NAN,
                    f32::NEG_INFINITY,
                    -1.0,
                    f32::INFINITY,
                    0.0,
                    0.0,
                    f32::MAX,
                    f32::MAX,
                    f32::MAX,
                    1.0,
                    0.5,
                    0.25,
                    f32::from_bits(1),
                    0.0,
                    0.0,
                ],
            )
            .unwrap(),
        );
        let mut hdr = Vec::new();
        encode_hdr_bounded(&mut hdr, &img).unwrap();
        let decoded = image::load_from_memory_with_format(&hdr, ImageFormat::Hdr)
            .unwrap()
            .to_rgb32f();
        assert_eq!(decoded.dimensions(), (5, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [0.0, 0.0, 0.0]);
        assert_eq!(decoded.get_pixel(4, 0).0, [0.0, 0.0, 0.0]);
        assert!(decoded
            .pixels()
            .flat_map(|pixel| pixel.0)
            .all(|component| component.is_finite() && component >= 0.0));
        assert!(decoded.get_pixel(1, 0).0[0] > 1.0e38);
        assert!(decoded.get_pixel(2, 0).0[0] > 1.0e38);
    }

    #[test]
    fn output_extension_routing_is_explicit_and_honest() {
        for ext in [
            "avif", "jxl", "psd", "dds", "jp2", "pcx", "sgi", "pfm", "dpx", "fits", "xpm", "pict",
            "ras", "palm",
        ] {
            assert!(ext_needs_magick(ext), "{ext} must route through Magick");
            assert_eq!(edit_output_ext(ext), ext);
        }
        for (ext, format) in [
            ("png", ImageFormat::Png),
            ("jpg", ImageFormat::Jpeg),
            ("jpeg", ImageFormat::Jpeg),
            ("jpe", ImageFormat::Jpeg),
            ("jfif", ImageFormat::Jpeg),
            ("gif", ImageFormat::Gif),
            ("webp", ImageFormat::WebP),
            ("pam", ImageFormat::Pnm),
            ("ppm", ImageFormat::Pnm),
            ("pnm", ImageFormat::Pnm),
            ("tiff", ImageFormat::Tiff),
            ("tif", ImageFormat::Tiff),
            ("tga", ImageFormat::Tga),
            ("bmp", ImageFormat::Bmp),
            ("ico", ImageFormat::Ico),
            ("hdr", ImageFormat::Hdr),
            ("exr", ImageFormat::OpenExr),
            ("ff", ImageFormat::Farbfeld),
            ("qoi", ImageFormat::Qoi),
        ] {
            assert_eq!(native_output_format(ext), Some(format), "{ext}");
            assert_eq!(edit_output_ext(ext), ext);
        }
        for ext in ["", "heic", "svg", "pbm", "pgm", "unknown"] {
            assert_eq!(native_output_format(ext), None, "{ext}");
            assert!(!ext_needs_magick(ext), "{ext}");
            assert_eq!(edit_output_ext(ext), "png", "{ext}");
        }
    }

    #[test]
    fn exact_unknown_conversion_rejects_without_replacing_destination() {
        let dir = std::env::temp_dir().join(format!(
            "st2k-exact-unknown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 2, image::Rgb([20, 80, 160])))
            .save(&input)
            .unwrap();
        let output = dir.join("existing.unknown");
        std::fs::write(&output, b"original destination").unwrap();

        assert!(convert_to(input.to_str().unwrap(), &output, 90, None, Resize::None).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), b"original destination");
        assert!(!with_tmp_suffix(&output).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_source_edits_use_png_name_and_signature() {
        let dir = std::env::temp_dir().join(format!(
            "st2k-edit-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.heic");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            12,
            8,
            image::Rgb([20, 80, 160]),
        ))
        .save_with_format(&input, ImageFormat::Png)
        .unwrap();

        let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
        assert_eq!(edited.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert!(std::fs::read(&edited)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image::open(&edited).unwrap().dimensions(), (8, 12));

        let resized = resize_file(input.to_str().unwrap(), Resize::Fit(6, 4)).unwrap();
        assert_eq!(
            resized.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
        assert!(std::fs::read(&resized)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image::open(&resized).unwrap().dimensions(), (6, 4));
        let _ = std::fs::remove_dir_all(dir);
    }

    // Needs ImageMagick (bundled on a full install, or on PATH).
    #[test]
    #[ignore]
    fn exact_psd_and_magick_backed_edits_have_psd_signatures() {
        if !decode::magick_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "st2k-magick-routing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.png");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            30,
            image::Rgb([30, 160, 90]),
        ))
        .save(&input)
        .unwrap();

        let psd = dir.join("existing.psd");
        std::fs::write(&psd, b"old destination").unwrap();
        convert_to(input.to_str().unwrap(), &psd, 90, None, Resize::None).unwrap();
        assert!(std::fs::read(&psd).unwrap().starts_with(b"8BPS"));
        assert!(!with_tmp_suffix(&psd).exists());

        let edited = transform_file(psd.to_str().unwrap(), Transform::Right90).unwrap();
        assert_eq!(edited.extension().and_then(|ext| ext.to_str()), Some("psd"));
        assert!(std::fs::read(&edited).unwrap().starts_with(b"8BPS"));

        let resized = resize_file(psd.to_str().unwrap(), Resize::Fit(20, 15)).unwrap();
        assert_eq!(
            resized.extension().and_then(|ext| ext.to_str()),
            Some("psd")
        );
        assert!(std::fs::read(&resized).unwrap().starts_with(b"8BPS"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `SystemTime`+pid-suffixed scratch dir, matching the pattern the other `transform_file`/
    /// `resize_file` tests in this module already use.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "st2k-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Little-endian TIFF IFD0 with one entry, Orientation (tag 0x0112) as SHORT — same
    /// shape `carry`'s own (private, cross-file-inaccessible) test fixture uses, rebuilt here
    /// because a private helper in a sibling module cannot be imported across the file
    /// boundary.
    fn tiff_with_orientation(o: u16) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        v.extend_from_slice(&1u16.to_le_bytes()); // one entry
        v.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        v.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
        v.extend_from_slice(&1u32.to_le_bytes()); // count
        v.extend_from_slice(&(o as u32).to_le_bytes()); // value, left-packed inline
        v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        v
    }

    fn orientation_of(tiff: &[u8]) -> u16 {
        let e = 8 + 2; // IFD0 offset(8) + entry count(2) -> first (only) entry
        u16::from_le_bytes([tiff[e + 8], tiff[e + 9]])
    }

    /// Little-endian TIFF IFD0 with one ASCII entry, Make (tag 0x010F) = "SageT\0" (6 bytes,
    /// out-of-line since ASCII > 4 bytes doesn't fit the inline value field).
    fn tiff_with_make() -> Vec<u8> {
        // header(8) + count(2) + one 12-byte entry + next-IFD(4) = where the out-of-line
        // value lands.
        let value_offset: u32 = 8 + 2 + 12 + 4;
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0x010Fu16.to_le_bytes()); // Make
        v.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
        v.extend_from_slice(&6u32.to_le_bytes()); // count, incl. the NUL
        v.extend_from_slice(&value_offset.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        v.extend_from_slice(b"SageT\0");
        assert_eq!(
            v.len() as u32,
            value_offset + 6,
            "offset math must match the actual layout"
        );
        v
    }

    fn jpeg_with_exif(base: &[u8], tiff: &[u8]) -> Vec<u8> {
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(tiff);
        let mut out = base[0..2].to_vec(); // SOI
        out.extend_from_slice(&[0xFF, img_parts::jpeg::markers::APP1]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&base[2..]);
        out
    }

    fn png_with_exif(w: u32, h: u32, tiff: &[u8]) -> Vec<u8> {
        let mut base = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            w,
            h,
            image::Rgba([20, 80, 160, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut base), ImageFormat::Png)
        .unwrap();
        let mut png = img_parts::png::Png::from_bytes(img_parts::Bytes::from(base)).unwrap();
        png.chunks_mut().insert(
            1, // straight after IHDR, matching carry::apply_png's own placement
            img_parts::png::PngChunk::new(*b"eXIf", img_parts::Bytes::from(tiff.to_vec())),
        );
        png.encoder().bytes().to_vec()
    }

    /// A273: `transform_file`'s LOSSLESS jpegtran path keeps a source's APP1 EXIF segment
    /// byte-for-byte verbatim while physically rotating the DCT grid, so a stale
    /// Orientation=6 must come back reset to 1 — otherwise an orientation-aware viewer
    /// rotates the already-rotated pixels a second time.
    #[test]
    fn transform_file_lossless_jpeg_path_resets_stale_orientation() {
        let dir = scratch_dir("lossless-orient");
        let input = dir.join("source.jpg");

        // 32x32 is MCU-aligned for both 4:2:0 and 4:4:4 chroma subsampling, so the lossless
        // jpegtran path is guaranteed to take the transform rather than falling through to
        // the pixel path this test isn't exercising.
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            32,
            32,
            image::Rgb([90, 140, 30]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut base), ImageFormat::Jpeg)
        .unwrap();
        let with_exif = jpeg_with_exif(&base, &tiff_with_orientation(6));
        std::fs::write(&input, &with_exif).unwrap();

        let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
        assert_eq!(
            edited.extension().and_then(|e| e.to_str()),
            Some("jpg"),
            "the lossless path keeps the source extension"
        );

        let out_bytes = std::fs::read(&edited).unwrap();
        let jpeg = img_parts::jpeg::Jpeg::from_bytes(img_parts::Bytes::from(out_bytes)).unwrap();
        let exif_seg = jpeg
            .segments()
            .iter()
            .find(|s| {
                s.marker() == img_parts::jpeg::markers::APP1
                    && s.contents().starts_with(b"Exif\0\0")
            })
            .expect("lossless transform must not drop the EXIF segment entirely");
        let tiff_out = &exif_seg.contents()[6..];
        assert_eq!(
            orientation_of(tiff_out),
            1,
            "a stale non-identity Orientation must be reset after the lossless rotate, \
             or viewers double-rotate it"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A104: `transform_file`'s PIXEL fallback (progressive JPEG / PNG / TIFF / …) decodes
    /// and re-encodes, which drops every metadata block on its own unless carried through —
    /// exactly what `resize_file` already does and this branch didn't. A plain `.png` source
    /// never takes the lossless jpegtran path at all, so this exercises the pixel fallback
    /// directly.
    #[test]
    fn transform_file_pixel_fallback_carries_exif_through_rotation() {
        let dir = scratch_dir("pixel-carry");
        let input = dir.join("source.png");
        std::fs::write(&input, png_with_exif(12, 8, &tiff_with_make())).unwrap();

        let edited = transform_file(input.to_str().unwrap(), Transform::Right90).unwrap();
        let info = crate::strip::read_info(edited.to_str().unwrap());
        assert_eq!(
            info.make.as_deref(),
            Some("SageT"),
            "EXIF Make must survive the pixel-fallback rotate, matching resize_file"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
