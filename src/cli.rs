//! Command-line / agent API — the verbs the `st2k` console binary exposes.
//!
//! Every verb reuses the exact same engine the shell extension uses (every format
//! we decode via `decode_full`, the convert/rotate/strip/OCR/PDF logic), so an
//! installed SageThumbs 2K doubles as an offline image toolbox for scripts and
//! AI agents — no extra installs. Each verb returns `Ok(stdout text)` or
//! `Err(message)`; the binary prints and maps to an exit code.

use std::path::Path;

use crate::{decode, formats, ocr, settings, strip, topdf, verbs};

/// `st2k devmode on|off|status`: toggle the developer-test-box flag (the HKCU `DevMachine`
/// value). When ON, this machine's startup manifest request carries `&dev=1`. A plain
/// machine-local flag, not an identifier; OFF on every real install.
pub fn devmode(sub: &str) -> Result<String, String> {
    match sub {
        "on" | "enable" | "1" => {
            settings::set_dev_machine(true).map_err(|_| "couldn't write the DevMachine flag".to_string())?;
            Ok("dev mode ON (this machine's manifest request carries &dev=1).".into())
        }
        "off" | "disable" | "0" => {
            settings::set_dev_machine(false).map_err(|_| "couldn't clear the DevMachine flag".to_string())?;
            Ok("dev mode OFF (this machine's manifest request is unmodified).".into())
        }
        "status" | "" => Ok(format!(
            "dev mode is {} (HKCU\\Software\\SageThumbs2K\\DevMachine)",
            if settings::is_dev_machine() { "ON" } else { "OFF" }
        )),
        other => Err(format!("unknown devmode '{other}' (use: on | off | status)")),
    }
}

/// Render any supported image to `output` (format from its extension) at most
/// `max_dim` px on the long edge (`0` = full size). The headline verb: produces
/// previews for the formats Windows itself can't.
pub fn thumbnail(input: &str, output: &str, max_dim: u32) -> Result<String, String> {
    // Generic archive (.zip/.rar/.7z): the same list-then-extract path Explorer
    // uses — zip/7z stream off the open file (no size cap, the central directory
    // plus the picked entries is all that's read), and the contact sheet composes
    // per the same Setting. Falls through to the normal decode if it isn't really
    // an archive (renamed file) so the magic-dispatch tiers still get their shot.
    if let Some(img) = archive_thumbnail(input) {
        let out = if max_dim > 0 { img.thumbnail(max_dim, max_dim) } else { img };
        out.save(output).map_err(|e| e.to_string())?;
        return Ok(output.to_string());
    }
    // Cap the read at the shared input budget (metadata-checked before allocating)
    // so a scripted/agent/MCP call can't load a multi-GB file wholesale — the same
    // ceiling Explorer thumbnailing and the path verbs apply. Head-preview
    // containers (.blend / PSD-PSB) past the cap still render from a bounded prefix.
    let bytes = decode::read_preview_capped(input).map_err(|e| e.to_string())?;
    // Preview fidelity (embedded/container previews OK) — that's what a
    // thumbnail is; `convert` is the full-fidelity verb.
    let img = decode::decode_preview(&bytes).map_err(|_| format!("cannot decode {input}"))?;
    let out = if max_dim > 0 { img.thumbnail(max_dim, max_dim) } else { img };
    out.save(output).map_err(|e| e.to_string())?;
    Ok(output.to_string())
}

/// The generic-archive cover/contact-sheet for a `.zip`/`.rar`/`.7z` PATH, or None
/// to take the normal decode route (not an archive extension, unreadable, or no
/// image entries — the CLI then reports "cannot decode", mirroring the shell's
/// stock-icon fallback). 1024px edge matches the preview pane's compose target.
fn archive_thumbnail(input: &str) -> Option<image::DynamicImage> {
    use std::io::Read;
    let ext = Path::new(input).extension().and_then(|e| e.to_str()).unwrap_or("");
    if !crate::formats::is_archive(ext) {
        return None;
    }
    let want = if crate::settings::archive_collage() { 4 } else { 1 };
    let mut f = std::fs::File::open(input).ok()?;
    let mut head = [0u8; 8];
    f.read_exact(&mut head).ok()?;
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0)).ok()?;
    let covers = if crate::container::archive_needs_buffer(&head) {
        // RAR buffers whole (`rars` accepts no reader) — same bounded read as the
        // normal path, so a multi-GB .rar fails to the normal decode error.
        let bytes = decode::read_preview_capped(input).ok()?;
        crate::container::archive_covers(&bytes, want)?
    } else {
        crate::container::archive_covers_seek(&mut f, &head, want)?
    };
    let d = decode::thumbnail_from_covers(&covers, 1024).ok()?;
    image::RgbaImage::from_raw(d.width, d.height, d.rgba).map(image::DynamicImage::ImageRgba8)
}

/// Convert `input` to the exact `output` path at `quality`, optional `resize`.
/// `webp_quality = Some(q)` writes lossy WebP at quality `q` (only meaningful when
/// `output` is a `.webp`); `None` keeps WebP lossless.
pub fn convert(input: &str, output: &str, quality: u8, webp_quality: Option<u8>, resize: verbs::Resize) -> Result<String, String> {
    verbs::convert_to(input, Path::new(output), quality, webp_quality, resize).map_err(|_| format!("convert failed: {input}"))?;
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
        _ => return Err(format!("unknown rotation '{by}' (right|left|180|fliph|flipv)")),
    };
    verbs::transform_file(input, t)
        .map(|p| p.display().to_string())
        .map_err(|_| format!("rotate failed: {input}"))
}

/// Decode `input` and return it as in-memory PNG bytes, fit within `max_dim` (0 = full
/// size). Powers the MCP `view` tool — lets an AI agent SEE any of our supported formats
/// directly (HEIC/RAW/PSD/ebook covers/CAD previews/…), not just convert them to a file.
pub fn view_png(input: &str, max_dim: u32) -> Result<Vec<u8>, String> {
    let bytes = decode::read_preview_capped(input).map_err(|e| e.to_string())?;
    let img = decode::decode_preview(&bytes).map_err(|_| format!("cannot decode {input}"))?;
    let img = if max_dim > 0 { img.thumbnail(max_dim, max_dim) } else { img };
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Compress to a target file size → a "(compressed)" JPEG sibling at or under
/// `target_bytes` (quality binary-search + downscale fallback). See [`parse_size`].
pub fn compress(input: &str, target_bytes: u64) -> Result<String, String> {
    verbs::compress_to_size(input, target_bytes)
        .map(|p| p.display().to_string())
        .map_err(|_| format!("compress failed: {input}"))
}

/// Parse a human size — `"1MB"`, `"500KB"`, `"800kb"`, or a bare byte count `"800000"` —
/// into bytes. Decimal units (1KB = 1000 B), case-insensitive, optional trailing `B`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let core = lower.strip_suffix('b').unwrap_or(&lower); // tolerate MB/KB/B
    let (num, mult) = if let Some(n) = core.strip_suffix('m') {
        (n, 1_000_000u64)
    } else if let Some(n) = core.strip_suffix('k') {
        (n, 1_000)
    } else {
        (core, 1)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad size '{s}' (try 1MB / 500KB / 800000)"))?;
    if v <= 0.0 {
        return Err(format!("size must be positive: '{s}'"));
    }
    Ok((v * mult as f64) as u64)
}

/// Strip EXIF/IPTC/XMP metadata in place (JPEG/PNG, lossless).
pub fn strip_meta(input: &str) -> Result<String, String> {
    strip::strip_metadata(input).map_err(|_| format!("strip failed (JPEG/PNG only): {input}"))?;
    Ok(format!("stripped {input}"))
}

/// OCR an image to plain text on stdout.
pub fn ocr(input: &str) -> Result<String, String> {
    // Same shared input cap as `thumbnail` — OCR additionally copies the bytes onto
    // a worker thread, so an uncapped huge file would be ~2x its size in memory.
    let bytes = decode::read_capped(input).map_err(|e| e.to_string())?;
    // Propagate the REAL error — "no text", "no language pack", and "decode failed" are
    // three different, actionable situations (especially for an MCP/AI caller parsing this).
    ocr::recognize_bytes(&bytes)
        .map_err(|e| format!("OCR failed: {e} (no text found, or no OCR language pack installed)"))
}

/// Combine images into one PDF (one page each).
pub fn pdf(output: &str, inputs: &[String]) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    // Same JPEG quality the right-click Combine-to-PDF verb uses (the user's configured
    // setting) — a hardcoded 85 silently diverged from the menu path for no reason.
    topdf::combine_to_pdf(inputs, Path::new(output), crate::settings::jpeg_quality())
        .map_err(|_| "pdf build failed".to_string())?;
    Ok(output.to_string())
}

/// Image dimensions + EXIF (camera/date/GPS), as text or JSON.
pub fn info(input: &str, json: bool) -> Result<String, String> {
    let i = strip::read_info(input);
    if i.width == 0 && i.height == 0 {
        return Err(format!("cannot read {input}"));
    }
    if json {
        // A malformed EXIF rational (0 denominator) can produce inf/NaN; drop it
        // rather than emit `NaN`, which is not valid JSON.
        let gps = i.gps.filter(|(a, b)| a.is_finite() && b.is_finite()).map(|(a, b)| [a, b]);
        Ok(serde_json::json!({
            "width": i.width,
            "height": i.height,
            "make": i.make,
            "model": i.model,
            "datetime": i.datetime,
            "gps": gps,
        })
        .to_string())
    } else {
        let mut s = format!("{} x {} px", i.width, i.height);
        if let Some(m) = &i.make {
            s.push_str(&format!("\ncamera: {m}"));
        }
        if let Some(m) = &i.model {
            s.push_str(&format!(" {m}"));
        }
        if let Some(d) = &i.datetime {
            s.push_str(&format!("\ntaken: {d}"));
        }
        if let Some((la, lo)) = i.gps {
            s.push_str(&format!("\ngps: {la:.5}, {lo:.5}"));
        }
        Ok(s)
    }
}

/// Parse the optional `resize` argument ("WxH" fit, no upscale; or "N%" scale)
/// into a [`verbs::Resize`]. `None`/empty → `Resize::None`. Shared by the CLI
/// (`st2k convert --resize`) and the MCP `convert` tool so the syntax stays
/// identical in both front ends.
pub fn parse_resize(s: Option<&str>) -> Result<verbs::Resize, String> {
    let Some(v) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(verbs::Resize::None);
    };
    if let Some(p) = v.strip_suffix('%') {
        let pct: u32 = p.trim().parse().map_err(|_| format!("bad percent '{v}'"))?;
        return Ok(verbs::Resize::Percent(pct.clamp(1, 1000)));
    }
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(|| format!("bad resize '{v}' (use WxH or N%)"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width in '{v}'"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height in '{v}'"))?;
    Ok(verbs::Resize::Fit(w.max(1), h.max(1)))
}

/// Expand `inputs` (files and/or directories) into a flat list of SUPPORTED image
/// files (directories are scanned one level deep; unsupported extensions dropped).
fn expand_inputs(inputs: &[String]) -> Vec<String> {
    fn supported(p: &Path) -> bool {
        // `is_known` is ASCII-case-insensitive — no lowercase allocation needed.
        p.extension().and_then(|e| e.to_str()).is_some_and(formats::is_known)
    }
    let mut out = Vec::new();
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    let ep = e.path();
                    if ep.is_file() && supported(&ep) {
                        out.push(ep.to_string_lossy().into_owned());
                    }
                }
            }
        } else if p.is_file() && supported(p) {
            out.push(i.clone());
        }
    }
    out
}

/// BULK process many inputs (files and/or folders) in ONE process, fanned out
/// across all cores via the shared batch pool — the fast path for the regression
/// harness and AI agents (no more one `st2k` spawn per file). `op` is `thumbnail`
/// (→ PNG at `size`px) or `convert` (→ `to_ext`, honoring `quality`/`resize`).
/// Outputs go to `out_dir` (created if needed) or next to each source. Returns a
/// `done/total` summary.
#[allow(clippy::too_many_arguments)]
pub fn batch(
    op: &str,
    inputs: &[String],
    out_dir: Option<&str>,
    size: u32,
    to_ext: Option<&str>,
    quality: u8,
    resize: verbs::Resize,
) -> Result<String, String> {
    let is_convert = match op {
        "thumbnail" | "thumb" => false,
        "convert" => true,
        other => return Err(format!("unknown batch op '{other}' (thumbnail|convert)")),
    };
    let ext = if is_convert {
        to_ext.ok_or("batch convert needs --to <ext>")?.trim_start_matches('.').to_ascii_lowercase()
    } else {
        "png".to_string()
    };

    let files = expand_inputs(inputs);
    if files.is_empty() {
        return Err("no supported image files found in the inputs".to_string());
    }
    if let Some(d) = out_dir {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create output dir {d}: {e}"))?;
    }

    // Pre-compute collision-free output paths SERIALLY, so the parallel pass never
    // races on a name (two sources with the same stem → `name`, `name (1)`, …).
    let mut used = std::collections::HashSet::new();
    let mut pairs: Vec<(String, std::path::PathBuf)> = Vec::with_capacity(files.len());
    for f in &files {
        let src = Path::new(f);
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
        let dir = match out_dir {
            Some(d) => std::path::PathBuf::from(d),
            None => src.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from(".")),
        };
        let mut out = dir.join(format!("{stem}.{ext}"));
        let mut n = 1u32;
        while used.contains(&out) || out.exists() {
            out = dir.join(format!("{stem} ({n}).{ext}"));
            n += 1;
        }
        used.insert(out.clone());
        pairs.push((f.clone(), out));
    }

    // Fan out: each (input, pre-reserved output) is independent → no naming race.
    let results = crate::parallel::map(&pairs, |_, (input, output)| -> bool {
        if is_convert {
            verbs::convert_to(input, output, quality, None, resize).is_ok()
        } else {
            thumbnail(input, &output.to_string_lossy(), size).is_ok()
        }
    });
    let done = results.iter().filter(|&&ok| ok).count();
    let total = files.len();
    // Total failure must FAIL the command (nonzero exit for scripts/CI/MCP callers) — a
    // "0/12 succeeded" with exit code 0 was indistinguishable from a good run without
    // parsing English stdout. Partial success stays Ok but now names the failure count.
    if done == 0 {
        return Err(format!("0/{total} succeeded"));
    }
    if done < total {
        return Ok(format!("{done}/{total} succeeded ({} failed)", total - done));
    }
    Ok(format!("{done}/{total} succeeded"))
}

/// `st2k upload-hosts [--open]` — show (or open) the user-editable upload-hosts config
/// file. The right-click "Upload" verb and the screenshot Upload button read this file
/// to decide which keyless host(s) to POST to; editing it lets you reorder / add hosts
/// or point at your own server. The documented template is created on first use. Path +
/// template are shared with the app via [`crate::upload_config`].
pub fn upload_hosts(open: bool) -> Result<String, String> {
    let path = crate::upload_config::ensure_config()
        .ok_or_else(|| "couldn't resolve %APPDATA% for the upload-hosts config path".to_string())?;
    let p = path.display().to_string();
    if open {
        // Open in the default editor (same "ShellExecute open" the Settings button uses).
        unsafe {
            use windows::core::{w, PCWSTR};
            use windows::Win32::UI::Shell::ShellExecuteW;
            use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
            let file = crate::wide(&p);
            ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
        Ok(format!("Opening upload-hosts config in your default editor:\n{p}"))
    } else {
        Ok(format!(
            "Upload-hosts config file:\n{p}\n\n\
             Edit it to choose / reorder / add upload hosts \u{2014} one host per line:\n  \
             <https-url> | <field> | text|json | extra=value ...\n\
             While every line is commented out, SageThumbs 2K uses its built-in defaults.\n\
             Run `st2k upload-hosts --open` to open it in your editor."
        ))
    }
}

/// List every supported input extension (with category + description).
pub fn list_formats(json: bool) -> String {
    if json {
        let items: Vec<_> = formats::FORMATS
            .iter()
            .map(|(ext, desc)| {
                serde_json::json!({
                    "ext": ext,
                    "category": formats::category_label(formats::category(ext)),
                    "description": desc,
                })
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    } else {
        let mut s = format!("{} supported input formats:\n", formats::FORMATS.len());
        for (ext, desc) in formats::FORMATS {
            s.push_str(&format!("  .{ext:<6} {desc}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_thumbnail_and_info_and_formats() {
        let dir = std::env::temp_dir().join(format!("st2k_cli_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(400, 300)).save(&src).unwrap();
        let sp = src.to_str().unwrap();

        let out = dir.join("t.png");
        thumbnail(sp, out.to_str().unwrap(), 128).unwrap();
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 128 && d.height() <= 128 && d.width() == 128);

        let cv = dir.join("a.jpg");
        convert(sp, cv.to_str().unwrap(), 85, None, verbs::Resize::Fit(100, 100)).unwrap();
        assert!(image::open(&cv).unwrap().width() <= 100);

        assert!(info(sp, true).unwrap().contains("\"width\":400"));
        assert!(list_formats(false).contains(".png"));
        assert!(list_formats(true).starts_with('['));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
