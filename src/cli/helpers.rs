//! Shared helper code used by the CLI verbs: the output-alias/extension guards, the
//! atomic writer, the CLI's own fit-to-box, and the size/resize argument parsers.

use super::*;

/// Refuse an `output` that is one of the `inputs`, before anything is read or written
/// (2026-09-05 audit, F30). Every exact-destination verb here reads its inputs and then
/// replaces the destination, so `thumbnail same.png same.png` destroyed the original; a
/// case variant, a relative or `..` spelling and a hard link all did the same and none of
/// them is caught by comparing the strings. `fsutil::same_file` compares canonical paths
/// and, for hard links, the file's own identity. No verb here offers an in-place form, so
/// there is no flag that lifts this.
pub(super) fn reject_output_alias<'a>(
    output: &str,
    inputs: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    match crate::fsutil::aliased_input(Path::new(output), inputs) {
        Some(input) => Err(format!(
            "output {output} is the same file as input {input}; refusing to overwrite the \
             source (write to a different path)"
        )),
        None => Ok(()),
    }
}

/// The composer behind a verb writes exactly one file type, so its destination has to say
/// so (2026-09-05 audit, F30): `pdf same.png same.png` used to put PDF bytes into a `.png`.
/// Case-insensitive, like every other extension test in this crate.
pub(super) fn require_output_ext(output: &str, ext: &str) -> Result<(), String> {
    let got = Path::new(output)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if got.eq_ignore_ascii_case(ext) {
        Ok(())
    } else {
        Err(format!(
            "output must be a .{ext} file (got {output}); this command only writes .{ext}"
        ))
    }
}

/// Write `img` to `output` through the shared atomic writer (temp sibling + rename), the
/// same path the convert verbs take. `thumbnail` used to call `DynamicImage::save`, which
/// truncates the destination BEFORE encoding, so a failed encode left a 0-byte file where a
/// good one had been (2026-09-05 audit, F30). `format` was decided from `output`'s extension
/// up front, because the temp file's own extension is `.st2ktmp`.
pub(super) fn save_atomic(
    img: &image::DynamicImage,
    output: &str,
    format: image::ImageFormat,
) -> Result<(), String> {
    verbs::write_atomic(Path::new(output), |tmp| {
        img.save_with_format(tmp, format)
            .map_err(|e| windows::core::Error::new(E_FAIL, format!("write {output}: {e}")))
    })
    .map_err(|e| e.message())
}

/// Render any supported image to `output` (format from its extension) at most
/// The CLI's fit-to-size, kept deliberately SEPARATE from the shell extension's `fit_to_box`.
///
/// Shrinking uses the one shared reduction, so `st2k thumbnail` and the MCP `view` tool now
/// produce the SAME pixels the thumbnail provider does - which is the whole point, since every
/// visual gate in this repo drives this path and used to validate a picture Explorer never
/// drew (see `decode::thumb`'s `the_gates_reduce_a_thumbnail_the_way_the_shell_extension_does`).
///
/// ENLARGING is left exactly as it was, `DynamicImage::thumbnail`, and that is not an oversight.
/// `--size` has always FILLED the box here: a 72x72 APK icon asked for at 256 came back 256x256,
/// verified against the shipped 2.1.2 binary. Routing the small case through the shared
/// reduction (which never enlarges) would have returned 72x72 instead - a better picture by
/// most arguments, and a silent change to the OUTPUT DIMENSIONS of a shipped CLI that scripts
/// and the MCP tool depend on. Improving the filter is not a licence to change the contract, so
/// the two are decided separately: shrink better, enlarge identically.
pub(super) fn fit_for_cli(img: image::DynamicImage, max_dim: u32) -> image::DynamicImage {
    if max_dim == 0 {
        return img;
    }
    if img.width() > max_dim || img.height() > max_dim {
        decode::reduce_to_fit(img, max_dim, max_dim)
    } else {
        img.thumbnail(max_dim, max_dim)
    }
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
    // f64::from_str accepts "inf"/"infinity"/"nan" (any case). Neither is caught by
    // `v <= 0.0` (INFINITY > 0.0 is true; every NaN comparison is false), and the
    // trailing `as u64` cast is Rust's SATURATING float->int cast — inf would
    // silently become u64::MAX and nan would become 0 as a "compress target".
    if !v.is_finite() {
        return Err(format!("size must be a finite number: '{s}'"));
    }
    if v <= 0.0 {
        return Err(format!("size must be positive: '{s}'"));
    }
    Ok((v * mult as f64) as u64)
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
    let (w, h) = v
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("bad resize '{v}' (use WxH or N%)"))?;
    let w: u32 = w
        .trim()
        .parse()
        .map_err(|_| format!("bad width in '{v}'"))?;
    let h: u32 = h
        .trim()
        .parse()
        .map_err(|_| format!("bad height in '{v}'"))?;
    Ok(verbs::Resize::Fit(w.max(1), h.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f64::from_str` accepts "inf"/"infinity"/"nan" (any case) and neither trips
    /// the `v <= 0.0` guard (INFINITY > 0.0; every NaN comparison is false), so
    /// without the `is_finite` check the trailing saturating `as u64` cast would
    /// silently turn "inf" into `u64::MAX` and "nan" into `0` as a compress target.
    #[test]
    fn parse_size_rejects_non_finite_values() {
        assert!(parse_size("inf").is_err());
        assert!(parse_size("Infinity").is_err());
        assert!(parse_size("-inf").is_err());
        assert!(parse_size("nan").is_err());
        assert!(parse_size("NaN").is_err());
        // Still accepts ordinary sizes.
        assert_eq!(parse_size("1MB"), Ok(1_000_000));
        assert_eq!(parse_size("500KB"), Ok(500_000));
    }
}
