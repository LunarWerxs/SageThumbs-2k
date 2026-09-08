//! IW44_ENTROPY_PROBE (round 51) — per-band bit/byte attribution for the IW44
//! encoder, on real BG44 production content.
//!
//! Round 22 (IW44_ENTROPY_GAP) found the size gap vs DjVuLibre's `c44` splits by
//! content: smooth backgrounds are at parity (or better), textured backgrounds
//! are a genuine ~1.3x at matched SSIM. It stopped at "the gap is entropy coding,
//! not masking" without localizing *where in the bitstream* the textured-content
//! bits concentrate. This harness answers that with real emitted bytes, using the
//! `iw44-probe` diagnostic feature (`djvu_iw44::encode::probe`): a thread-local,
//! purely-observational counter set that records, per one of the 10 IW44
//! frequency bands (0 = DC/coarsest .. 9 = finest):
//!
//!   - cumulative ZP-coder output bytes (an approximation — see
//!     `ZpEncoder::bytes_written` doc — but band-granular, which no wall-clock
//!     total ever was),
//!   - call/true counts for the four bit categories the codec emits:
//!     block-band NEW, per-bucket NEW, coefficient activation, coefficient
//!     refinement.
//!
//! It operates on the same "segmented BG (production BG44 workload)" that round
//! 35 (IW44_SLICE_RD) used: the real sub-sampled background pixmap extracted
//! from each page's existing BG44 chunk, not a synthetic full-detail render.
//!
//! Usage:
//!   cargo run --release --features="iw44-probe,cli" --example iw44_band_probe \
//!       -- tests/corpus/colorbook.djvu tests/corpus/watchmaker.djvu
#![cfg(feature = "iw44-probe")]

use std::path::Path;
use std::process::ExitCode;

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color, probe},
};

/// `BAND_BUCKETS` from `djvu-iw44/src/lib.rs` — not exported (encoder/decoder
/// shared spec constant, `pub(crate)`), so mirrored here for display purposes
/// only. Band 0 = DC (coarsest); band 9 = finest detail.
const BAND_BUCKETS: [(usize, usize); 10] = [
    (0, 0),
    (1, 1),
    (2, 2),
    (3, 3),
    (4, 7),
    (8, 11),
    (12, 15),
    (16, 31),
    (32, 47),
    (48, 63),
];

fn decode_bg44(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

/// Write an RGBA pixmap as a binary PPM (P6) for feeding to `c44`.
fn write_ppm(path: &Path, pm: &Pixmap) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(pm.data.len());
    buf.extend_from_slice(format!("P6\n{} {}\n255\n", pm.width, pm.height).as_bytes());
    for p in 0..(pm.width as usize * pm.height as usize) {
        buf.push(pm.data[p * 4]);
        buf.push(pm.data[p * 4 + 1]);
        buf.push(pm.data[p * 4 + 2]);
    }
    std::fs::write(path, buf)
}

/// Run `c44` on a pixmap, return the encoded `.djvu` size in bytes (BG44 chunk
/// dominates for a photo/background-only input). Returns `None` if `c44` is not
/// on PATH or fails.
fn c44_size(pm: &Pixmap, tmp_tag: &str) -> Option<usize> {
    let tmp = std::env::temp_dir();
    let ppm = tmp.join(format!("iw44_band_probe_{tmp_tag}.ppm"));
    let out = tmp.join(format!("iw44_band_probe_{tmp_tag}.djvu"));
    write_ppm(&ppm, pm).ok()?;
    let status = std::process::Command::new("c44")
        .args([
            ppm.to_string_lossy().as_ref(),
            out.to_string_lossy().as_ref(),
        ])
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&ppm);
    if !status.status.success() {
        let _ = std::fs::remove_file(&out);
        return None;
    }
    let size = std::fs::metadata(&out).ok()?.len() as usize;
    let _ = std::fs::remove_file(&out);
    Some(size)
}

fn print_band_table(snap: &[probe::BandStats; 10], zp_total: u64) {
    println!(
        "{:>4} {:>10} {:>9} {:>7}  {:>13} {:>13} {:>13} {:>13}",
        "band",
        "buckets",
        "bytes",
        "%zp",
        "blockband c/t",
        "bucketNEW c/t",
        "activate c/t",
        "refine c/t"
    );
    for (band, s) in snap.iter().enumerate() {
        let (from, to) = BAND_BUCKETS[band];
        let pct = if zp_total > 0 {
            100.0 * s.bytes as f64 / zp_total as f64
        } else {
            0.0
        };
        println!(
            "{:>4} {:>10} {:>9} {:>6.2}%  {:>6}/{:<6} {:>6}/{:<6} {:>6}/{:<6} {:>6}/{:<6}",
            band,
            format!("{from}-{to}"),
            s.bytes,
            pct,
            s.block_band.calls,
            s.block_band.true_count,
            s.bucket_new.calls,
            s.bucket_new.true_count,
            s.activate.calls,
            s.activate.true_count,
            s.refine.calls,
            s.refine.true_count,
        );
    }
}

fn process_file(path: &Path) {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: {e}", path.display());
            return;
        }
    };
    let doc = match DjVuDocument::parse(&data) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: parse failed: {e}", path.display());
            return;
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

        probe::reset();
        let opts = Iw44EncodeOptions::default();
        let rs_chunks = encode_iw44_color(&orig_pixmap, &opts);
        let snap = probe::snapshot();

        let rs_total: usize = rs_chunks.iter().map(|c| c.len()).sum();
        let zp_total: u64 = snap.iter().map(|s| s.bytes).sum();
        let c44 = c44_size(&orig_pixmap, &format!("{name}_{page_idx}"));

        println!(
            "\n=== {name} page {page_idx} ({}x{}) ===",
            orig_pixmap.width, orig_pixmap.height
        );
        println!(
            "ours: {rs_total} B total ({zp_total} B zp-coded, {} B header/pad overhead){}",
            rs_total.saturating_sub(zp_total as usize),
            match c44 {
                Some(c) => format!(
                    "  |  c44: {c} B  (ours/c44 = {:.3}x)",
                    rs_total as f64 / c as f64
                ),
                None => "  |  c44: n/a (not on PATH or failed)".to_string(),
            }
        );
        print_band_table(&snap, zp_total);

        // Coarse (bands 0-3) vs mid (4-6) vs fine (7-9) byte share — the
        // question this probe exists to answer.
        let coarse: u64 = snap[0..4].iter().map(|s| s.bytes).sum();
        let mid: u64 = snap[4..7].iter().map(|s| s.bytes).sum();
        let fine: u64 = snap[7..10].iter().map(|s| s.bytes).sum();
        if zp_total > 0 {
            println!(
                "share: coarse(0-3)={:.1}%  mid(4-6)={:.1}%  fine(7-9)={:.1}%",
                100.0 * coarse as f64 / zp_total as f64,
                100.0 * mid as f64 / zp_total as f64,
                100.0 * fine as f64 / zp_total as f64,
            );
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: iw44_band_probe <file.djvu> [<file2.djvu> ...]\n\n\
             Per-band bit/byte attribution for the IW44 BG44 encoder on each\n\
             page's real background. Requires --features iw44-probe."
        );
        return ExitCode::from(2);
    }
    for arg in &args {
        process_file(Path::new(arg));
    }
    ExitCode::SUCCESS
}
