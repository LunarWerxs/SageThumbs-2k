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
use super::outcome::OmitCause;
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

/// Carry EXIF / XMP / IPTC from the source into a converted or resized output.
mod carry;
mod compress;
mod samplers;
mod slots;
mod streaming;
mod watermark;

// Parent-hub imports: children are glob-imported PRIVATELY so the pipeline below reads
// as one flat namespace and each child's `use super::*` sees the shared types. The
// crate-facing names are re-exported explicitly.
use samplers::*;
use streaming::*;
mod transform;
use transform::*;
mod magickpath;
use magickpath::*;
mod variants;
pub(crate) use magickpath::ext_needs_magick;
pub use magickpath::{
    convert_image_to_pdf_in, convert_to_magick, convert_to_magick_in, convert_to_magick_in_named,
};
pub use transform::transform_file;
use variants::*;

pub use compress::compress_to_size;
#[cfg(test)]
pub(crate) use slots::staging_leftovers;
pub(crate) use slots::{
    predict_unique_suffix, preserve_src_time, reserve, reserve_unique_suffix, unique_output,
    write_atomic, OutSlot,
};
pub use watermark::{Corner, Watermark};

// pub(crate): the routed CLI path (verbs::actions::helper::shrink_one) formats this
// into `--quality` instead of hard-coding "82", so the two paths can't silently desync.
/// JPEG quality used by the shrink-for-email presets (a sensible "looks fine in
/// an email, stays small" middle ground, independent of the saved Options value).
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

/// Read a file into memory for a full-fidelity verb, refusing anything past
/// `decode::limits::MAX_FULL_FIDELITY_INPUT_BYTES` (checked via metadata before the
/// allocation) so a multi-GB file can't be loaded wholesale.
///
/// The name says which cap applies: `decode::read_capped` refuses past the much
/// smaller thumbnail ceiling (`MAX_INPUT_BYTES`, 256 MiB), and the preview tier's
/// reader truncates instead of refusing. Issue #34: this used to share the thumbnail
/// ceiling under the same name, which silently dropped every PSD over 256 MiB from a
/// Convert batch. See [`crate::decode::read_full_fidelity`] for why the user-chosen
/// file gets its own, larger budget.
///
/// The io error is logged and carried in the returned error's message, because a
/// bare `E_FAIL` is what made the failure unexplainable: the verb call sites have
/// no room for an error string, so without the log line the size refusal reached
/// the user as a file that simply was not there.
pub(crate) fn read_full_fidelity_capped(path: &str) -> Result<Vec<u8>> {
    crate::decode::read_full_fidelity(path).map_err(|e| {
        crate::safety::log(&format!("cannot read {path}: {e}"));
        Error::new(E_FAIL, format!("read {path}: {e}"))
    })
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
    let bytes = read_full_fidelity_capped(path)?;
    let img = decode::decode_full_for_path(&bytes, path)?;

    let slot = unique_output(Path::new(path), target.ext);

    // Magick-only targets (AVIF/JXL): write to the same-volume temp and replace
    // the reserved placeholder only after a clean child exit, exactly like the
    // native encoders below.
    if ext_needs_magick(target.ext) {
        convert_file_via_magick(path, &bytes, &img, target, &slot)?;
    } else {
        convert_file_native(path, &bytes, img, target, &slot)?;
    }
    Ok(slot.path().to_path_buf())
}

/// The native-encoder branch of [`convert_file`]: flatten for JPEG, encode, and
/// graft the carried metadata onto the written file.
fn convert_file_native(
    path: &str,
    bytes: &[u8],
    img: DynamicImage,
    target: Target,
    slot: &OutSlot,
) -> Result<()> {
    let img = if matches!(target.format, ImageFormat::Jpeg) {
        flatten_onto_white(&img)
    } else {
        img
    };

    // Honor the target's WebP-quality (lossy for the quick WebP verb), and the
    // saved JPEG/PNG settings — same as `encode_to`, plus the lossy-WebP selector.
    let carried = carry::read(bytes, &src_ext(path));
    encode_and_carry(
        &img,
        target.format,
        crate::settings::jpeg_quality(),
        crate::settings::png_level(),
        target.webp_quality,
        target.ext,
        carried.as_ref(),
        slot.path(),
    )?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(())
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

/// Resize via a menu preset and write a new "(resized)" file next to the source,
/// keeping the original format. Never upscales. Returns the output path.
pub fn resize_file(path: &str, r: Resize) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
    let img = apply_resize(decode::decode_full_for_path(&bytes, path)?, r);
    let src = Path::new(path);
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let out_ext = edit_output_ext(&ext);
    let native_format = native_writer_for(out_ext);
    let slot = reserve_unique_suffix(src, "resized", out_ext);
    let carried = carry::read(&bytes, &ext);
    write_reencoded(&img, out_ext, native_format, carried.as_ref(), &slot)?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
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
///
/// `Clone`, not `Copy`: [`Watermark`] carries an owned path, so a caller that
/// needs the same options more than once (the dialog's "write every preset
/// size" mode runs one `ConvertOpts` per size) clones explicitly.
#[derive(Clone)]
pub struct ConvertOpts {
    pub target: Target,
    pub jpeg_quality: u8,
    pub png_level: u32,
    /// `Some(q)` = lossy WebP at quality q; `None` = lossless WebP (ignored for
    /// non-WebP formats).
    pub webp_quality: Option<u8>,
    pub resize: Resize,
    /// `Some(mark)` overlays an image watermark after resize and before encode.
    /// `None` (the common case) leaves the pipeline exactly as it was.
    pub watermark: Option<Watermark>,
}

/// Read and decode `wm`'s mark image through the same bounded reader/decoder
/// the source image goes through, then alpha-blend it onto `img`. A mark that
/// fails to read or decode fails the file's conversion via `?`, exactly like
/// any other convert error - never a silent skip.
fn apply_watermark(img: &mut DynamicImage, wm: &Watermark) -> Result<()> {
    let bytes = read_full_fidelity_capped(&wm.path)?;
    let mark = decode::decode_full_for_output(&bytes)?;
    watermark::apply(img, &mark, wm.corner, wm.scale_pct, wm.opacity_pct);
    Ok(())
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
    let bytes = read_full_fidelity_capped(path)?;
    let mut img = apply_resize(decode::decode_full_for_path(&bytes, path)?, opts.resize);
    if let Some(wm) = &opts.watermark {
        apply_watermark(&mut img, wm)?;
    }
    if matches!(opts.target.format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let ext = opts.target.ext.to_string();
    let slot = reserve(reserved_name(out_dir.to_path_buf(), stem, tag, ext));
    // Same metadata carry-through the quick Convert verb does — the dialog is the
    // path people run on a folder of photos, so it is the one that matters most.
    let carried = carry::read(&bytes, &src_ext(path));
    write_converted_named(&img, &opts, carried.as_ref(), &slot)?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// The reserved-path closure shared by every convert verb: `<stem><tag>.<ext>`, with
/// ` (<n>)` inserted before the extension for the nth collision.
fn reserved_name(
    dir: PathBuf,
    stem: String,
    tag: Option<&str>,
    ext: String,
) -> impl Fn(u32) -> PathBuf {
    let tag = tag.map(|t| format!(" ({t})")).unwrap_or_default();
    move |n| {
        let name = if n == 0 {
            format!("{stem}{tag}.{ext}")
        } else {
            format!("{stem}{tag} ({n}).{ext}")
        };
        dir.join(name)
    }
}

/// Encode `img` per the given settings and graft the carried metadata onto the
/// written file. Shared by the three convert paths so their encode+carry sequence
/// can't drift apart.
#[allow(clippy::too_many_arguments)] // the encoder's inputs, gathered from the caller
fn encode_and_carry(
    img: &DynamicImage,
    format: ImageFormat,
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    ext: &str,
    carried: Option<&carry::Carried>,
    out: &Path,
) -> Result<()> {
    write_atomic(out, |tmp| {
        encode_to_opts(img, format, quality, png_level, webp_quality, ext, tmp)?;
        if let Some(m) = carried {
            carry::apply(m, tmp, ext)?;
        }
        Ok(())
    })
}

/// Encode the converted `img` per `opts` and graft the carried metadata onto it.
fn write_converted_named(
    img: &DynamicImage,
    opts: &ConvertOpts,
    carried: Option<&carry::Carried>,
    slot: &OutSlot,
) -> Result<()> {
    encode_and_carry(
        img,
        opts.target.format,
        opts.jpeg_quality,
        opts.png_level,
        opts.webp_quality,
        opts.target.ext,
        carried,
        slot.path(),
    )
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
    convert_to_reporting(input, out, quality, webp_quality, resize).map_err(|(_, e)| e)
}

/// [`convert_to`], keeping the PHASE that failed alongside the error (2026-09-05 audit,
/// F11) so a bulk caller can tell a corrupt input from an output it cannot produce. The
/// error itself is handed back untouched, so `convert_to` above still reports exactly the
/// text and HRESULT it always did.
///
/// The write phase reports as `Unencodable` rather than splitting encode from rename: both
/// come back through one `write_atomic` error, and the caller that reads this cause
/// (`cli::batch`) has already created the destination file when it reserved the name, so a
/// failure this late is the encoder's. A destination the process cannot write at all fails
/// at that reservation instead, which is what `Unwritable` is for.
pub fn convert_to_reporting(
    input: &str,
    out: &Path,
    quality: u8,
    webp_quality: Option<u8>,
    resize: Resize,
) -> std::result::Result<(), (OmitCause, Error)> {
    convert_to_reporting_with(input, out, quality, webp_quality, resize, true)
}

/// [`convert_to`] for the one caller that must NOT carry metadata across even while the
/// user's "keep metadata" preference is on: Shrink for email, whose point is a small, clean
/// attachment (`st2k convert --strip-metadata`).
pub fn convert_to_stripped(
    input: &str,
    out: &Path,
    quality: u8,
    webp_quality: Option<u8>,
    resize: Resize,
) -> Result<()> {
    convert_to_reporting_with(input, out, quality, webp_quality, resize, false).map_err(|(_, e)| e)
}

/// The body of [`convert_to_reporting`]. `carry_metadata` = graft the source's EXIF/ICC onto a
/// NATIVE output when the "keep metadata" preference allows it (`carry::read` checks that).
/// The installed quick Convert and Resize verbs run through here (`st2k convert`), and until
/// 2026-09-19 this path never carried anything, so the preference held in the Convert dialog
/// and silently did not in the right-click menu (audit F05).
fn convert_to_reporting_with(
    input: &str,
    out: &Path,
    quality: u8,
    webp_quality: Option<u8>,
    resize: Resize,
    carry_metadata: bool,
) -> std::result::Result<(), (OmitCause, Error)> {
    let ext = output_ext(out)?;
    // Route every explicitly supported Magick target through its named coder.
    if ext_needs_magick(&ext) {
        // None = magick's default quality, so the quick verb's out-of-process (`st2k convert`)
        // path stays byte-identical to its in-process twin. The Convert dialog uses
        // `convert_to_magick_in` with an explicit quality instead.
        //
        // One cause for the whole subprocess: magick decodes AND encodes behind one exit
        // code, so splitting the two here would be a guess. The message it carries names
        // which coder refused.
        return convert_to_magick(input, out, resize, None)
            .map_err(|e| (OmitCause::Unencodable, e));
    }
    // Validate the requested writer before touching the input. Besides avoiding
    // wasted decode work, this guarantees an unknown suffix fails even when the
    // input path is missing or hostile.
    let format = native_output_format(&ext).ok_or_else(|| {
        (
            OmitCause::Unencodable,
            Error::new(E_FAIL, format!("convert: no writer for .{ext}")),
        )
    })?;
    let bytes = read_full_fidelity_capped(input).map_err(|e| (OmitCause::Unreadable, e))?;
    let decoded =
        decode::decode_full_for_path(&bytes, input).map_err(|e| (OmitCause::Undecodable, e))?;
    let mut img = apply_resize(decoded, resize);
    if matches!(format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let carried = carry_metadata
        .then(|| carry::read(&bytes, &src_ext(input)))
        .flatten();
    write_converted_to(
        &img,
        format,
        &ext,
        quality,
        webp_quality,
        carried.as_ref(),
        out,
    )
    .map_err(|e| (OmitCause::Unencodable, e))?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// The lowercased output extension, or the `Unencodable` "no extension" error.
fn output_ext(out: &Path) -> std::result::Result<String, (OmitCause, Error)> {
    out.extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            (
                OmitCause::Unencodable,
                Error::new(
                    E_FAIL,
                    format!("convert: {} has no extension", out.display()),
                ),
            )
        })
        .map(str::to_ascii_lowercase)
}

/// Encode `img` to `out` per the requested native writer and graft the carried metadata.
fn write_converted_to(
    img: &DynamicImage,
    format: ImageFormat,
    ext: &str,
    quality: u8,
    webp_quality: Option<u8>,
    carried: Option<&carry::Carried>,
    out: &Path,
) -> Result<()> {
    encode_and_carry(
        img,
        format,
        quality,
        crate::settings::png_level(),
        webp_quality,
        ext,
        carried,
        out,
    )
}

/// Decode `path`, cap its longest edge to the preset, and write a small
/// "(email)" JPEG sibling (flattened onto white — JPEG has no alpha). Never
/// upscales; never touches the original. Returns the output path.
pub fn shrink_for_email(path: &str, size: EmailSize) -> Result<PathBuf> {
    let bytes = read_full_fidelity_capped(path)?;
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
mod bounded_native_encoder_tests;
