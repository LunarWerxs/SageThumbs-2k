//! Decode → (optional resize / flatten) → encode primitives: the `Target` /
//! `Resize` / `ConvertOpts` descriptors, the size-capped reader, the atomic
//! encode-to-file path, and the per-file convert / transform / resize / email
//! entry points the menu actions and the CLI dispatch to.

use std::path::{Path, PathBuf};

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
const EMAIL_JPEG_QUALITY: u8 = 82;

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

/// A reserved, collision-free output path. Creating it makes an EMPTY placeholder
/// file with `create_new`, so concurrent workers — even in separate processes (the
/// DLL pre-reserves a name, then `st2k.exe` writes it) — can never pick the same
/// name. (A plain `while path.exists()` check is a TOCTOU race once batches run in
/// parallel.) The writer renames its finished temp ON TOP of the placeholder
/// (`write_atomic` does exactly this), turning it into the real file.
///
/// On drop the placeholder is removed IFF it's still a zero-byte file: an
/// abandoned/failed reservation never litters, while a real (non-empty) output is
/// never touched. No explicit "commit" is needed — a successful write leaves a
/// non-empty file behind, which drop keeps.
pub(crate) struct OutSlot(PathBuf);

impl OutSlot {
    /// The reserved path — hand this to the encoder / `st2k`.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for OutSlot {
    fn drop(&mut self) {
        // Remove only a still-empty placeholder: a successful write replaced it with
        // a non-empty file (keep it); a failed/abandoned one left it at zero bytes
        // (clean it up). Image encoders never produce a 0-byte success.
        if std::fs::metadata(&self.0).map(|m| m.len() == 0).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Atomically reserve the first free path produced by `name(n)` for n = 0, 1, 2…,
/// by creating an empty placeholder with `create_new`. See [`OutSlot`].
pub(crate) fn reserve(name: impl Fn(u32) -> PathBuf) -> OutSlot {
    let mut n = 0u32;
    loop {
        let cand = name(n);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&cand) {
            Ok(_) => return OutSlot(cand), // the placeholder handle closes here
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            // Couldn't create (permission / missing dir): hand the name back anyway —
            // the encode will surface the real error. Don't loop on a non-Exists error.
            Err(_) => return OutSlot(cand),
        }
    }
}

/// Reserve a free `<stem>.<ext>` next to `src` (`<stem> (n).<ext>` if taken),
/// atomically (see [`reserve`]). Replaces the old existence-check picker.
pub(crate) fn unique_output(src: &Path, ext: &str) -> OutSlot {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let ext = ext.to_string();
    reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        dir.join(name)
    })
}

/// Read a file into memory, refusing anything past the shared
/// `decode::limits::MAX_INPUT_BYTES` ceiling (checked via metadata before the
/// allocation) so a multi-GB file can't be loaded wholesale. Delegates to
/// [`crate::decode::read_capped`] — the SAME DoS budget the thumbnail path uses —
/// and flattens the io error to `E_FAIL` for the verb call sites.
pub(crate) fn read_capped(path: &str) -> Result<Vec<u8>> {
    crate::decode::read_capped(path).map_err(|_| Error::from(E_FAIL))
}

/// Output extensions the `image` crate can't encode — written through the bundled
/// ImageMagick instead (`decode::encode_via_magick`). Used by the quick
/// "Convert into ▸ AVIF" verb and the `st2k convert` CLI so an AVIF target routes
/// to magick the same way the Convert… dialog's exotic targets do. Magick provides
/// the output coder only; our pipeline still does every input decode.
pub(crate) fn ext_needs_magick(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "avif" | "jxl")
}

/// Decode `path` and re-encode it as `target` next to the original, choosing a
/// non-colliding name (never overwrites the source or an existing file) and
/// writing via a temp file + rename so a failed encode leaves no partial file.
/// Returns the output path on success.
pub fn convert_file(path: &str, target: Target) -> Result<std::path::PathBuf> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?;

    let slot = unique_output(Path::new(path), target.ext);

    // Magick-only targets (AVIF/JXL): magick infers the format from the output
    // extension and overwrites the reserved placeholder directly (like
    // `convert_to_magick`). `encode_via_magick` removes a partial file on failure,
    // so a failed encode still leaves nothing behind.
    if ext_needs_magick(target.ext) {
        // The quick "Convert into ▸ AVIF/JXL" verb: magick's default quality (None) — kept
        // byte-identical to before. The Convert… dialog carries an explicit quality instead.
        decode::encode_via_magick(&img, slot.path(), None)?;
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
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            target.format,
            crate::settings::jpeg_quality(),
            crate::settings::png_level(),
            target.webp_quality,
            tmp,
        )
    })?;
    preserve_src_time(Path::new(path), slot.path());
    Ok(slot.path().to_path_buf())
}

/// Apply a [`Transform`] and write the result as a NEW file ("<name> (edited)")
/// next to the original — never overwrites the source (a JPEG would re-compress).
/// Keeps the source format. Returns the output path.
pub fn transform_file(path: &str, t: Transform) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();

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
            let slot = reserve_unique_suffix(src, "edited", &ext);
            write_atomic(slot.path(), |tmp| {
                std::fs::write(tmp, &out_bytes).map_err(|_| Error::from(E_FAIL))
            })?;
            preserve_src_time(src, slot.path());
            return Ok(slot.path().to_path_buf());
        }
    }

    // Lossy fallback: decode → transform pixels → re-encode (keeps the format).
    let img = decode::decode_full(&bytes)?;
    let out_img = match t {
        Transform::Right90 => img.rotate90(),
        Transform::Left90 => img.rotate270(),
        Transform::Rotate180 => img.rotate180(),
        Transform::FlipH => img.fliph(),
        Transform::FlipV => img.flipv(),
    };
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    let slot = reserve_unique_suffix(src, "edited", &ext);
    write_atomic(slot.path(), |tmp| encode_to(&out_img, format, tmp))?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// Resize via a menu preset and write a new "(resized)" file next to the source,
/// keeping the original format. Never upscales. Returns the output path.
pub fn resize_file(path: &str, r: Resize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let img = apply_resize(decode::decode_full(&bytes)?, r);
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    let slot = reserve_unique_suffix(src, "resized", &ext);
    write_atomic(slot.path(), |tmp| encode_to(&img, format, tmp))?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// `<out>.st2ktmp` — the temp path a write goes to before the atomic rename.
pub(crate) fn with_tmp_suffix(out: &Path) -> PathBuf {
    let mut s = out.to_path_buf().into_os_string();
    s.push(".st2ktmp");
    PathBuf::from(s)
}

/// Atomic write: run `write` against a same-volume `<out>.st2ktmp`, then rename
/// it over `out`. Owns the temp naming ([`with_tmp_suffix`]), the on-error temp
/// cleanup (a failed/partial write leaves no `.st2ktmp` and never an `out`), and
/// a short bounded rename retry (strip.rs-style: 5×40 ms) so a transient
/// Explorer/thumbnail-cache lock (os error 5/32) doesn't fail an otherwise good
/// write. `write` receives the temp path and must produce the finished file there.
pub(crate) fn write_atomic(out: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = with_tmp_suffix(out);
    write(&tmp).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    crate::fsutil::rename_retrying(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })
}

/// If "preserve original file date" is enabled (Options), stamp the source file's
/// modified-time onto a freshly-saved output. Best-effort — never fails a save.
pub(crate) fn preserve_src_time(src: &Path, out: &Path) {
    if !crate::settings::preserve_file_date() {
        return;
    }
    if let Ok(mtime) = std::fs::metadata(src).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(out) {
            let _ = f.set_modified(mtime);
        }
    }
}

/// Reserve a free `<stem> (<suffix>).<ext>` next to `src` (`<stem> (<suffix> n)`
/// if taken), atomically (see [`reserve`]). Used by the IN-PROCESS edit verbs
/// (rotate/resize/email) and the DLL's routed resize/email (which pass the reserved
/// path to `st2k`).
pub(crate) fn reserve_unique_suffix(src: &Path, suffix: &str, ext: &str) -> OutSlot {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let (suffix, ext) = (suffix.to_string(), ext.to_string());
    reserve(move |n| {
        let name = if n == 0 {
            format!("{stem} ({suffix}).{ext}")
        } else {
            format!("{stem} ({suffix} {}).{ext}", n + 1)
        };
        dir.join(name)
    })
}

/// PREDICT (read-only, no reservation) the `<stem> (<suffix>).<ext>` name that
/// [`reserve_unique_suffix`] would currently pick. Used ONLY by the DLL's routed
/// `st2k rotate`, where `st2k` auto-names the sibling itself — the DLL must guess
/// the name to reveal it WITHOUT creating a placeholder (a placeholder would push
/// st2k's own picker to `(… 2)`). Rotate names derive from the distinct source
/// stem, so parallel rotates over a selection don't collide on the prediction.
pub(crate) fn predict_unique_suffix(src: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cand = dir.join(format!("{stem} ({suffix}).{ext}"));
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({suffix} {n}).{ext}"));
        n += 1;
    }
    cand
}

/// Encode `img` to `path` as `format`, honoring the user's saved JPEG quality /
/// PNG compression settings (Options). WebP stays lossless (the quick verbs have
/// no quality knob).
fn encode_to(img: &DynamicImage, format: ImageFormat, path: &Path) -> Result<()> {
    encode_to_opts(
        img,
        format,
        crate::settings::jpeg_quality(),
        crate::settings::png_level(),
        None,
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
    path: &Path,
) -> Result<()> {
    use std::io::Write;
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
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, jpeg_quality))
            .map_err(|_| Error::from(E_FAIL)),
        // Lossy WebP via libwebp (image-webp only encodes lossless). Smaller
        // files for photos; alpha is preserved. Optional: without `webp-lossy`,
        // WebP falls through to the lossless `other` arm (the `image` encoder).
        #[cfg(feature = "webp-lossy")]
        ImageFormat::WebP if webp_quality.is_some() => {
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
                .encode(webp_quality.unwrap().clamp(1, 100) as f32);
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
        Resize::FitUp(w, h) => img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3),
        Resize::Percent(p) => {
            let s = p.clamp(1, 1000) as f64 / 100.0;
            let w = ((img.width() as f64 * s).round() as u32).max(1);
            let h = ((img.height() as f64 * s).round() as u32).max(1);
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        }
    }
}

/// Convert `path` into `out_dir` per `opts` (the Convert… dialog path). Picks a
/// non-colliding name, writes atomically. Returns the output path.
pub fn convert_file_opts(path: &str, opts: ConvertOpts, out_dir: &Path) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, opts.resize);
    if matches!(opts.target.format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let ext = opts.target.ext.to_string();
    let dir = out_dir.to_path_buf();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        dir.join(name)
    });
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(
            &img,
            opts.target.format,
            opts.jpeg_quality,
            opts.png_level,
            opts.webp_quality,
            tmp,
        )
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
pub fn convert_to(input: &str, out: &Path, quality: u8, webp_quality: Option<u8>, resize: Resize) -> Result<()> {
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    // AVIF/JXL aren't encodable by the `image` crate — route them through the
    // bundled ImageMagick (this is the path the quick "Convert into ▸ AVIF" verb
    // hits when it runs out-of-process via `st2k convert <in> <out.avif>`).
    if ext_needs_magick(&ext) {
        // None = magick's default quality, so the quick verb's out-of-process (`st2k convert`)
        // path stays byte-identical to its in-process twin. The Convert… dialog uses
        // `convert_to_magick_in` with an explicit quality instead.
        return convert_to_magick(input, out, resize, None);
    }
    let bytes = read_capped(input)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, resize);
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    if matches!(format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    write_atomic(out, |tmp| {
        encode_to_opts(&img, format, quality, crate::settings::png_level(), webp_quality, tmp)
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` to the EXACT `out` path via the bundled ImageMagick — for the
/// exotic Convert targets the `image` crate can't encode (PSD/DDS/JP2/EXR/…).
/// Decodes with OUR pipeline (so every input format works), applies `resize`, then
/// hands magick a PNG to write `out` (format inferred from its extension).
pub fn convert_to_magick(input: &str, out: &Path, resize: Resize, quality: Option<u8>) -> Result<()> {
    let bytes = read_capped(input)?;
    let img = apply_resize(decode::decode_full(&bytes)?, resize);
    decode::encode_via_magick(&img, out, quality)?;
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
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = out_dir.to_path_buf();
    let e = ext.to_string();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.{e}") } else { format!("{stem} ({n}).{e}") };
        dir.join(name)
    });
    convert_to_magick(input, slot.path(), resize, quality)?;
    Ok(slot.path().to_path_buf())
}

/// One image → a single-page PDF in `out_dir` (collision-free reserved name).
/// Wraps [`crate::topdf::combine_to_pdf`] so the Convert… dialog's PDF target
/// carries no naming logic. Returns the output path.
pub fn convert_image_to_pdf_in(input: &str, out_dir: &Path, quality: u8) -> Result<PathBuf> {
    let stem = Path::new(input).file_stem().and_then(|s| s.to_str()).unwrap_or("image").to_string();
    let dir = out_dir.to_path_buf();
    let slot = reserve(move |n| {
        let name = if n == 0 { format!("{stem}.pdf") } else { format!("{stem} ({n}).pdf") };
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
    let img = flatten_onto_white(&apply_resize(decode::decode_full(&bytes)?, Resize::Fit(edge, edge)));
    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "email", "jpg");
    write_atomic(slot.path(), |tmp| {
        encode_to_opts(&img, ImageFormat::Jpeg, EMAIL_JPEG_QUALITY, 6, None, tmp)
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}

/// JPEG quality search bounds for [`compress_to_size`].
const COMPRESS_Q_MIN: u8 = 20;
const COMPRESS_Q_MAX: u8 = 95;

/// Encode `img` to in-memory JPEG bytes at `quality` — the probe the size search uses.
fn jpeg_bytes(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality))
        .map_err(|_| Error::from(E_FAIL))?;
    Ok(buf)
}

/// Highest-quality JPEG of `img` at or under `target` bytes (binary search on quality),
/// or `None` if even [`COMPRESS_Q_MIN`] overshoots — then the caller downscales + retries.
fn jpeg_under(img: &DynamicImage, target: u64) -> Result<Option<Vec<u8>>> {
    let floor = jpeg_bytes(img, COMPRESS_Q_MIN)?;
    if floor.len() as u64 > target {
        return Ok(None);
    }
    let (mut lo, mut hi) = (COMPRESS_Q_MIN, COMPRESS_Q_MAX);
    let mut best = floor;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let b = jpeg_bytes(img, mid)?;
        if b.len() as u64 <= target {
            best = b;
            lo = mid + 1; // fits — try higher quality
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    Ok(Some(best))
}

/// Compress `path` into a JPEG at or under `target_bytes`, by binary-searching JPEG
/// quality and — if even the lowest quality overshoots — progressively downscaling (20%
/// a step, down to a ~32px floor). The "(compressed)" sibling never upscales and never
/// overwrites the original. With an unreasonably tiny target it ships the smallest it can
/// make (which may slightly exceed it). Reusable by the CLI and a future menu/dialog.
pub fn compress_to_size(path: &str, target_bytes: u64) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    // JPEG has no alpha → flatten transparency onto white, like shrink-for-email.
    let mut img = flatten_onto_white(&decode::decode_full(&bytes)?);
    let target = target_bytes.max(1);

    let mut chosen = None;
    for _ in 0..8 {
        if let Some(b) = jpeg_under(&img, target)? {
            chosen = Some(b);
            break;
        }
        let (w, h) = (img.width(), img.height());
        if w.min(h) <= 32 {
            break; // already tiny — stop shrinking
        }
        img = img.resize(
            (w * 4 / 5).max(1),
            (h * 4 / 5).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }
    let data = match chosen {
        Some(b) => b,
        None => jpeg_bytes(&img, COMPRESS_Q_MIN)?, // best-effort floor
    };

    let src = Path::new(path);
    let slot = reserve_unique_suffix(src, "compressed", "jpg");
    write_atomic(slot.path(), |tmp| {
        std::fs::write(tmp, &data).map_err(|_| Error::from(E_FAIL))
    })?;
    preserve_src_time(src, slot.path());
    Ok(slot.path().to_path_buf())
}
