//! IW44 encoder quality benchmark — re-encodes the BG44 background of every
//! page in the supplied DjVu file(s), then measures PSNR (luminance) of the
//! re-decoded pixmap against djvu-rs's own decode of the original BG44.
//!
//! Output: one JSON line per page (jsonl), plus a final summary on stderr.
//!
//! Usage:
//!     cargo run --release --features=std --example encode_quality_iw44 -- <file.djvu> [...]

use std::path::Path;
use std::process::ExitCode;

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color},
};

#[derive(Clone, Copy, Debug)]
struct PageResult {
    page: usize,
    width: u32,
    height: u32,
    orig_bg44_bytes: usize,
    rs_bg44_bytes: usize,
    psnr_db: f64,
}

/// PSNR of `b` against reference `a` over a luminance channel.
/// Uses ITU-R BT.601 luma coefficients on RGBA8 pixmaps.
fn psnr_rgba(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    let n = (a.width as usize) * (a.height as usize);
    let mut mse = 0.0f64;
    for i in 0..n {
        let off = i * 4;
        let ay = 0.299 * a.data[off] as f64
            + 0.587 * a.data[off + 1] as f64
            + 0.114 * a.data[off + 2] as f64;
        let by = 0.299 * b.data[off] as f64
            + 0.587 * b.data[off + 1] as f64
            + 0.114 * b.data[off + 2] as f64;
        let d = ay - by;
        mse += d * d;
    }
    mse /= n as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// Decode an entire BG44 chunk stream into a Pixmap.
fn decode_bg44_chunks(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

/// Same, for `Vec<Vec<u8>>` (encoder output).
fn decode_bg44_owned(chunks: &[Vec<u8>]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

fn process_file(path: &Path) -> Vec<PageResult> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: {}", path.display(), e);
            return vec![];
        }
    };
    let doc = match DjVuDocument::parse(&data) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: parse failed: {}", path.display(), e);
            return vec![];
        }
    };

    let mut out = Vec::new();
    for page_idx in 0..doc.page_count() {
        let page = match doc.page(page_idx) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let bg44_chunks: Vec<&[u8]> = page.bg44_chunks();
        if bg44_chunks.is_empty() {
            continue;
        }

        let orig_bytes: usize = bg44_chunks.iter().map(|d| d.len()).sum();
        let orig_pixmap = match decode_bg44_chunks(&bg44_chunks) {
            Some(p) => p,
            None => continue,
        };

        // Re-encode with the same chunking strategy as DjVuLibre's c44 default
        // (10 slices/chunk × 10 chunks = 100 slices total).
        let opts = Iw44EncodeOptions::default();
        let rs_chunks = encode_iw44_color(&orig_pixmap, &opts);
        let rs_bytes: usize = rs_chunks.iter().map(|v| v.len()).sum();

        let rs_pixmap = match decode_bg44_owned(&rs_chunks) {
            Some(p) => p,
            None => continue,
        };
        let psnr = psnr_rgba(&orig_pixmap, &rs_pixmap);

        out.push(PageResult {
            page: page_idx,
            width: orig_pixmap.width,
            height: orig_pixmap.height,
            orig_bg44_bytes: orig_bytes,
            rs_bg44_bytes: rs_bytes,
            psnr_db: psnr,
        });
    }
    out
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: encode_quality_iw44 <file.djvu> [<file2.djvu> ...]\n\
             \n\
             Re-encodes every BG44 background and measures PSNR vs djvu-rs's\n\
             decode of the original BG44 (the encoder is the unit under test;\n\
             the original decode is the reference)."
        );
        return ExitCode::from(2);
    }

    let mut total_orig = 0usize;
    let mut total_rs = 0usize;
    let mut total_pages = 0usize;
    let mut psnr_sum = 0.0f64;
    let mut psnr_min = f64::INFINITY;

    for arg in &args {
        let path = Path::new(arg);
        for r in process_file(path) {
            println!(
                "{{\"file\":\"{}\",\"page\":{},\"width\":{},\"height\":{},\
                 \"orig_bg44_bytes\":{},\"rs_bg44_bytes\":{},\"psnr_db\":{:.3}}}",
                path.display(),
                r.page,
                r.width,
                r.height,
                r.orig_bg44_bytes,
                r.rs_bg44_bytes,
                r.psnr_db
            );
            total_orig += r.orig_bg44_bytes;
            total_rs += r.rs_bg44_bytes;
            total_pages += 1;
            if r.psnr_db.is_finite() {
                psnr_sum += r.psnr_db;
                if r.psnr_db < psnr_min {
                    psnr_min = r.psnr_db;
                }
            }
        }
    }

    if total_pages == 0 {
        eprintln!("no IW44/BG44 pages processed");
        return ExitCode::from(1);
    }

    eprintln!();
    eprintln!("=== IW44 encoder quality summary ===");
    eprintln!("pages:          {}", total_pages);
    eprintln!("orig BG44 size: {:>10} bytes", total_orig);
    eprintln!(
        "rs   BG44 size: {:>10} bytes  ({:.3}× orig)",
        total_rs,
        total_rs as f64 / total_orig.max(1) as f64
    );
    eprintln!(
        "PSNR (luma):    avg {:.2} dB,  min {:.2} dB",
        psnr_sum / total_pages as f64,
        psnr_min
    );

    ExitCode::SUCCESS
}
