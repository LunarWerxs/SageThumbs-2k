//! IW44_ENTROPY_PROBE (round 51) — chunking-lever byte sweep on real BG44
//! production content.
//!
//! Companion to `iw44_chunking_lever` (which validates decodability/interop on
//! 3 full-page renders) and `iw44_band_probe` (which found the ours/c44 size
//! ratio is anti-correlated with page size, not concentrated in any band).
//! This sweeps `slices_per_chunk` directly via `encode_iw44_color` on every
//! page's *real* BG44 background (the same "segmented BG (production BG44
//! workload)" round 35/IW44_SLICE_RD measured, extracted from each corpus
//! file's existing BG44 chunk — not a synthetic full-detail render), so the
//! byte deltas below are representative of the real production BG44 path, not
//! just 3 sampled page-0 renders.
//!
//! Usage:
//!   cargo run --release --example iw44_chunking_sweep -- \
//!       tests/fixtures/colorbook.djvu tests/corpus/watchmaker.djvu tests/corpus/conquete_paix.djvu
use std::path::Path;

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color},
};

const CANDIDATES: [u8; 5] = [10, 25, 50, 74, 99];

fn decode_bg44(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

fn decode_bg44_owned(chunks: &[Vec<u8>]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: iw44_chunking_sweep <file.djvu> [<file2.djvu> ...]");
        return;
    }

    // Per-candidate running totals across every BG44 page in every input file.
    let mut totals = [0u64; CANDIDATES.len()];
    let mut baseline_pixels_match = true;
    let mut pages = 0usize;

    for arg in &args {
        let path = Path::new(arg);
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skip {}: {e}", path.display());
                continue;
            }
        };
        let doc = match DjVuDocument::parse(&data) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skip {}: parse failed: {e}", path.display());
                continue;
            }
        };
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");

        for page_idx in 0..doc.page_count() {
            let page = match doc.page(page_idx) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let bg44_chunks: Vec<&[u8]> = page.bg44_chunks();
            if bg44_chunks.is_empty() {
                continue;
            }
            let orig_pixmap = match decode_bg44(&bg44_chunks) {
                Some(p) => p,
                None => continue,
            };
            pages += 1;

            let mut baseline_rgba: Option<Vec<u8>> = None;
            let mut row = format!("{name:>16} p{page_idx:<4}");
            for (i, &n) in CANDIDATES.iter().enumerate() {
                let opts = Iw44EncodeOptions {
                    slices_per_chunk: n,
                    ..Default::default()
                };
                let chunks = encode_iw44_color(&orig_pixmap, &opts);
                let bytes: u64 = chunks.iter().map(|c| c.len() as u64).sum();
                totals[i] += bytes;

                // Pixel-identity check vs the n=10 baseline (chunking must be a
                // pure stream-framing change — decode and compare RGBA bytes).
                if let Some(decoded) = decode_bg44_owned(&chunks) {
                    match &baseline_rgba {
                        None => baseline_rgba = Some(decoded.data.clone()),
                        Some(base) => {
                            if *base != decoded.data {
                                baseline_pixels_match = false;
                                eprintln!(
                                    "MISMATCH: {name} p{page_idx} n={n} decodes to different pixels than n=10 baseline"
                                );
                            }
                        }
                    }
                }
                row.push_str(&format!(" {bytes:>8}"));
            }
            println!("{row}");
        }
    }

    println!(
        "\n{:>21} {}",
        "n/chunk:",
        CANDIDATES
            .iter()
            .map(|n| format!("{n:>8}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let baseline = totals[0] as f64;
    println!(
        "{:>21} {}",
        "totals (B):",
        totals
            .iter()
            .map(|t| format!("{t:>8}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "{:>21} {}",
        "Δ% vs n=10:",
        totals
            .iter()
            .map(|t| format!("{:>7.2}%", 100.0 * (*t as f64 - baseline) / baseline))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "\n{pages} BG44 pages processed. pixel-identity across all n values: {}",
        if baseline_pixels_match {
            "PASS (bit-identical)"
        } else {
            "FAIL — see MISMATCH lines above"
        }
    );
}
