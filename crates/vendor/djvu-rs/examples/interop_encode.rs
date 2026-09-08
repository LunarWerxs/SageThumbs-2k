//! Encoder-side DjVuLibre interop harness.
//!
//! The existing interop tools (`interop_pixdiff`, `diff_djvulibre`) are
//! *decode-side*: they take an existing `.djvu` and compare **our** render against
//! `ddjvu`'s. Neither checks whether a file **we encode** is decodable by
//! DjVuLibre at all. That is exactly the gate the normative IW44 masked-wavelet
//! work needs — "a decoder that never sees the mask must still reconstruct a valid
//! stream" — and it is a real hazard: our `chroma_half` mode, for one, produces a
//! stream `ddjvu` rejects with *Unexpected End Of File*.
//!
//! For each source page this harness:
//!   1. renders it to a source pixmap (our decoder),
//!   2. re-encodes that pixmap with **our** colour `Quality` encoder → `ours.djvu`,
//!   3. decodes `ours.djvu` with **`ddjvu`** (DjVuLibre) → PPM,   ← the interop gate
//!   4. decodes `ours.djvu` with **our** decoder → pixmap,
//!   5. reports: did `ddjvu` accept our file, do the dimensions agree, and the
//!      per-channel pixel-diff distribution of (ddjvu-of-ours vs us-of-ours) and
//!      (ddjvu-of-ours vs the original source).
//!
//! The `ddjvu-of-ours vs us-of-ours` diff is the interop-fidelity number: both
//! decode the *same* bytes, so a large diff means our encoder emits a stream the
//! two decoders read differently — a latent interop bug. The `vs source` diff is
//! the end-to-end encode quality as DjVuLibre sees it.
//!
//! Exit code is non-zero if any page fails the gate (ddjvu rejects the file or
//! dimensions disagree), so this doubles as a pass/fail interop check.
//!
//! Usage:
//!   cargo run --release --features cli --example interop_encode -- <file.djvu> [file.djvu ...]
//!   cargo run --release --features cli --example interop_encode -- --corpus
//!
//! Requires `ddjvu` on PATH (DjVuLibre: `brew install djvulibre`).
#![allow(deprecated)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

/// Parse a binary PPM (P6) → (width, height, rgb).
fn parse_ppm(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    if data.get(0..2)? != b"P6" {
        return None;
    }
    let mut pos = 2usize;
    let mut nums = [0usize; 3];
    for slot in nums.iter_mut() {
        loop {
            let b = *data.get(pos)?;
            if b == b'#' {
                while *data.get(pos)? != b'\n' {
                    pos += 1;
                }
            } else if b.is_ascii_whitespace() {
                pos += 1;
            } else {
                break;
            }
        }
        let mut v = 0usize;
        while let Some(&b) = data.get(pos) {
            if b.is_ascii_digit() {
                v = v * 10 + (b - b'0') as usize;
                pos += 1;
            } else {
                break;
            }
        }
        *slot = v;
    }
    pos += 1;
    let (w, h, maxval) = (nums[0], nums[1], nums[2]);
    if maxval != 255 {
        return None;
    }
    let rgb = data.get(pos..pos + w * h * 3)?.to_vec();
    Some((w, h, rgb))
}

struct Diff {
    mean_abs: f64,
    max_abs: u8,
    p99: u8,
    pct_gt8: f64,
}

/// Per-channel |Δ| distribution between an RGBA pixmap and a reference RGB buffer.
fn diff(w: usize, h: usize, ours_rgba: &[u8], ref_rgb: &[u8]) -> Diff {
    let mut hist = [0u64; 256];
    let mut sum = 0u64;
    let mut max_abs = 0u8;
    for p in 0..w * h {
        for c in 0..3 {
            let d = ours_rgba[p * 4 + c].abs_diff(ref_rgb[p * 3 + c]);
            hist[d as usize] += 1;
            sum += d as u64;
            if d > max_abs {
                max_abs = d;
            }
        }
    }
    let total = (w * h * 3) as u64;
    let pctile = |frac: f64| -> u8 {
        let target = (frac * total as f64) as u64;
        let mut acc = 0u64;
        for (d, &c) in hist.iter().enumerate() {
            acc += c;
            if acc >= target {
                return d as u8;
            }
        }
        255
    };
    let gt8: u64 = hist[9..].iter().sum();
    Diff {
        mean_abs: sum as f64 / total as f64,
        max_abs,
        p99: pctile(0.99),
        pct_gt8: 100.0 * gt8 as f64 / total as f64,
    }
}

fn native_opts(page: &djvu_rs::djvu_document::DjVuPage) -> RenderOptions {
    RenderOptions::fit_to_width(page, page.width() as u32)
}

/// Render page 0 of a `.djvu` to a pixmap with our decoder.
fn render_source(path: &Path) -> Result<Pixmap, String> {
    let data = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let doc = DjVuDocument::parse(&data).map_err(|e| format!("parse: {e}"))?;
    let page = doc.page(0).map_err(|e| format!("page 0: {e}"))?;
    render_pixmap(page, &native_opts(page)).map_err(|e| format!("render: {e}"))
}

struct Report {
    name: String,
    encoded_bytes: usize,
    ddjvu_ok: bool,
    ddjvu_stderr: String,
    dims_ok: bool,
    /// ddjvu-of-ours vs us-of-ours — interop fidelity (both decode the same bytes).
    interop: Option<Diff>,
    /// ddjvu-of-ours vs the original source — end-to-end encode quality.
    quality: Option<Diff>,
}

fn check_one(path: &Path, tmp: &Path) -> Result<Report, String> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("x")
        .to_string();

    // 1. source pixmap.
    let src = render_source(path)?;

    // 2. re-encode with our colour Quality encoder.
    let encoded = PageEncoder::from_pixmap(&src)
        .with_quality(EncodeQuality::Quality)
        .encode()
        .map_err(|e| format!("encode: {e:?}"))?;
    let our_djvu = tmp.join(format!("interop_enc_{name}.djvu"));
    std::fs::write(&our_djvu, &encoded).map_err(|e| format!("write djvu: {e}"))?;

    // 3. decode OUR file with ddjvu — the interop gate.
    let out_ppm = tmp.join(format!("interop_enc_{name}.ppm"));
    let in_arg = our_djvu.to_string_lossy();
    let out_arg = out_ppm.to_string_lossy();
    let output = Command::new("ddjvu")
        .args(["-format=ppm", in_arg.as_ref(), out_arg.as_ref()])
        .output()
        .map_err(|e| format!("spawn ddjvu: {e}"))?;
    let ddjvu_ok = output.status.success() && out_ppm.exists();
    let ddjvu_stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    let mut report = Report {
        name,
        encoded_bytes: encoded.len(),
        ddjvu_ok,
        ddjvu_stderr,
        dims_ok: false,
        interop: None,
        quality: None,
    };
    if !ddjvu_ok {
        let _ = std::fs::remove_file(&our_djvu);
        return Ok(report);
    }

    // 4. our decode of our file.
    let ours = render_source(&our_djvu)?;

    // 5. diffs.
    let ref_bytes = std::fs::read(&out_ppm).map_err(|e| format!("read ppm: {e}"))?;
    let _ = std::fs::remove_file(&out_ppm);
    let _ = std::fs::remove_file(&our_djvu);
    let (rw, rh, ref_rgb) = parse_ppm(&ref_bytes).ok_or("parse ddjvu ppm")?;

    report.dims_ok = (rw, rh) == (ours.width as usize, ours.height as usize)
        && (rw, rh) == (src.width as usize, src.height as usize);
    if report.dims_ok {
        report.interop = Some(diff(rw, rh, &ours.data, &ref_rgb));
        report.quality = Some(diff(rw, rh, &src.data, &ref_rgb));
    }
    Ok(report)
}

fn corpus_files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for dir in ["tests/fixtures", "tests/corpus"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("djvu") {
                    v.push(p);
                }
            }
        }
    }
    v.sort();
    v
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: Vec<PathBuf> = if args.iter().any(|a| a == "--corpus") {
        corpus_files()
    } else if args.is_empty() {
        eprintln!("usage: interop_encode <file.djvu ...> | --corpus");
        return ExitCode::FAILURE;
    } else {
        args.iter().map(PathBuf::from).collect()
    };

    let tmp = std::env::temp_dir();
    let mut failures = 0usize;
    let mut checked = 0usize;
    println!(
        "{:<26} {:>9}  {:<6} {:<24} {:<26}",
        "page", "enc B", "ddjvu", "interop(ddjvu vs us)", "quality(ddjvu vs src)"
    );
    for f in &files {
        match check_one(f, &tmp) {
            Ok(r) => {
                checked += 1;
                let gate = r.ddjvu_ok && r.dims_ok;
                if !gate {
                    failures += 1;
                }
                let interop = r
                    .interop
                    .as_ref()
                    .map(|d| format!("mean{:.3} p99={} max{}", d.mean_abs, d.p99, d.max_abs))
                    .unwrap_or_else(|| "-".into());
                let quality = r
                    .quality
                    .as_ref()
                    .map(|d| format!("mean{:.2} >8:{:.1}%", d.mean_abs, d.pct_gt8))
                    .unwrap_or_else(|| "-".into());
                let status = if !r.ddjvu_ok {
                    format!("REJECT ({})", r.ddjvu_stderr.lines().next().unwrap_or(""))
                } else if !r.dims_ok {
                    "DIMS!".into()
                } else {
                    "ok".into()
                };
                println!(
                    "{:<26} {:>9} {:<6} {:<24} {:<26}",
                    r.name, r.encoded_bytes, status, interop, quality
                );
            }
            Err(e) => {
                eprintln!("{}: skip ({e})", f.display());
            }
        }
    }
    eprintln!("\n{checked} checked, {failures} failed the interop gate");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
