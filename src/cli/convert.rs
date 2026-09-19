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
    if crate::formats::is_archive(archive_ext) {
        reject_oversized_archive(input, crate::settings::max_file_size_bytes())
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
    let img = match decode::decode_preview_streamed(input, edge) {
        Some(img) => img,
        None => {
            // `..._for`: this verb named a size, so a head prefix whose baked preview
            // cannot reach it must not stand in for the real picture (issue #33).
            let bytes = decode::read_preview_capped_for(input, edge)
                .map_err(|e| (OmitCause::Unreadable, e.to_string()))?;
            // Cap the decode at the edge we're about to shrink to anyway — the streamed
            // path above already takes `edge`, and rendering ImageMagick's full 4096 first
            // costs seconds on a big scan for pixels this immediately discards.
            decode::decode_preview_capped_for_path(&bytes, edge, input)
                .map_err(|_| (OmitCause::Undecodable, format!("cannot decode {input}")))?
        }
    };
    let out = fit_for_cli(img, max_dim);
    save_atomic(&out, output, format).map_err(|e| (OmitCause::Unencodable, e))?;
    Ok(output.to_string())
}

/// Fail before opening or parsing a generic archive when either the user's
/// MaxSize preference or the shared hard input ceiling rejects its metadata
/// length. `configured_max == u64::MAX` is Settings' "Unlimited" representation.
fn reject_oversized_archive(input: &str, configured_max: u64) -> Result<(), String> {
    let max = decode::effective_input_cap(configured_max);
    if let Ok(meta) = std::fs::metadata(input) {
        if meta.len() > max {
            return Err(format!(
                "input is {} bytes, over the effective archive limit of {max} bytes",
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
    use std::io::Read;
    let ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !crate::formats::is_archive(ext) {
        return None;
    }
    let want = if crate::settings::archive_collage() {
        4
    } else {
        1
    };
    let mut f = std::fs::File::open(input).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).ok()?;
    let prefs = crate::container::select::CoverPrefs::from_settings();
    let covers = if crate::container::archive_needs_buffer(&head) {
        // RAR buffers whole (`rars` accepts no reader) — same bounded read as the
        // normal path, so a multi-GB .rar fails to the normal decode error.
        let bytes = decode::read_preview_capped(input).ok()?;
        crate::container::archive_covers(&bytes, want, &prefs)?
    } else {
        crate::container::archive_covers_seek(&mut f, &head, want, &prefs)?
    };
    let d = decode::thumbnail_from_covers(&covers, 1024).ok()?;
    image::RgbaImage::from_raw(d.width, d.height, d.rgba).map(image::DynamicImage::ImageRgba8)
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
/// same lines a `--strict` refusal carries. The JSON form is [`verbs::Combined::to_json`].
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
    s
}

/// Combine images into one PDF (one page each). The destination must be a `.pdf` and must
/// not be one of the inputs (2026-09-05 audit, F30); inputs the composer cannot use are
/// reported per input, or refused outright under `opts.strict` (F31).
pub fn pdf(output: &str, inputs: &[String], opts: CombineOpts) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    require_output_ext(output, "pdf")?;
    // Same JPEG quality the right-click Combine-to-PDF verb uses (the user's configured
    // setting) — a hardcoded 85 silently diverged from the menu path for no reason.
    let combined = topdf::combine_to_pdf(
        inputs,
        Path::new(output),
        crate::settings::jpeg_quality(),
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
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    require_output_ext(output, "cbz")?;
    let combined = verbs::combine_to_cbz(inputs, Path::new(output), opts.on_omit())
        .map_err(|e| format!("cbz build failed: {}", e.message()))?;
    Ok(combined_report(&combined, opts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unlimited_archive_setting_still_rejects_before_parse_at_hard_cap() {
        let path = std::env::temp_dir().join(format!(
            "st2k_cli_oversized_{}_{}.7z",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::File::create(&path).unwrap();
        // A real 7z signature makes this representative if a future refactor
        // accidentally moves the gate after format probing. set_len keeps the
        // test sparse/fast instead of writing 256 MiB.
        file.write_all(b"7z\xBC\xAF\x27\x1C").unwrap();
        file.set_len(decode::limits::MAX_INPUT_BYTES + 1).unwrap();
        drop(file);

        let err = reject_oversized_archive(path.to_str().unwrap(), u64::MAX).unwrap_err();
        assert!(err.contains(&decode::limits::MAX_INPUT_BYTES.to_string()));

        let _ = std::fs::remove_file(path);
    }

    /// A caller that bypasses both front ends' own clamping (a raw library call, or a
    /// future front end that forgets to clamp) must still get a sane encoder quality —
    /// `convert`/`batch` own the clamp now, not just `mcp.rs`.
    #[test]
    fn convert_and_batch_clamp_quality_into_1_to_100() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_qclamp_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(32, 32))
            .save(&src)
            .unwrap();

        // quality: 0 must not panic/misbehave the encoder — it should behave as if 1 was
        // requested, not literally zero.
        let out = dir.join("a.jpg");
        convert(
            src.to_str().unwrap(),
            out.to_str().unwrap(),
            0,
            None,
            verbs::Resize::None,
            false,
        )
        .unwrap();
        assert!(out.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CBZ front door must exist and actually produce a readable archive —
    /// `combine_to_cbz` itself is already tested in `verbs.rs`; this pins the CLI/MCP-facing
    /// `cli::cbz` wrapper specifically (the missing piece the review found).
    #[test]
    fn cbz_combines_images_into_a_readable_archive() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_cbz_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("page1.png");
        let b = dir.join("page2.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10))
            .save(&a)
            .unwrap();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10))
            .save(&b)
            .unwrap();

        let out = dir.join("comic.cbz");
        let text = cbz(
            out.to_str().unwrap(),
            &[
                a.to_str().unwrap().to_string(),
                b.to_str().unwrap().to_string(),
            ],
            CombineOpts::default(),
        )
        .unwrap();
        assert_eq!(
            text,
            out.to_str().unwrap(),
            "an all-good combine prints exactly the output path"
        );
        assert!(out.exists());
        let f = std::fs::File::open(&out).unwrap();
        let zip = zip::ZipArchive::new(f).unwrap();
        assert!(
            zip.len() >= 2,
            "expected at least the two pages plus the sidecar"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn save_png(path: &Path, w: u32, h: u32) -> String {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([40, 90, 200]),
        ))
        .save(path)
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    /// 2026-09-05 audit, F30: `thumbnail same.png same.png` used to decode the file and then
    /// save the 128 px result over it, destroying the original. Every spelling of the input
    /// (itself, a case variant, a `..` detour, a hard link) must be refused BEFORE anything
    /// is written, and the input must be byte-identical afterwards. Against the pre-fix code
    /// this fails at the first `is_err` (the call succeeds) and the bytes differ.
    #[test]
    fn thumbnail_refuses_every_spelling_of_its_own_input() {
        let dir = scratch("alias");
        let src = save_png(&dir.join("Photo.png"), 300, 200);
        let before = std::fs::read(&src).unwrap();
        let link = dir.join("link.png");
        std::fs::hard_link(&src, &link).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        let aliases = [
            src.clone(),
            dir.join("PHOTO.PNG").to_string_lossy().into_owned(),
            dir.join("sub")
                .join("..")
                .join("Photo.png")
                .to_string_lossy()
                .into_owned(),
            link.to_string_lossy().into_owned(),
        ];
        for alias in &aliases {
            let err = thumbnail(&src, alias, 128).expect_err(alias);
            assert!(err.contains("same file"), "the refusal must say why: {err}");
            assert_eq!(
                std::fs::read(&src).unwrap(),
                before,
                "the input was modified via alias {alias}"
            );
        }

        // A distinct destination still works.
        let out = dir.join("thumb.png");
        thumbnail(&src, out.to_str().unwrap(), 128).unwrap();
        assert!(image::open(&out).unwrap().width() <= 128);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `thumbnail` wrote its output with a plain `DynamicImage::save`,
    /// which truncates the destination BEFORE encoding, so an encode failure left a 0-byte
    /// file where a good one had been. The write now goes through the shared atomic writer.
    /// ICO is the deterministic failure: its header cannot express an edge over 256 px, so a
    /// full-size render of a 400 px source fails inside the encoder, after the file is open.
    /// Against the pre-fix code the existing `out.ico` is left at 0 bytes.
    #[test]
    fn a_failed_thumbnail_write_leaves_an_existing_output_intact() {
        let dir = scratch("atomic");
        let src = save_png(&dir.join("big.png"), 400, 300);
        let out = dir.join("out.ico");
        let existing = b"an existing icon the user wants to keep".to_vec();
        std::fs::write(&out, &existing).unwrap();

        let err = thumbnail(&src, out.to_str().unwrap(), 0).expect_err("ico cannot hold 400 px");
        assert!(!err.is_empty());
        assert_eq!(
            std::fs::read(&out).unwrap(),
            existing,
            "a failed write must not touch the existing destination"
        );
        assert!(
            verbs::staging_leftovers(&out).is_empty(),
            "the temp file must be cleaned up"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F31: a PDF from one good PNG, one corrupt PNG and one missing file
    /// used to exit 0 printing only the output path. The report now carries the counts, a
    /// `partial` status and one machine-readable line per omitted input with a DISTINCT
    /// cause (corrupt = undecodable, missing = unreadable), in both the text and the JSON
    /// form; `strict` refuses to write at all. Against the pre-fix code the text is the bare
    /// path (no `partial:` line) and the JSON form does not exist.
    #[test]
    fn pdf_reports_each_omitted_input_with_its_cause_and_strict_refuses() {
        let dir = scratch("pdf_partial");
        let good = save_png(&dir.join("good.png"), 30, 20);
        let corrupt = dir.join("corrupt.png");
        std::fs::write(&corrupt, b"not a png at all").unwrap();
        let corrupt = corrupt.to_str().unwrap().to_string();
        let missing = dir.join("missing.png").to_str().unwrap().to_string();
        let inputs = [good.clone(), corrupt.clone(), missing.clone()];

        let out = dir.join("out.pdf");
        let text = pdf(out.to_str().unwrap(), &inputs, CombineOpts::default()).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            out.to_str(),
            "first line stays the output path"
        );
        assert_eq!(
            lines.next(),
            Some("partial: 1 of 3 inputs combined, 2 omitted")
        );
        let omitted: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
        assert_eq!(omitted.len(), 2, "{text}");
        for row in &omitted {
            assert_eq!(row[0], "omitted");
            assert_eq!(
                row.len(),
                4,
                "omitted<TAB>input<TAB>cause<TAB>detail: {row:?}"
            );
        }
        let cause_of = |input: &str| {
            omitted
                .iter()
                .find(|r| r[1] == input)
                .map(|r| r[2])
                .unwrap_or_else(|| panic!("{input} not listed in {text}"))
        };
        assert_eq!(cause_of(&corrupt), "undecodable");
        assert_eq!(cause_of(&missing), "unreadable");

        // The JSON form carries the same facts for an MCP caller.
        let json_out = dir.join("out2.pdf");
        let opts = CombineOpts {
            json: true,
            ..CombineOpts::default()
        };
        let v: serde_json::Value =
            serde_json::from_str(&pdf(json_out.to_str().unwrap(), &inputs, opts).unwrap()).unwrap();
        assert_eq!(v["status"], "partial");
        assert_eq!(v["requested"], 3);
        assert_eq!(v["combined"], 1);
        assert_eq!(v["output"], json_out.to_str().unwrap());
        let causes: Vec<(&str, &str)> = v["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| (o["input"].as_str().unwrap(), o["cause"].as_str().unwrap()))
            .collect();
        assert!(causes.contains(&(corrupt.as_str(), "undecodable")), "{v}");
        assert!(causes.contains(&(missing.as_str(), "unreadable")), "{v}");

        // Strict: fail, name both, write nothing.
        let strict_out = dir.join("strict.pdf");
        let opts = CombineOpts {
            strict: true,
            ..CombineOpts::default()
        };
        let err = pdf(strict_out.to_str().unwrap(), &inputs, opts).unwrap_err();
        assert!(!strict_out.exists(), "strict must not write a partial PDF");
        assert!(err.contains("strict"), "{err}");
        assert!(err.contains(&corrupt) && err.contains(&missing), "{err}");

        // An all-good combine still prints exactly the path, and its JSON says "ok".
        let clean = dir.join("clean.pdf");
        assert_eq!(
            pdf(
                clean.to_str().unwrap(),
                std::slice::from_ref(&good),
                CombineOpts::default()
            )
            .unwrap(),
            clean.to_str().unwrap()
        );
        let opts = CombineOpts {
            json: true,
            ..CombineOpts::default()
        };
        let clean2 = dir.join("clean2.pdf");
        let v: serde_json::Value =
            serde_json::from_str(&pdf(clean2.to_str().unwrap(), &[good], opts).unwrap()).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["omitted"].as_array().map(Vec::len), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CBZ sibling of the test above: a missing input is listed as `unreadable` with its
    /// count, and `strict` writes nothing. Against the pre-fix code the text is the bare path.
    #[test]
    fn cbz_reports_a_missing_input_as_unreadable_and_strict_refuses() {
        let dir = scratch("cbz_partial");
        let good = save_png(&dir.join("p1.png"), 10, 10);
        let missing = dir.join("p2.png").to_str().unwrap().to_string();
        let inputs = [good, missing.clone()];

        let out = dir.join("out.cbz");
        let text = cbz(out.to_str().unwrap(), &inputs, CombineOpts::default()).unwrap();
        assert!(
            text.contains("partial: 1 of 2 inputs combined, 1 omitted"),
            "{text}"
        );
        assert!(
            text.contains(&format!("omitted\t{missing}\tunreadable\t")),
            "{text}"
        );
        let zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), 2, "the sidecar plus the one readable page");

        let strict_out = dir.join("strict.cbz");
        let opts = CombineOpts {
            strict: true,
            ..CombineOpts::default()
        };
        let err = cbz(strict_out.to_str().unwrap(), &inputs, opts).unwrap_err();
        assert!(!strict_out.exists(), "strict must not write a partial CBZ");
        assert!(err.contains(&missing), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `pdf same.png same.png` and `cbz same.png same.png` exited 0
    /// and replaced the PNG with PDF/ZIP bytes. The destination must carry the composer's
    /// extension, and an output that IS an input (here a real PDF/CBZ re-combined over itself,
    /// the same-type alias the extension test cannot catch) is refused with the source left
    /// byte-identical. Against the pre-fix code every one of these calls succeeds.
    #[test]
    fn pdf_and_cbz_refuse_a_foreign_extension_and_an_output_that_is_an_input() {
        let dir = scratch("pdf_cbz_alias");
        let png = save_png(&dir.join("same.png"), 30, 20);
        let png_before = std::fs::read(&png).unwrap();

        type Verb = fn(&str, &[String], CombineOpts) -> Result<String, String>;
        let verbs: [(Verb, &str); 2] = [(pdf, "pdf"), (cbz, "cbz")];
        for (verb, name) in verbs {
            let err =
                verb(&png, std::slice::from_ref(&png), CombineOpts::default()).expect_err(name);
            assert!(err.contains(&format!(".{name}")), "{name}: {err}");
            assert_eq!(
                std::fs::read(&png).unwrap(),
                png_before,
                "{name} touched the PNG"
            );
        }

        let doc = dir.join("doc.pdf");
        pdf(
            doc.to_str().unwrap(),
            std::slice::from_ref(&png),
            CombineOpts::default(),
        )
        .unwrap();
        let doc_before = std::fs::read(&doc).unwrap();
        let err = pdf(
            dir.join("DOC.PDF").to_str().unwrap(),
            &[png.clone(), doc.to_str().unwrap().to_string()],
            CombineOpts::default(),
        )
        .expect_err("a PDF re-combined over itself");
        assert!(err.contains("same file"), "{err}");
        assert_eq!(
            std::fs::read(&doc).unwrap(),
            doc_before,
            "the PDF was modified"
        );

        let comic = dir.join("comic.cbz");
        cbz(
            comic.to_str().unwrap(),
            std::slice::from_ref(&png),
            CombineOpts::default(),
        )
        .unwrap();
        let comic_before = std::fs::read(&comic).unwrap();
        let err = cbz(
            comic.to_str().unwrap(),
            &[png, comic.to_str().unwrap().to_string()],
            CombineOpts::default(),
        )
        .expect_err("a CBZ re-combined over itself");
        assert!(err.contains("same file"), "{err}");
        assert_eq!(
            std::fs::read(&comic).unwrap(),
            comic_before,
            "the CBZ was modified"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F30: `convert` has no in-place form either, so the same alias
    /// guard applies; a distinct destination still converts.
    #[test]
    fn convert_refuses_an_output_that_is_its_input() {
        let dir = scratch("convert_alias");
        let src = save_png(&dir.join("a.png"), 20, 20);
        let before = std::fs::read(&src).unwrap();
        let err = convert(&src, &src, 90, None, verbs::Resize::None, false).unwrap_err();
        assert!(err.contains("same file"), "{err}");
        assert_eq!(std::fs::read(&src).unwrap(), before);
        let out = dir.join("a.jpg");
        convert(
            &src,
            out.to_str().unwrap(),
            90,
            None,
            verbs::Resize::None,
            false,
        )
        .unwrap();
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-09-05 audit, F32: `compress x.png --max-size 1` used to succeed with a 628-byte
    /// JPEG while the help promised "at or under". One policy now: a target the search cannot
    /// meet (0, 1) fails, writes nothing, and names the smallest size it can reach; an exact
    /// fit and a normal target succeed at or under the target. Against the pre-fix code the
    /// first two calls succeed and leave a "(compressed)" file behind.
    #[test]
    fn compress_refuses_an_impossible_target_and_honours_a_feasible_one() {
        let dir = scratch("compress_policy");
        // Per-pixel noise so the JPEG has real size to search over.
        let img = image::RgbImage::from_fn(96, 96, |x, y| {
            let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B)).rotate_left(7);
            image::Rgb([h as u8, (h >> 8) as u8, (h >> 16) as u8])
        });
        let src = dir.join("noise.png");
        image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
        let src = src.to_str().unwrap().to_string();
        let sibling_count = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e.is_ok())
                .count()
        };

        for impossible in [0u64, 1] {
            let err = compress(&src, impossible).unwrap_err();
            assert!(
                err.contains(&format!("cannot fit in {impossible} bytes"))
                    && err.contains("smallest")
                    && err.contains("nothing was written"),
                "{err}"
            );
            assert_eq!(
                sibling_count(),
                1,
                "a refused compress must write nothing: {err}"
            );
        }

        // A normal target: the output is at or under it.
        let normal = compress(&src, 100_000).unwrap();
        let normal_len = std::fs::metadata(&normal).unwrap().len();
        assert!(
            normal_len <= 100_000,
            "{normal_len} bytes over a 100000-byte target"
        );

        // An exact fit: asking for precisely what the search produced must succeed at
        // exactly that size, not be refused as "over".
        let exact = compress(&src, normal_len).unwrap();
        assert_eq!(std::fs::metadata(&exact).unwrap().len(), normal_len);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `compress`'s `map_err` must not drop the underlying error — an MCP/agent caller
    /// needs the REAL reason (missing file, decode failure, ...), not a bare "compress
    /// failed: <input>" with nothing else to act on.
    #[test]
    fn compress_error_message_keeps_the_underlying_reason() {
        let err = compress("this_file_does_not_exist_at_all.png", 100_000).unwrap_err();
        assert!(
            err.len() > "compress failed: this_file_does_not_exist_at_all.png".len(),
            "error message dropped the underlying reason: {err}"
        );
    }
}
