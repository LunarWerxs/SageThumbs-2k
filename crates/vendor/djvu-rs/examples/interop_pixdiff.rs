//! DjVuLibre interop pixel-diff harness (round-5 #14).
//!
//! Renders a page with our decoder and with DjVuLibre's `ddjvu` at the same
//! native resolution, then reports the per-pixel RGB difference distribution.
//! This is the quantitative "quality floor" for future render experiments: a
//! change that claims to improve quality can be judged against the reference,
//! and a change that must stay faithful can prove it did not drift.
//!
//! Not a byte-identity check — our chroma upsampling (#422 bilinear) and
//! anti-aliasing are deliberately different from (often better than) DjVuLibre,
//! so small diffs are expected. The harness surfaces the *distribution* so
//! regressions (a new large tail) are visible against the established baseline.
//!
//! Usage:
//!   cargo run --release --example interop_pixdiff -- <file.djvu> [page]
//!   cargo run --release --example interop_pixdiff -- --corpus   # sweep a set
//!
//! Requires `ddjvu` on PATH (DjVuLibre).
#![allow(deprecated)]

#[path = "support/mod.rs"]
mod support;

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::{DiffStats, diff_stats, native_opts, parse_ppm};

fn compare_page(path: &Path, page_no: usize) -> Result<DiffStats, String> {
    // Reference render via ddjvu → PPM.
    let out = std::env::temp_dir().join(format!(
        "interop_ref_{}_{}.ppm",
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("x"),
        page_no
    ));
    let status = Command::new("ddjvu")
        .args([
            "-format=ppm",
            &format!("-page={page_no}"),
            &path.to_string_lossy(),
            &out.to_string_lossy(),
        ])
        .status()
        .map_err(|e| format!("failed to run ddjvu: {e}"))?;
    if !status.success() {
        return Err("ddjvu returned non-zero".into());
    }
    let ref_bytes = std::fs::read(&out).map_err(|e| format!("read ppm: {e}"))?;
    let _ = std::fs::remove_file(&out);
    let (rw, rh, ref_rgb) = parse_ppm(&ref_bytes).ok_or("could not parse ddjvu PPM")?;

    // Our render at the same native resolution.
    let data = std::fs::read(path).map_err(|e| format!("read djvu: {e}"))?;
    let doc = DjVuDocument::parse(&data).map_err(|e| format!("parse: {e}"))?;
    let page = doc
        .page(page_no - 1)
        .map_err(|e| format!("page {page_no}: {e}"))?;
    let (pw, ph) = (page.width() as usize, page.height() as usize);
    if (pw, ph) != (rw, rh) {
        return Err(format!(
            "dimension mismatch: ours {pw}x{ph} vs ddjvu {rw}x{rh}"
        ));
    }
    let pm = djvu_render::render_pixmap(page, &native_opts(pw as u32, ph as u32))
        .map_err(|e| format!("render: {e}"))?;
    Ok(diff_stats(rw, rh, &pm.data, &ref_rgb))
}

fn print_row(label: &str, s: &DiffStats) {
    println!(
        "{label:28} {}x{:<5}  mean={:5.2}  p50={:3} p95={:3} p99={:3} max={:3}  >2={:5.2}% >8={:5.2}% >32={:5.2}%",
        s.w, s.h, s.mean_abs, s.p50, s.p95, s.p99, s.max_abs, s.pct_gt2, s.pct_gt8, s.pct_gt32
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    if args.first().map(String::as_str) == Some("--corpus") {
        // A representative spread: bilevel, colour, palette, large.
        let files = [
            // Tier 1
            ("tests/corpus/watchmaker.djvu", 1),
            ("tests/corpus/cable_1973_100133.djvu", 1),
            ("references/djvujs/library/assets/colorbook.djvu", 1),
            ("references/djvujs/library/assets/navm_fgbz.djvu", 1),
            ("references/djvujs/library/assets/boy.djvu", 1),
            ("references/djvujs/library/assets/carte.djvu", 1),
            // Tier 2 (#558): newspaper, mixed layout, map, CJK, Cyrillic, photo
            ("tests/corpus/war_1812.djvu", 1),
            ("tests/corpus/war_1812.djvu", 3),
            ("tests/corpus/goody_twoshoes.djvu", 1),
            ("tests/corpus/map_atlas_sample.djvu", 1),
            ("tests/corpus/chinese_cookbook_sample.djvu", 2),
            ("tests/corpus/cyrillic_simonovich_co2.djvu", 1),
            ("tests/corpus/big_scanned_page.djvu", 1),
        ];
        println!(
            "{:28} {:11}  {:11}  {:23} tail (% channels over)",
            "file", "dims", "mean-abs", "percentiles"
        );
        for (rel, page) in files {
            let path = manifest.join(rel);
            if !path.exists() {
                continue;
            }
            match compare_page(&path, page) {
                Ok(s) => {
                    let label = format!(
                        "{}#{}",
                        path.file_name().and_then(|s| s.to_str()).unwrap_or(rel),
                        page
                    );
                    print_row(&label, &s);
                }
                Err(e) => println!("{rel:28} page={page} SKIP: {e}"),
            }
        }
        return;
    }

    let Some(file) = args.first() else {
        eprintln!("usage: interop_pixdiff <file.djvu> [page] | --corpus");
        std::process::exit(2);
    };
    let page_no: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    match compare_page(Path::new(file), page_no) {
        Ok(s) => print_row(file, &s),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
