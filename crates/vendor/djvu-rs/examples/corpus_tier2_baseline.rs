//! Tier-2 corpus baseline (#558): bytes, ddjvu PSNR/SSIM, render wall time.
//!
//! ```text
//! cargo run --release --example corpus_tier2_baseline
//! ```
//!
//! Requires `ddjvu` on PATH for the interop columns. Files absent from
//! `tests/corpus/` are skipped. Renders at native page size (same contract as
//! `interop_pixdiff`); the photo page is timed at 1/4 linear for wall-clock
//! only, with a separate native interop row.
#![allow(deprecated)]

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{self, RenderOptions, Resampling, UserRotation};
use djvu_rs::quality;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

struct Entry {
    rel: &'static str,
    class: &'static str,
    page: usize, // 0-based for our API; ddjvu uses 1-based
    /// If set, time a reduced render; interop still uses native dims.
    time_div: u32,
}

const TIER2: &[Entry] = &[
    Entry {
        rel: "tests/corpus/war_1812.djvu",
        class: "newspaper",
        page: 0,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/war_1812.djvu",
        class: "newspaper-text",
        page: 2,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/goody_twoshoes.djvu",
        class: "mixed-layout",
        page: 0,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/map_atlas_sample.djvu",
        class: "map/line-art",
        page: 0,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/chinese_cookbook_sample.djvu",
        class: "cjk",
        page: 1,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/cyrillic_simonovich_co2.djvu",
        class: "cyrillic",
        page: 0,
        time_div: 1,
    },
    Entry {
        rel: "tests/corpus/big_scanned_page.djvu",
        class: "photo-maskless",
        page: 0,
        time_div: 4,
    },
];

fn opts(w: u32, h: u32) -> RenderOptions {
    RenderOptions {
        width: w,
        height: h,
        scale: 1.0,
        bold: 0,
        aa: false,
        rotation: UserRotation::None,
        permissive: false,
        resampling: Resampling::Bilinear,
        mask_aa: false,
    }
}

fn ppm_to_pixmap(data: &[u8]) -> Option<Pixmap> {
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
                pos += 1;
            } else if b.is_ascii_whitespace() {
                pos += 1;
            } else {
                break;
            }
        }
        let start = pos;
        while data.get(pos).map(|b| b.is_ascii_digit()).unwrap_or(false) {
            pos += 1;
        }
        *slot = std::str::from_utf8(&data[start..pos]).ok()?.parse().ok()?;
    }
    while data
        .get(pos)
        .map(|b| b.is_ascii_whitespace())
        .unwrap_or(false)
    {
        pos += 1;
    }
    let (w, h) = (nums[0], nums[1]);
    let need = w.checked_mul(h)?.checked_mul(3)?;
    let rgb = data.get(pos..pos + need)?;
    let mut pm = Pixmap::new(w as u32, h as u32, 0, 0, 0, 255);
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        pm.data[i * 4] = px[0];
        pm.data[i * 4 + 1] = px[1];
        pm.data[i * 4 + 2] = px[2];
        pm.data[i * 4 + 3] = 255;
    }
    Some(pm)
}

fn ddjvu_native(path: &Path, page_1based: usize) -> Result<Pixmap, String> {
    let out = std::env::temp_dir().join(format!(
        "tier2_ref_{}_{}.ppm",
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("x"),
        page_1based
    ));
    let status = Command::new("ddjvu")
        .args([
            "-format=ppm",
            &format!("-page={page_1based}"),
            &path.to_string_lossy(),
            &out.to_string_lossy(),
        ])
        .status()
        .map_err(|e| format!("ddjvu spawn: {e}"))?;
    if !status.success() {
        return Err(format!("ddjvu exit {status}"));
    }
    let bytes = std::fs::read(&out).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&out);
    ppm_to_pixmap(&bytes).ok_or_else(|| "ppm parse failed".to_string())
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!(
        "{:<28} {:<16} {:>8} {:>10} {:>8} {:>8} {:>10}",
        "file", "class", "pages", "bytes", "psnr", "ssim", "render_ms"
    );
    for entry in TIER2 {
        let path = root.join(entry.rel);
        if !path.exists() {
            println!("{:<28} {:<16} SKIP (missing)", entry.rel, entry.class);
            continue;
        }
        let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                println!("{:<28} {:<16} SKIP read: {e}", entry.rel, entry.class);
                continue;
            }
        };
        let doc = match DjVuDocument::parse(&data) {
            Ok(d) => d,
            Err(e) => {
                println!("{:<28} {:<16} SKIP parse: {e}", entry.rel, entry.class);
                continue;
            }
        };
        let page = match doc.page(entry.page) {
            Ok(p) => p,
            Err(e) => {
                println!("{:<28} {:<16} SKIP page: {e}", entry.rel, entry.class);
                continue;
            }
        };
        let (w, h) = (page.width() as u32, page.height() as u32);
        let (tw, th) = (w / entry.time_div, h / entry.time_div);

        let t0 = Instant::now();
        let timed = match djvu_render::render_pixmap(page, &opts(tw, th)) {
            Ok(p) => p,
            Err(e) => {
                println!("{:<28} {:<16} SKIP render: {e}", entry.rel, entry.class);
                continue;
            }
        };
        let render_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let _ = timed;

        let (psnr_s, ssim_s) = match ddjvu_native(&path, entry.page + 1) {
            Ok(reference) => {
                let (rw, rh) = (reference.width, reference.height);
                if (rw, rh) != (w, h) {
                    (format!("dim {rw}x{rh}"), format!("vs {w}x{h}"))
                } else {
                    let ours = match djvu_render::render_pixmap(page, &opts(w, h)) {
                        Ok(p) => p,
                        Err(e) => {
                            println!("{:<28} {:<16} SKIP native: {e}", entry.rel, entry.class);
                            continue;
                        }
                    };
                    let c = quality::compare(&ours, &reference);
                    let psnr = if c.psnr_db.is_finite() {
                        format!("{:.2}", c.psnr_db)
                    } else {
                        "inf".to_string()
                    };
                    (psnr, format!("{:.5}", c.ssim))
                }
            }
            Err(e) => ("n/a".to_string(), format!("({e})")),
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(entry.rel);
        let label = if entry.time_div > 1 {
            format!("{name}/t{}", entry.time_div)
        } else {
            name.to_string()
        };
        println!(
            "{label:<28} {:<16} {:>8} {:>10} {:>8} {:>8} {:>10.1}",
            entry.class,
            doc.page_count(),
            file_bytes,
            psnr_s,
            ssim_s,
            render_ms
        );
    }
}
