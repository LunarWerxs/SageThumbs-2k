//! The targets only ImageMagick can write: our decode, our PNG, magick's encoder.

use super::*;

/// Output extensions written through the bundled ImageMagick.
///
/// The authoritative writer list lives beside ImageMagick's explicit coder
/// mapping. Keeping this as a forwarding predicate prevents exact conversion,
/// transforms, and resizes from disagreeing about formats such as PSD or DDS.
pub(crate) fn ext_needs_magick(ext: &str) -> bool {
    decode::magick_output_supported(ext)
}

/// Map an intermediate-PNG encode failure to `E_FAIL` with context: the closing error
/// for `encode_via_magick_carrying`, whose intermediate PNG magick decodes first.
fn png_encode_error(e: image::ImageError) -> Error {
    Error::new(E_FAIL, format!("encode intermediate PNG: {e}"))
}

/// Encode `img` to `out` via ImageMagick, carrying `carried`'s EXIF/XMP/ICC onto
/// the intermediate PNG handed to magick - the same PNG magick decodes before it
/// writes the exotic target (PSD/DDS/AVIF/JXL/...), so magick propagates that
/// metadata into whatever it writes, for the formats that can hold it. The ONE
/// path every magick-backed convert/resize/rotate verb goes through, so none of
/// them can drift from the others on what gets carried.
pub(super) fn encode_via_magick_carrying(
    img: &DynamicImage,
    carried: Option<&carry::Carried>,
    out: &Path,
    out_ext: &str,
    quality: Option<u8>,
) -> Result<()> {
    let mut png = crate::magick_png_bytes!(img, png_encode_error);
    if let Some(meta) = carried {
        png = carry::apply_to_png_bytes(meta, png);
    }
    decode::encode_via_magick_png(png, out, out_ext, quality)
}

/// The magick-only branch of [`convert_file`]: carry the source metadata onto the
/// intermediate PNG and hand it to magick, then stamp the source's time.
pub(super) fn convert_file_via_magick(
    path: &str,
    bytes: &[u8],
    img: &DynamicImage,
    target: Target,
    slot: &OutSlot,
) -> Result<()> {
    // The quick "Convert into ▸ AVIF/JXL" verb: magick's default quality (None) — kept
    // byte-identical to before. The Convert… dialog carries an explicit quality instead.
    let carried = carry::read(bytes, &src_ext(path));
    write_atomic(slot.path(), |tmp| {
        encode_via_magick_carrying(img, carried.as_ref(), tmp, target.ext, None)
    })?;
    preserve_src_time(Path::new(path), slot.path());
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
    convert_to_magick_watermarked(input, out, resize, quality, None)
}

/// [`convert_to_magick`] with an optional watermark applied after resize and
/// before the intermediate PNG magick decodes - the Convert… dialog's own
/// entry point for the exotic (magick-backed) targets, so a watermark lands
/// there exactly as it does for the native encoders. Kept separate from the
/// public [`convert_to_magick`] so its no-watermark callers (the quick "Convert
/// into" verb, the `st2k` out-of-process routing) keep their existing 4-argument
/// signature.
pub(super) fn convert_to_magick_watermarked(
    input: &str,
    out: &Path,
    resize: Resize,
    quality: Option<u8>,
    watermark: Option<&Watermark>,
) -> Result<()> {
    let target_ext = out
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            Error::new(
                E_FAIL,
                format!("magick: {} has no extension", out.display()),
            )
        })?;
    if !decode::magick_output_supported(target_ext) {
        return Err(Error::new(
            E_FAIL,
            format!("magick: .{target_ext} is not a supported output format"),
        ));
    }
    let bytes = read_full_fidelity_capped(input)?;
    let mut img = apply_resize(decode::decode_full_for_path(&bytes, input)?, resize);
    if let Some(wm) = watermark {
        apply_watermark(&mut img, wm)?;
    }
    let carried = carry::read(&bytes, &src_ext(input));
    write_atomic(out, |tmp| {
        encode_via_magick_carrying(&img, carried.as_ref(), tmp, target_ext, quality)
    })?;
    preserve_src_time(Path::new(input), out);
    Ok(())
}

/// Convert `input` into `out_dir` via the bundled ImageMagick at extension `ext`,
/// picking a collision-free reserved name (race-safe under parallel batches), with
/// the same name tag [`convert_file_opts_named`]
/// takes, so the dialog's "write every preset size" mode names its AVIF/JXL/PSD
/// outputs the same way it names the native ones. Without it three sizes would
/// land as `photo.avif`, `photo (2).avif`, `photo (3).avif` with nothing to say
/// which is which. `watermark` is the same optional image overlay
/// [`ConvertOpts::watermark`] carries for the native encoders - the Convert…
/// dialog's exotic (magick-backed) targets go through this entry point rather
/// than a `ConvertOpts`, so the overlay is threaded through as its own argument.
#[allow(clippy::too_many_arguments)]
pub fn convert_to_magick_in_named(
    input: &str,
    out_dir: &Path,
    ext: &str,
    resize: Resize,
    quality: Option<u8>,
    tag: Option<&str>,
    watermark: Option<&Watermark>,
) -> Result<PathBuf> {
    if !decode::magick_output_supported(ext) {
        return Err(Error::new(
            E_FAIL,
            format!("magick: .{ext} is not a supported output format"),
        ));
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
    convert_to_magick_watermarked(input, slot.path(), resize, quality, watermark)?;
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
    // One input: it is either the whole document or nothing, so the omission policy
    // cannot matter here (an unusable input already fails as "none could be decoded").
    crate::topdf::combine_to_pdf(&one, slot.path(), quality, crate::verbs::OnOmit::Report)?;
    preserve_src_time(Path::new(input), slot.path());
    Ok(slot.path().to_path_buf())
}
