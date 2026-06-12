//! Command-line / agent API — the verbs the `st2k` console binary exposes.
//!
//! Every verb reuses the exact same engine the shell extension uses (185-format
//! decode via `decode_full`, the convert/rotate/strip/OCR/PDF logic), so an
//! installed SageThumbs 2K doubles as an offline image toolbox for scripts and
//! AI agents — no extra installs. Each verb returns `Ok(stdout text)` or
//! `Err(message)`; the binary prints and maps to an exit code.

use std::path::Path;

use crate::{decode, formats, ocr, strip, topdf, verbs};

/// Render any supported image to `output` (format from its extension) at most
/// `max_dim` px on the long edge (`0` = full size). The headline verb: produces
/// previews for the ~185 types Windows itself can't.
pub fn thumbnail(input: &str, output: &str, max_dim: u32) -> Result<String, String> {
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    // Preview fidelity (embedded/container previews OK) — that's what a
    // thumbnail is; `convert` is the full-fidelity verb.
    let img = decode::decode_preview(&bytes).map_err(|_| format!("cannot decode {input}"))?;
    let out = if max_dim > 0 { img.thumbnail(max_dim, max_dim) } else { img };
    out.save(output).map_err(|e| e.to_string())?;
    Ok(output.to_string())
}

/// Convert `input` to the exact `output` path at `quality`, optional `resize`.
pub fn convert(input: &str, output: &str, quality: u8, resize: verbs::Resize) -> Result<String, String> {
    verbs::convert_to(input, Path::new(output), quality, resize).map_err(|_| format!("convert failed: {input}"))?;
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

/// Strip EXIF/IPTC/XMP metadata in place (JPEG/PNG, lossless).
pub fn strip_meta(input: &str) -> Result<String, String> {
    strip::strip_metadata(input).map_err(|_| format!("strip failed (JPEG/PNG only): {input}"))?;
    Ok(format!("stripped {input}"))
}

/// OCR an image to plain text on stdout.
pub fn ocr(input: &str) -> Result<String, String> {
    let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
    ocr::recognize_bytes(&bytes).map_err(|_| "no text found / OCR language pack not installed".to_string())
}

/// Combine images into one PDF (one page each).
pub fn pdf(output: &str, inputs: &[String]) -> Result<String, String> {
    if inputs.is_empty() {
        return Err("no input images".to_string());
    }
    topdf::combine_to_pdf(inputs, Path::new(output), 85).map_err(|_| "pdf build failed".to_string())?;
    Ok(output.to_string())
}

/// Image dimensions + EXIF (camera/date/GPS), as text or JSON.
pub fn info(input: &str, json: bool) -> Result<String, String> {
    let i = strip::read_info(input);
    if i.width == 0 && i.height == 0 {
        return Err(format!("cannot read {input}"));
    }
    if json {
        let gps = i.gps.map(|(a, b)| format!("[{a},{b}]")).unwrap_or_else(|| "null".to_string());
        Ok(format!(
            "{{\"width\":{},\"height\":{},\"make\":{},\"model\":{},\"datetime\":{},\"gps\":{}}}",
            i.width,
            i.height,
            json_str(&i.make),
            json_str(&i.model),
            json_str(&i.datetime),
            gps
        ))
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

/// List every supported input extension (with category + description).
pub fn list_formats(json: bool) -> String {
    if json {
        let items: Vec<String> = formats::FORMATS
            .iter()
            .map(|(ext, desc)| {
                let cat = formats::category_label(formats::category(ext));
                format!("{{\"ext\":\"{ext}\",\"category\":\"{cat}\",\"description\":{}}}", json_lit(desc))
            })
            .collect();
        format!("[{}]", items.join(","))
    } else {
        let mut s = format!("{} supported input formats:\n", formats::FORMATS.len());
        for (ext, desc) in formats::FORMATS {
            s.push_str(&format!("  .{ext:<6} {desc}\n"));
        }
        s
    }
}

/// JSON string literal for an `Option<String>` (null when None).
fn json_str(o: &Option<String>) -> String {
    o.as_deref().map(json_lit).unwrap_or_else(|| "null".to_string())
}

/// Minimal JSON string escaping.
fn json_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_thumbnail_and_info_and_formats() {
        let dir = std::env::temp_dir().join("st2k_cli");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(400, 300)).save(&src).unwrap();
        let sp = src.to_str().unwrap();

        let out = dir.join("t.png");
        thumbnail(sp, out.to_str().unwrap(), 128).unwrap();
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 128 && d.height() <= 128 && d.width() == 128);

        let cv = dir.join("a.jpg");
        convert(sp, cv.to_str().unwrap(), 85, verbs::Resize::Fit(100, 100)).unwrap();
        assert!(image::open(&cv).unwrap().width() <= 100);

        assert!(info(sp, true).unwrap().contains("\"width\":400"));
        assert!(list_formats(false).contains(".png"));
        assert!(list_formats(true).starts_with('['));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
