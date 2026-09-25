//! The thumbnail/convert/resize/rotate/compress verbs, plus the PDF/CBZ combiners and
//! the private helpers only they need (the archive contact-sheet fast path, the
//! oversized-archive guard, and the omitted-input report shared by `pdf`/`cbz`).

use super::*;

/// `max_dim` px on the long edge (`0` = full size). The headline verb: produces
/// previews for the formats Windows itself can't.
///
/// Refuses an `output` that is the `input` under any spelling, and an `output` whose
/// extension names no writable format, before any decode work (2026-09-05 audit, F30).
pub fn thumbnail(input: &str, output: &str, max_dim: u32) -> Result<String, String> {
    thumbnail_reporting(input, output, max_dim).map_err(|(_, e)| e)
}

/// [`thumbnail`], keeping the PHASE that failed alongside the message (2026-09-05 audit,
/// F11), so `batch` can say whether a file was unreadable, undecodable or simply had a
/// destination it could not use. The message is handed back unchanged, so `thumbnail`
/// above reads exactly as it always did.
///
/// The save phase reports as `Unencodable` for the same reason `convert_to_reporting`'s
/// does: `save_atomic` returns encode and rename failures through one error, and `batch`
/// has already created the destination file by the time this runs, so an unwritable
/// destination has failed earlier as `Unwritable`.
pub(super) fn thumbnail_reporting(
    input: &str,
    output: &str,
    max_dim: u32,
) -> std::result::Result<String, (verbs::OmitCause, String)> {
    use verbs::OmitCause;

    reject_output_alias(output, [input]).map_err(|e| (OmitCause::Unwritable, e))?;
    let format = image::ImageFormat::from_path(output).map_err(|_| {
        (
            OmitCause::Unencodable,
            format!("cannot write {output}: the extension names no image format"),
        )
    })?;
    let archive_ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if st2k_base::formats::is_archive(archive_ext) {
        reject_oversized_archive(input, st2k_base::settings::max_file_size_bytes())
            .map_err(|e| (OmitCause::Unreadable, e))?;
    }
    // Generic archive (.zip/.rar/.7z): the same list-then-extract path Explorer
    // uses — including the user's MaxSize gate before archive parsing — and the
    // contact sheet composes per the same Setting. Falls through to the normal
    // decode if it isn't really an archive (renamed file) so the magic-dispatch
    // tiers still get their shot.
    if let Some(img) = archive_thumbnail(input) {
        let out = fit_for_cli(img, max_dim);
        save_atomic(&out, output, format).map_err(|e| (OmitCause::Unencodable, e))?;
        return Ok(output.to_string());
    }
    let img = decode_thumbnail_image(input, max_dim)?;
    let out = fit_for_cli(img, max_dim);
    save_atomic(&out, output, format).map_err(|e| (OmitCause::Unencodable, e))?;
    Ok(output.to_string())
}

/// Decode the preview or primary image for thumbnailing, respecting size caps.
fn decode_thumbnail_image(
    input: &str,
    max_dim: u32,
) -> std::result::Result<image::DynamicImage, (verbs::OmitCause, String)> {
    use verbs::OmitCause;
    // Cap the read at the shared input budget (metadata-checked before allocating)
    // so a scripted/agent/MCP call can't load a multi-GB file wholesale — the same
    // ceiling Explorer thumbnailing and the path verbs apply. Head-preview
    // containers (.blend / PSD-PSB) past the cap still render from a bounded prefix.
    // Preview fidelity (embedded/container previews OK) — that's what a
    // thumbnail is; `convert` is the full-fidelity verb. By PATH, so the streaming
    // rescues apply: an OpenEXR is scaled straight off the file handle instead of
    // being refused for exceeding the shared input budget (which a 12K render pass
    // always does), and anything else already PAST that budget gets one last try
    // through the OS codecs reading the file directly. Every format under the
    // budget takes the same bounded whole-file read as before, and a file neither
    // rescue can open still reports the same size-limit error text.
    let edge = if max_dim > 0 {
        max_dim
    } else {
        decode::EXR_PATH_EDGE
    };
    match decode::decode_preview_streamed(input, edge) {
        Some(img) => Ok(img),
        None => {
            // `..._for`: this verb named a size, so a head prefix whose baked preview
            // cannot reach it must not stand in for the real picture (issue #33).
            let bytes = decode::read_preview_capped_for(input, edge)
                .map_err(|e| (OmitCause::Unreadable, e.to_string()))?;
            // Cap the decode at the edge we're about to shrink to anyway — the streamed
            // path above already takes `edge`, and rendering ImageMagick's full 4096 first
            // costs seconds on a big scan for pixels this immediately discards.
            decode::decode_preview_capped_for_path(&bytes, edge, input)
                .map_err(|_| (OmitCause::Undecodable, format!("cannot decode {input}")))
        }
    }
}

/// Fail before opening or parsing a generic archive when the user's MaxSize preference rejects
/// its length, the bound Explorer applies. The input ceiling is not one: past it an archive is
/// read by seeking, never buffered (a RAR by walking its block headers).
/// `configured_max == u64::MAX` is Settings' "Unlimited" representation.
fn reject_oversized_archive(input: &str, configured_max: u64) -> Result<(), String> {
    if let Ok(meta) = std::fs::metadata(input) {
        if meta.len() > configured_max {
            return Err(format!(
                "input is {} bytes, over the archive limit (MaxSize) of {configured_max} bytes",
                meta.len()
            ));
        }
    }
    Ok(())
}

/// The generic-archive cover/contact-sheet for a `.zip`/`.rar`/`.7z` PATH, or None
/// to take the normal decode route (not an archive extension, unreadable, or no
/// image entries — the CLI then reports "cannot decode", mirroring the shell's
/// stock-icon fallback). 1024px edge matches the preview pane's compose target.
fn archive_thumbnail(input: &str) -> Option<image::DynamicImage> {
    let covers = archive_covers(input)?;
    let d = decode::thumbnail_from_covers(&covers, 1024).ok()?;
    image::RgbaImage::from_raw(d.width, d.height, d.rgba).map(image::DynamicImage::ImageRgba8)
}

/// Extract candidate cover images from an archive file.
fn archive_covers(input: &str) -> Option<Vec<Vec<u8>>> {
    use std::io::Read;
    let ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !st2k_base::formats::is_archive(ext) {
        return None;
    }
    let want = if st2k_base::settings::archive_collage() {
        4
    } else {
        1
    };
    let mut f = std::fs::File::open(input).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).ok()?;
    let prefs = st2k_codecs::container::select::CoverPrefs::from_settings();
    let size = f.metadata().ok()?.len();
    if st2k_codecs::container::archive_needs_buffer(&head)
        && size <= decode::limits::MAX_INPUT_BYTES
    {
        // RAR buffers whole inside the ceiling (`rars` accepts no reader), the same bounded
        // read as the normal path; past it, the seek path walks its block headers.
        let bytes = decode::read_preview_capped(input).ok()?;
        st2k_codecs::container::archive_covers(&bytes, want, &prefs)
    } else {
        st2k_codecs::container::archive_covers_seek(&mut f, &head, want, &prefs)
    }
}

/// Convert `input` to the exact `output` path at `quality`, optional `resize`.
/// `webp_quality = Some(q)` writes lossy WebP at quality `q` (only meaningful when
/// `output` is a `.webp`); `None` keeps WebP lossless. Refuses an `output` that is the
/// `input` under any spelling (2026-09-05 audit, F30): the write replaces the destination,
/// and there is no in-place form of this verb.
pub fn convert(
    input: &str,
    output: &str,
    quality: u8,
    webp_quality: Option<u8>,
    resize: verbs::Resize,
    strip_metadata: bool,
) -> Result<String, String> {
    reject_output_alias(output, [input])?;
    // Clamp HERE, not just at each front end, so the CLI (which only clamped via
    // `u8::from_str` rejecting out-of-range strings, not in-range-but-silly ones like 0 or
    // 255) and the MCP surface (which already clamped) actually agree on what a "quality"
    // argument means, regardless of which one a caller went through.
    let quality = quality.clamp(1, 100);
    let webp_quality = webp_quality.map(|w| w.clamp(1, 100));
    // `--strip-metadata` is Shrink for email's opt-out; every other caller follows the user's
    // "keep metadata" preference (2026-09-19 audit F05).
    let converted = if strip_metadata {
        verbs::convert_to_stripped(input, Path::new(output), quality, webp_quality, resize)
    } else {
        verbs::convert_to(input, Path::new(output), quality, webp_quality, resize)
    };
    converted.map_err(|e| format!("convert failed: {input}: {e}"))?;
    Ok(output.to_string())
}

/// Rotate/flip → a "(edited)" sibling. `by` ∈ right|left|180|fliph|flipv.
pub fn rotate(input: &str, by: &str) -> Result<String, String> {
    let t = match by {
        "right" => verbs::Transform::Right90,
        "left" => verbs::Transform::Left90,
        "180" => verbs::Transform::Rotate180,
        "fliph" => verbs::Transform::FlipH,
        "flipv" => verbs::Transform::FlipV,
        _ => {
            return Err(format!(
                "unknown rotation '{by}' (right|left|180|fliph|flipv)"
            ))
        }
    };
    verbs::transform_file(input, t)
        .map(|p| p.display().to_string())
        .map_err(|e| format!("rotate failed: {input}: {e}"))
}

/// Decode `input` and return it as in-memory PNG bytes, fit within `max_dim` (0 = full
/// size). Powers the MCP `view` tool — lets an AI agent SEE any of our supported formats
/// directly (HEIC/RAW/PSD/ebook covers/CAD previews/…), not just convert them to a file.
pub fn view_png(input: &str, max_dim: u32) -> Result<Vec<u8>, String> {
    // An agent asking for a big view of a PSD wants the composite, not the 160 px baked
    // preview stretched to fill it (issue #33). `max_dim == 0` means full size, which is the
    // opposite of `ANY_PREVIEW` - ask for the largest edge there is.
    let want = if max_dim == 0 { u32::MAX } else { max_dim };
    let bytes = decode::read_preview_capped_for(input, want).map_err(|e| e.to_string())?;
    let img = decode::decode_preview_capped_for_path(&bytes, 0, input)
        .map_err(|_| format!("cannot decode {input}"))?;
    let img = fit_for_cli(img, max_dim);
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Compress to a target file size → a "(compressed)" JPEG sibling at or under
/// `target_bytes` (quality binary-search + downscale). See [`parse_size`]. A target the
/// search cannot meet fails and writes nothing; the error names the smallest size it can
/// reach (2026-09-05 audit, F32). The MCP `compress` tool shares this exact contract.
pub fn compress(input: &str, target_bytes: u64) -> Result<String, String> {
    verbs::compress_to_size(input, target_bytes)
        .map(|p| p.display().to_string())
        .map_err(|e| format!("compress failed: {input}: {e}"))
}

/// How the multi-input verbs (`pdf`, `cbz`) report and police omitted inputs, from either
/// front door (2026-09-05 audit, F31).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CombineOpts {
    /// Fail, writing nothing, if any input would be left out (`--strict`).
    pub strict: bool,
    /// Return the [`verbs::Combined`] JSON instead of text (`--json`; always on over MCP).
    pub json: bool,
    /// `pdf` only: OCR each page and add an invisible text layer (`--searchable`).
    pub searchable: bool,
}

impl CombineOpts {
    fn on_omit(self) -> verbs::OnOmit {
        if self.strict {
            verbs::OnOmit::Fail
        } else {
            verbs::OnOmit::Report
        }
    }
}

/// Render a composer's result for the caller. The all-good text form is exactly the output
/// path, as it always was, so a script reading stdout keeps working; a partial result adds
/// a `partial:` status line and one tab-separated `omitted` line per left-out input, the
/// same lines a `--strict` refusal carries; a searchable PDF with pages OCR could not read
/// adds a `no text:` line. The JSON form is [`verbs::Combined::to_json`].
fn combined_report(c: &verbs::Combined, opts: CombineOpts) -> String {
    if opts.json {
        return c.to_json().to_string();
    }
    let mut s = c.output.display().to_string();
    if c.is_partial() {
        s.push_str(&format!(
            "\npartial: {} of {} inputs combined, {} omitted",
            c.used,
            c.requested(),
            c.omitted.len()
        ));
        for o in &c.omitted {
            s.push('\n');
            s.push_str(&o.as_line());
        }
    }
    if c.untexted > 0 {
        s.push_str(&format!(
            "\nno text: {} of {} pages could not be read by OCR and are not searchable",
            c.untexted, c.used
        ));
    }
    s
}

/// Shared front door of the [`pdf`] and [`cbz`] combiners: refuse an empty input list,
/// then an `output` whose extension is not `ext`, both before any combine work.
fn require_combine_inputs(output: &str, inputs: &[String], ext: &str) -> Result<(), String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    require_output_ext(output, ext)
}

/// Combine images into one PDF (one page each). The destination must be a `.pdf` and must
/// not be one of the inputs (2026-09-05 audit, F30); inputs the composer cannot use are
/// reported per input, or refused outright under `opts.strict` (F31).
pub fn pdf(output: &str, inputs: &[String], opts: CombineOpts) -> Result<String, String> {
    require_combine_inputs(output, inputs, "pdf")?;
    // Same JPEG quality the right-click Combine-to-PDF verb uses (the user's configured
    // setting) — a hardcoded 85 silently diverged from the menu path for no reason.
    let combine = if opts.searchable {
        topdf::combine_to_pdf_searchable
    } else {
        topdf::combine_to_pdf
    };
    let combined = combine(
        inputs,
        Path::new(output),
        st2k_base::settings::jpeg_quality(),
        opts.on_omit(),
    )
    .map_err(|e| format!("pdf build failed: {}", e.message()))?;
    Ok(combined_report(&combined, opts))
}

/// Combine images into one CBZ (comic-book zip) archive, natural-sorted, with a
/// `ComicInfo.xml` sidecar as the first entry. Same combiner the right-click
/// "Combine to CBZ" verb uses (`verbs::actions::handle_combine_to_cbz`) — this is
/// just its CLI/MCP front door, which never existed even though the PDF sibling
/// always had one. Same destination and omission contract as [`pdf`].
pub fn cbz(output: &str, inputs: &[String], opts: CombineOpts) -> Result<String, String> {
    if opts.searchable {
        return Err("--searchable applies to pdf only".to_string());
    }
    require_combine_inputs(output, inputs, "cbz")?;
    let combined = verbs::combine_to_cbz(inputs, Path::new(output), opts.on_omit())
        .map_err(|e| format!("cbz build failed: {}", e.message()))?;
    Ok(combined_report(&combined, opts))
}

#[cfg(test)]
mod tests;
