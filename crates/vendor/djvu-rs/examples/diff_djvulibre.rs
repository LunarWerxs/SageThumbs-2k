//! Differential testing harness against DjVuLibre's `ddjvu` (#192).
//!
//! For every page in each input file, renders the page with djvu-rs and with
//! `ddjvu` (subprocess) at the same target size, then compares the two RGBA
//! pixmaps with a per-pixel tolerance.
//!
//! Output: one JSON line per page (jsonl) + a stderr summary.
//!
//! Usage:
//!     cargo run --release --features=cli --example diff_djvulibre -- \
//!         [--width N] [--tolerance T] [--max-pages M] <file.djvu> [...]
//!
//!     --width N        target render width in px (default 1024). Height is
//!                      derived from page aspect ratio.
//!     --tolerance T    max allowed |Δ| per channel before a pixel counts
//!                      as a mismatch (default 4 — accommodates the small
//!                      rounding differences in IW44 wavelet reconstruction
//!                      between the two implementations).
//!     --max-pages M    cap pages compared per file (default 0 = unlimited).
//!
//! Notes:
//!  * `ddjvu` must be on `$PATH`. Brew: `brew install djvulibre`.
//!  * GPL/MIT cleanliness: the GPL `ddjvu` is invoked as an out-of-process
//!    binary; no GPL code is linked into this crate.
//!  * This is **Option B** from #192 — slow but trivially correct. CI-grade
//!    differential fuzzing (Option C: pre-rendered reference corpus + libfuzzer
//!    target) is a separate piece of work tracked on the issue.

// Names the deprecated, now-ignored `RenderOptions::scale` field (#377); allowed
// so this harness keeps building while the back-compat field exists.
#![allow(deprecated)]

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitCode};

use djvu_rs::{
    DjVuDocument, Pixmap,
    djvu_render::{RenderOptions, render_pixmap},
};

#[derive(Clone, Copy, Debug, Default)]
struct PageDiff {
    page: usize,
    width: u32,
    height: u32,
    total_px: u64,
    mismatched_px: u64,
    max_abs_diff: u8,
    mean_abs_diff: f64,
}

fn parse_ppm(data: &[u8]) -> Option<(u32, u32, &[u8])> {
    // Minimal P6 PPM parser (binary). Header: "P6\n<W> <H>\n<MAX>\n<bytes>"
    // Tolerates extra whitespace and `# ...` comments per the format spec.
    fn skip_ws_and_comments(d: &[u8], i: &mut usize) {
        while *i < d.len() {
            match d[*i] {
                b' ' | b'\t' | b'\n' | b'\r' => *i += 1,
                b'#' => {
                    while *i < d.len() && d[*i] != b'\n' {
                        *i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn read_token<'a>(d: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
        skip_ws_and_comments(d, i);
        let start = *i;
        while *i < d.len() && !matches!(d[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
        if *i == start {
            None
        } else {
            Some(&d[start..*i])
        }
    }

    if data.len() < 2 || &data[..2] != b"P6" {
        return None;
    }
    let mut i: usize = 2;
    let w: u32 = std::str::from_utf8(read_token(data, &mut i)?)
        .ok()?
        .parse()
        .ok()?;
    let h: u32 = std::str::from_utf8(read_token(data, &mut i)?)
        .ok()?
        .parse()
        .ok()?;
    let max: u32 = std::str::from_utf8(read_token(data, &mut i)?)
        .ok()?
        .parse()
        .ok()?;
    if max != 255 {
        return None;
    }
    // Exactly one whitespace byte separates the maxval from the binary data.
    if i >= data.len() {
        return None;
    }
    i += 1;
    let needed = (w as usize) * (h as usize) * 3;
    if data.len() < i + needed {
        return None;
    }
    Some((w, h, &data[i..i + needed]))
}

/// Compare a djvu-rs RGBA pixmap with ddjvu's RGB PPM bytes at matching size.
fn compare(rs: &Pixmap, libre_w: u32, libre_h: u32, libre_rgb: &[u8], tolerance: u8) -> PageDiff {
    assert_eq!(rs.width, libre_w);
    assert_eq!(rs.height, libre_h);
    let n = (rs.width as u64) * (rs.height as u64);
    let mut mismatched = 0u64;
    let mut max_abs = 0u8;
    let mut sum_abs: u64 = 0;
    for i in 0..(n as usize) {
        let r = rs.data[i * 4];
        let g = rs.data[i * 4 + 1];
        let b = rs.data[i * 4 + 2];
        let lr = libre_rgb[i * 3];
        let lg = libre_rgb[i * 3 + 1];
        let lb = libre_rgb[i * 3 + 2];
        let dr = r.abs_diff(lr);
        let dg = g.abs_diff(lg);
        let db = b.abs_diff(lb);
        let m = dr.max(dg).max(db);
        if m > tolerance {
            mismatched += 1;
        }
        if m > max_abs {
            max_abs = m;
        }
        sum_abs += dr as u64 + dg as u64 + db as u64;
    }
    PageDiff {
        page: 0,
        width: rs.width,
        height: rs.height,
        total_px: n,
        mismatched_px: mismatched,
        max_abs_diff: max_abs,
        mean_abs_diff: sum_abs as f64 / (n as f64 * 3.0),
    }
}

fn render_ddjvu(path: &Path, page_idx: usize, w: u32, h: u32) -> Option<(u32, u32, Vec<u8>)> {
    let out = tempfile::NamedTempFile::new().ok()?;
    let out_path = out.path().to_path_buf();
    let status = Command::new("ddjvu")
        .arg("-format=ppm")
        .arg(format!("-page={}", page_idx + 1))
        .arg(format!("-size={w}x{h}"))
        .arg("-aspect=no")
        .arg(path)
        .arg(&out_path)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut data = Vec::new();
    std::fs::File::open(&out_path)
        .ok()?
        .read_to_end(&mut data)
        .ok()?;
    let (pw, ph, rgb) = parse_ppm(&data)?;
    Some((pw, ph, rgb.to_vec()))
}

fn process_file(path: &Path, target_w: u32, tolerance: u8, max_pages: usize) -> Vec<PageDiff> {
    let bytes = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: {}", path.display(), e);
            return vec![];
        }
    };
    let doc = match DjVuDocument::parse(&bytes) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: {}", path.display(), e);
            return vec![];
        }
    };
    let pc = doc.page_count();
    let cap = if max_pages == 0 {
        pc
    } else {
        pc.min(max_pages)
    };

    let mut out = Vec::new();
    for page_idx in 0..cap {
        let page = match doc.page(page_idx) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let pw = page.width() as u32;
        let ph = page.height() as u32;
        if pw == 0 || ph == 0 {
            continue;
        }
        let render_w = target_w.min(pw);
        let render_h = ((ph as u64 * render_w as u64) / pw as u64).max(1) as u32;

        let opts = RenderOptions {
            width: render_w,
            height: render_h,
            scale: render_w as f32 / pw as f32,
            ..Default::default()
        };
        let rs_pix = match render_pixmap(page, &opts) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "djvu-rs render failed at {}#p{page_idx}: {e}",
                    path.display()
                );
                continue;
            }
        };

        let (libre_w, libre_h, libre_rgb) = match render_ddjvu(path, page_idx, render_w, render_h) {
            Some(t) => t,
            None => {
                eprintln!("ddjvu render failed at {}#p{page_idx}", path.display());
                continue;
            }
        };
        if libre_w != rs_pix.width || libre_h != rs_pix.height {
            eprintln!(
                "size mismatch at {}#p{page_idx}: rs {}x{} vs libre {}x{}",
                path.display(),
                rs_pix.width,
                rs_pix.height,
                libre_w,
                libre_h
            );
            continue;
        }

        let mut d = compare(&rs_pix, libre_w, libre_h, &libre_rgb, tolerance);
        d.page = page_idx;
        out.push(d);
    }
    out
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut target_w: u32 = 1024;
    let mut tolerance: u8 = 4;
    let mut max_pages: usize = 0;
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--width" => {
                i += 1;
                target_w = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1024);
            }
            "--tolerance" => {
                i += 1;
                tolerance = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(4);
            }
            "--max-pages" => {
                i += 1;
                max_pages = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: diff_djvulibre [--width N] [--tolerance T] [--max-pages M] <file.djvu> [...]"
                );
                return ExitCode::from(2);
            }
            other => files.push(other.to_string()),
        }
        i += 1;
    }
    if files.is_empty() {
        eprintln!(
            "usage: diff_djvulibre [--width N] [--tolerance T] [--max-pages M] <file.djvu> [...]"
        );
        return ExitCode::from(2);
    }

    let mut total_pages = 0u64;
    let mut total_px = 0u128;
    let mut total_mismatched = 0u128;
    let mut max_abs_global = 0u8;
    let mut max_mismatch_pct: f64 = 0.0;
    let mut worst_page: Option<(String, usize, f64)> = None;

    for arg in &files {
        let path = Path::new(arg);
        for d in process_file(path, target_w, tolerance, max_pages) {
            let pct = (d.mismatched_px as f64) / (d.total_px as f64) * 100.0;
            println!(
                "{{\"file\":\"{}\",\"page\":{},\"width\":{},\"height\":{},\
                 \"total_px\":{},\"mismatched_px\":{},\"mismatch_pct\":{:.4},\
                 \"max_abs_diff\":{},\"mean_abs_diff\":{:.3}}}",
                path.display(),
                d.page,
                d.width,
                d.height,
                d.total_px,
                d.mismatched_px,
                pct,
                d.max_abs_diff,
                d.mean_abs_diff
            );
            total_pages += 1;
            total_px += d.total_px as u128;
            total_mismatched += d.mismatched_px as u128;
            if d.max_abs_diff > max_abs_global {
                max_abs_global = d.max_abs_diff;
            }
            if pct > max_mismatch_pct {
                max_mismatch_pct = pct;
                worst_page = Some((arg.clone(), d.page, pct));
            }
        }
    }

    if total_pages == 0 {
        eprintln!("no pages compared");
        return ExitCode::from(1);
    }

    eprintln!();
    eprintln!("=== diff vs DjVuLibre ddjvu ===");
    eprintln!("pages compared:       {total_pages}");
    eprintln!("total pixels:         {total_px}");
    eprintln!(
        "mismatched (>tol={}): {total_mismatched}  ({:.4}% of total)",
        tolerance,
        (total_mismatched as f64) / (total_px as f64) * 100.0
    );
    eprintln!("max |Δ| any channel:  {max_abs_global}");
    if let Some((f, p, pct)) = worst_page {
        eprintln!("worst page:           {f} #p{p} ({pct:.4}% mismatched)");
    }

    ExitCode::SUCCESS
}
