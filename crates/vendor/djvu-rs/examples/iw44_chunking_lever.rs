//! IW44_ENTROPY_PROBE (round 51) — candidate lever: BG44 chunk-boundary layout.
//!
//! `iw44_band_probe` found the ours/c44 size ratio is strongly *anti-correlated*
//! with output size (small pages have the biggest gap: r = -0.59 across 62
//! colorbook pages) — not concentrated in any particular frequency band. `djvudump`
//! on a `c44`-encoded file shows why: `c44`'s default slice schedule is
//! `74+15+10` slices spread over **3** BG44 chunks, while our
//! `Iw44EncodeOptions::default()` is `slices_per_chunk=10` over **10** equal
//! chunks for the same 100-slice total. Each BG44 chunk is an independently
//! ZP-flushed sub-stream (`encode_chunks` starts a fresh `ZpEncoder` per chunk;
//! only the `PlaneEncoder`'s quantization/context state carries across chunk
//! boundaries) with a `min_zp_len = n + 4` byte floor and its own small
//! header/flush cost. More chunks → more paid floors/flushes, and on simple
//! content most of the trailing 10-slice sub-chunks are past quantization
//! saturation (all-null), paying pure floor/header cost for zero picture
//! content. Fewer, larger chunks should recover most of that.
//!
//! This is an encoder-only, decoder-unchanged lever: `encode_chunks`'s outer
//! loop already parameterizes chunk boundaries via `Iw44EncodeOptions::
//! slices_per_chunk`; chunking is pure stream framing, not part of the pixel
//! reconstruction, so pixel output must be bit-identical for every value
//! tested. This harness re-encodes each corpus page's full pixmap (matching
//! `interop_encode.rs`'s real end-to-end path) at several `slices_per_chunk`
//! values, and for every variant: checks `ddjvu` still accepts the file
//! (interop gate), checks the ddjvu-decoded pixels are identical to the
//! baseline's, and reports the byte delta.
//!
//! Usage:
//!   cargo run --release --features cli --example iw44_chunking_lever -- \
//!       tests/fixtures/colorbook.djvu tests/corpus/watchmaker.djvu
#![allow(deprecated)]

use std::path::{Path, PathBuf};
use std::process::Command;

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::iw44_encode::Iw44EncodeOptions;

/// Candidate `slices_per_chunk` values. 10 is the current default (baseline);
/// 99 is the max the option's documented range allows (single chunk for the
/// default `total_slices = 100`... 99 leaves one trailing 1-slice chunk, which
/// is intentional: it probes "almost all in one chunk" without changing the
/// documented `1..=99` range of the option).
const CANDIDATES: [u8; 5] = [10, 25, 50, 74, 99];

/// Parse a binary PPM (P6) → (width, height, rgb). Copied from interop_encode.rs.
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

fn native_opts(page: &djvu_rs::djvu_document::DjVuPage) -> RenderOptions {
    RenderOptions::fit_to_width(page, page.width() as u32)
}

fn render_source(path: &Path) -> Result<Pixmap, String> {
    let data = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let doc = DjVuDocument::parse(&data).map_err(|e| format!("parse: {e}"))?;
    let page = doc.page(0).map_err(|e| format!("page 0: {e}"))?;
    render_pixmap(page, &native_opts(page)).map_err(|e| format!("render: {e}"))
}

/// ddjvu-decode a `.djvu` file to raw RGB via a temp PPM. Returns `None` if
/// ddjvu rejects it or the PPM can't be parsed (the interop gate).
fn ddjvu_decode(path: &Path, tmp: &Path, tag: &str) -> Option<(usize, usize, Vec<u8>)> {
    let out_ppm = tmp.join(format!("iw44_chunk_lever_{tag}.ppm"));
    let output = Command::new("ddjvu")
        .args([
            "-format=ppm",
            path.to_string_lossy().as_ref(),
            out_ppm.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    if !output.status.success() || !out_ppm.exists() {
        return None;
    }
    let bytes = std::fs::read(&out_ppm).ok()?;
    let _ = std::fs::remove_file(&out_ppm);
    parse_ppm(&bytes)
}

fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

fn process_file(path: &Path, tmp: &Path) {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("x")
        .to_string();
    let src = match render_source(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip {}: {e}", path.display());
            return;
        }
    };

    println!("\n=== {name} ({}x{}) ===", src.width, src.height);
    println!(
        "{:>4} {:>10} {:>8} {:>6} {:>10} {:>10}",
        "n/ch", "bytes", "Δ%", "ddjvu", "chunks", "maxΔpx(vs n=10)"
    );

    let mut baseline_bytes: Option<usize> = None;
    let mut baseline_pixels: Option<(usize, usize, Vec<u8>)> = None;

    for &n in &CANDIDATES {
        let opts = Iw44EncodeOptions {
            slices_per_chunk: n,
            ..Default::default()
        };
        let encoded = match PageEncoder::from_pixmap(&src)
            .with_quality(EncodeQuality::Quality)
            .with_iw44_options(opts)
            .encode()
        {
            Ok(b) => b,
            Err(e) => {
                println!("{n:>4}   encode failed: {e:?}");
                continue;
            }
        };
        let bytes = encoded.len();
        let out_path = tmp.join(format!("iw44_chunk_lever_{name}_{n}.djvu"));
        if std::fs::write(&out_path, &encoded).is_err() {
            continue;
        }

        let decoded = ddjvu_decode(&out_path, tmp, &format!("{name}_{n}"));
        let _ = std::fs::remove_file(&out_path);
        let ddjvu_ok = decoded.is_some();

        let n_chunks = encoded_chunk_count(&encoded);

        let delta_pct = baseline_bytes
            .map(|b0| 100.0 * (bytes as f64 - b0 as f64) / b0 as f64)
            .unwrap_or(0.0);

        let px_diff = match (&decoded, &baseline_pixels) {
            (Some((_, _, rgb)), Some((_, _, base_rgb))) => max_abs_diff(rgb, base_rgb),
            _ => 0,
        };

        println!(
            "{n:>4} {bytes:>10} {delta_pct:>7.2}% {:>6} {n_chunks:>10} {px_diff:>15}",
            if ddjvu_ok { "ok" } else { "REJECT" },
        );

        if n == CANDIDATES[0] {
            baseline_bytes = Some(bytes);
            baseline_pixels = decoded;
        }
    }
}

/// Count top-level BG44 chunks in an encoded FORM:DJVU by scanning IFF chunk
/// tags (cheap textual scan — every leaf chunk is `TAG` + 4-byte BE length).
fn encoded_chunk_count(data: &[u8]) -> usize {
    let mut count = 0usize;
    // Skip "AT&T" magic (4) + "FORM" (4) + BE length (4) + "DJVU" subtype (4) = 16.
    let mut i = 16usize;
    while i + 8 <= data.len() {
        let tag = &data[i..i + 4];
        let len = u32::from_be_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
        if tag == b"BG44" {
            count += 1;
        }
        i += 8 + len + (len & 1); // chunks are even-padded
    }
    count
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: Vec<PathBuf> = if args.iter().any(|a| a == "--corpus") {
        corpus_files()
    } else if args.is_empty() {
        eprintln!("usage: iw44_chunking_lever <file.djvu ...> | --corpus");
        return;
    } else {
        args.iter().map(PathBuf::from).collect()
    };

    let tmp = std::env::temp_dir();
    for f in &files {
        process_file(f, &tmp);
    }
}
