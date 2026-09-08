//! JB2 encoder quality benchmark — compares djvu-rs re-encoded Sjbz payload
//! size against the original Sjbz chunk in a DjVu file (typically produced
//! by `cjb2` from DjVuLibre).
//!
//! Also verifies lossless round-trip: the re-encoded stream must decode to
//! the exact same `Bitmap` as the original.
//!
//! Usage:
//!     cargo run --release --example encode_quality_jb2 -- <file.djvu> [<file2.djvu> ...]
//!
//! Output: one JSON line per page (jsonl), plus a final summary table on stderr.
//!
//! Fields per page:
//!   file            — path of the source DjVu
//!   page            — 0-based page index
//!   width, height   — bitmap dimensions
//!   orig_sjbz_bytes — original Sjbz payload size
//!   rs_sjbz_bytes   — djvu-rs re-encoded Sjbz payload size
//!   bpp_orig        — bits per pixel of the original
//!   bpp_rs          — bits per pixel of the re-encoded
//!   size_ratio      — rs_sjbz_bytes / orig_sjbz_bytes (>1.0 means djvu-rs worse)
//!   roundtrip_ok    — re-encoded decodes to the same bitmap

use std::path::Path;
use std::process::ExitCode;

use djvu_rs::{
    Bitmap, DjVuDocument, jb2,
    jb2_encode::{Jb2EncodeOptions, encode_jb2, encode_jb2_dict, encode_jb2_dict_with_options},
};

#[derive(Copy, Clone, Debug)]
enum RoundtripStatus {
    Ok,
    Mismatch,
    /// Decoder rejected the re-encoded stream — see issue #198
    /// (encoder emits whole image as one type-3 symbol > MAX_SYMBOL_PIXELS).
    DecodeError,
}

impl RoundtripStatus {
    fn as_str(&self) -> &'static str {
        match self {
            RoundtripStatus::Ok => "ok",
            RoundtripStatus::Mismatch => "mismatch",
            RoundtripStatus::DecodeError => "decode_error",
        }
    }
}

struct PageResult {
    file: String,
    page: usize,
    width: u32,
    height: u32,
    orig_sjbz_bytes: usize,
    rs_sjbz_bytes: usize,
    rs_dict_sjbz_bytes: usize,
    rs_lossy_sjbz_bytes: usize,
    /// Reconstruction-error pixel count for lossy mode (Hamming between
    /// the original page bitmap and the lossy decode). 0 = lossless or
    /// no near-duplicates fired the substitution.
    lossy_hamming: u64,
    roundtrip: RoundtripStatus,
    roundtrip_dict: RoundtripStatus,
}

/// Pixel-wise Hamming distance between two same-size bitmaps. Used to
/// quantify lossy reconstruction error vs the original.
fn bitmap_hamming(a: &Bitmap, b: &Bitmap) -> u64 {
    if a.width != b.width || a.height != b.height || a.data.len() != b.data.len() {
        return u64::MAX;
    }
    let mut total: u64 = 0;
    for (x, y) in a.data.iter().zip(b.data.iter()) {
        total += (x ^ y).count_ones() as u64;
    }
    total
}

fn process_file(path: &Path, lossy_threshold: f32) -> Vec<PageResult> {
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
        let orig_sjbz = match page.raw_chunk(b"Sjbz") {
            Some(s) => s,
            None => continue,
        };
        let bitmap = match page.extract_mask() {
            Ok(Some(b)) => b,
            _ => continue,
        };

        let rs_encoded = encode_jb2(&bitmap);
        let rs_dict_encoded = encode_jb2_dict(&bitmap);
        // `..Default::default()` is needless without the `experimental` feature
        // (then `lossy_threshold` is the only field) but required with it
        // (it also has `cross_size_rec6_probe`); allow rather than cfg-split.
        #[allow(clippy::needless_update)]
        let rs_lossy_encoded = if lossy_threshold > 0.0 {
            encode_jb2_dict_with_options(
                &bitmap,
                &[],
                &Jb2EncodeOptions {
                    lossy_threshold,
                    ..Default::default()
                },
            )
        } else {
            rs_dict_encoded.clone()
        };
        let lossy_hamming = if lossy_threshold > 0.0 {
            match jb2::decode(&rs_lossy_encoded, None) {
                Ok(d) => bitmap_hamming(&bitmap, &d),
                Err(_) => u64::MAX,
            }
        } else {
            0
        };

        let roundtrip = match jb2::decode(&rs_encoded, None) {
            Ok(b) => {
                if b.width == bitmap.width && b.height == bitmap.height && b.data == bitmap.data {
                    RoundtripStatus::Ok
                } else {
                    RoundtripStatus::Mismatch
                }
            }
            Err(_) => RoundtripStatus::DecodeError,
        };
        let roundtrip_dict = match jb2::decode(&rs_dict_encoded, None) {
            Ok(b) => {
                if b.width == bitmap.width && b.height == bitmap.height && b.data == bitmap.data {
                    RoundtripStatus::Ok
                } else {
                    RoundtripStatus::Mismatch
                }
            }
            Err(_) => RoundtripStatus::DecodeError,
        };

        out.push(PageResult {
            file: path.display().to_string(),
            page: page_idx,
            width: bitmap.width,
            height: bitmap.height,
            orig_sjbz_bytes: orig_sjbz.len(),
            rs_sjbz_bytes: rs_encoded.len(),
            rs_dict_sjbz_bytes: rs_dict_encoded.len(),
            rs_lossy_sjbz_bytes: rs_lossy_encoded.len(),
            lossy_hamming,
            roundtrip,
            roundtrip_dict,
        });
    }
    out
}

fn bpp(bytes: usize, width: u32, height: u32) -> f64 {
    if width == 0 || height == 0 {
        return f64::NAN;
    }
    (bytes as f64 * 8.0) / (width as f64 * height as f64)
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut lossy_threshold: f32 = 0.0;
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw_args.len() {
        let a = &raw_args[i];
        if a == "--lossy-threshold" {
            i += 1;
            if i >= raw_args.len() {
                eprintln!("--lossy-threshold requires a value (e.g. 0.04)");
                return ExitCode::from(2);
            }
            lossy_threshold = match raw_args[i].parse::<f32>() {
                Ok(v) if (0.0..=1.0).contains(&v) => v,
                _ => {
                    eprintln!(
                        "--lossy-threshold must be a fraction in [0.0, 1.0] (got {:?})",
                        raw_args[i]
                    );
                    return ExitCode::from(2);
                }
            };
        } else if let Some(rest) = a.strip_prefix("--lossy-threshold=") {
            lossy_threshold = match rest.parse::<f32>() {
                Ok(v) if (0.0..=1.0).contains(&v) => v,
                _ => {
                    eprintln!(
                        "--lossy-threshold must be a fraction in [0.0, 1.0] (got {:?})",
                        rest
                    );
                    return ExitCode::from(2);
                }
            };
        } else {
            files.push(a.clone());
        }
        i += 1;
    }

    if files.is_empty() {
        eprintln!(
            "usage: encode_quality_jb2 [--lossy-threshold <fraction>] <file.djvu> [<file2.djvu> ...]\n\
             \n\
             Re-encodes every Sjbz chunk via djvu-rs and compares size to the\n\
             original. Also verifies lossless round-trip.\n\
             \n\
             --lossy-threshold <f>  enable lossy rec-7 substitution at the given\n\
                                    Hamming fraction (e.g. 0.04 = 4%% of pixels).\n\
                                    Default 0 = lossless."
        );
        return ExitCode::from(2);
    }

    let mut all: Vec<PageResult> = Vec::new();
    for arg in &files {
        let path = Path::new(arg);
        all.extend(process_file(path, lossy_threshold));
    }

    for r in &all {
        println!(
            "{{\"file\":\"{}\",\"page\":{},\"width\":{},\"height\":{},\
             \"orig_sjbz_bytes\":{},\"rs_sjbz_bytes\":{},\"rs_dict_sjbz_bytes\":{},\
             \"rs_lossy_sjbz_bytes\":{},\"lossy_hamming\":{},\
             \"bpp_orig\":{:.4},\"bpp_rs\":{:.4},\"bpp_rs_dict\":{:.4},\"bpp_rs_lossy\":{:.4},\
             \"size_ratio\":{:.3},\"size_ratio_dict\":{:.3},\"size_ratio_lossy\":{:.3},\
             \"roundtrip\":\"{}\",\"roundtrip_dict\":\"{}\"}}",
            r.file,
            r.page,
            r.width,
            r.height,
            r.orig_sjbz_bytes,
            r.rs_sjbz_bytes,
            r.rs_dict_sjbz_bytes,
            r.rs_lossy_sjbz_bytes,
            r.lossy_hamming,
            bpp(r.orig_sjbz_bytes, r.width, r.height),
            bpp(r.rs_sjbz_bytes, r.width, r.height),
            bpp(r.rs_dict_sjbz_bytes, r.width, r.height),
            bpp(r.rs_lossy_sjbz_bytes, r.width, r.height),
            r.rs_sjbz_bytes as f64 / r.orig_sjbz_bytes.max(1) as f64,
            r.rs_dict_sjbz_bytes as f64 / r.orig_sjbz_bytes.max(1) as f64,
            r.rs_lossy_sjbz_bytes as f64 / r.orig_sjbz_bytes.max(1) as f64,
            r.roundtrip.as_str(),
            r.roundtrip_dict.as_str(),
        );
    }

    if all.is_empty() {
        eprintln!("no JB2 pages processed");
        return ExitCode::from(1);
    }

    let total_orig: usize = all.iter().map(|r| r.orig_sjbz_bytes).sum();
    let total_rs: usize = all.iter().map(|r| r.rs_sjbz_bytes).sum();
    let total_rs_dict: usize = all.iter().map(|r| r.rs_dict_sjbz_bytes).sum();
    let total_pixels: u64 = all.iter().map(|r| r.width as u64 * r.height as u64).sum();

    let roundtrip_ok = all
        .iter()
        .filter(|r| matches!(r.roundtrip, RoundtripStatus::Ok))
        .count();
    let roundtrip_mismatch = all
        .iter()
        .filter(|r| matches!(r.roundtrip, RoundtripStatus::Mismatch))
        .count();
    let roundtrip_decode_err = all
        .iter()
        .filter(|r| matches!(r.roundtrip, RoundtripStatus::DecodeError))
        .count();

    let roundtrip_dict_ok = all
        .iter()
        .filter(|r| matches!(r.roundtrip_dict, RoundtripStatus::Ok))
        .count();
    let roundtrip_dict_mismatch = all
        .iter()
        .filter(|r| matches!(r.roundtrip_dict, RoundtripStatus::Mismatch))
        .count();
    let roundtrip_dict_decode_err = all
        .iter()
        .filter(|r| matches!(r.roundtrip_dict, RoundtripStatus::DecodeError))
        .count();

    eprintln!();
    eprintln!("=== JB2 encoder quality summary ===");
    eprintln!("pages:                  {}", all.len());
    eprintln!();
    eprintln!("-- direct (record type 3, whole image) --");
    eprintln!("roundtrip ok:           {}", roundtrip_ok);
    eprintln!("roundtrip mismatch:     {}", roundtrip_mismatch);
    eprintln!(
        "roundtrip decode err:   {}   (issue #198: >1 MP single-symbol)",
        roundtrip_decode_err
    );
    eprintln!(
        "total rs size:       {:>10} bytes  ({:.4} bpp)",
        total_rs,
        (total_rs as f64 * 8.0) / total_pixels.max(1) as f64
    );
    eprintln!(
        "ratio rs / orig:     {:.3}×",
        total_rs as f64 / total_orig.max(1) as f64
    );
    eprintln!();
    eprintln!("-- dict (CC + rec types 1+7) --");
    eprintln!("roundtrip ok:           {}", roundtrip_dict_ok);
    eprintln!("roundtrip mismatch:     {}", roundtrip_dict_mismatch);
    eprintln!("roundtrip decode err:   {}", roundtrip_dict_decode_err);
    eprintln!(
        "total rs-dict size:  {:>10} bytes  ({:.4} bpp)",
        total_rs_dict,
        (total_rs_dict as f64 * 8.0) / total_pixels.max(1) as f64
    );
    eprintln!(
        "ratio rs-dict / orig: {:.3}×  (>1 = djvu-rs encoder worse)",
        total_rs_dict as f64 / total_orig.max(1) as f64
    );
    eprintln!();
    eprintln!(
        "total orig size:     {:>10} bytes  ({:.4} bpp)",
        total_orig,
        (total_orig as f64 * 8.0) / total_pixels.max(1) as f64
    );

    if lossy_threshold > 0.0 {
        let total_rs_lossy: usize = all.iter().map(|r| r.rs_lossy_sjbz_bytes).sum();
        let total_lossy_hamming: u64 = all
            .iter()
            .map(|r| {
                if r.lossy_hamming == u64::MAX {
                    0
                } else {
                    r.lossy_hamming
                }
            })
            .sum();
        let lossy_decode_err = all.iter().filter(|r| r.lossy_hamming == u64::MAX).count();
        eprintln!();
        eprintln!(
            "-- lossy dict (rec-7 near-dup substitution, threshold = {:.4}) --",
            lossy_threshold
        );
        eprintln!(
            "total rs-lossy size: {:>10} bytes  ({:.4} bpp)",
            total_rs_lossy,
            (total_rs_lossy as f64 * 8.0) / total_pixels.max(1) as f64
        );
        eprintln!(
            "ratio rs-lossy / orig:    {:.3}×",
            total_rs_lossy as f64 / total_orig.max(1) as f64
        );
        eprintln!(
            "ratio rs-lossy / rs-dict: {:.3}×",
            total_rs_lossy as f64 / total_rs_dict.max(1) as f64
        );
        eprintln!(
            "total reconstruction err: {} pixels  ({:.6} bits/pixel error)",
            total_lossy_hamming,
            total_lossy_hamming as f64 / total_pixels.max(1) as f64
        );
        eprintln!("lossy decode errors:      {}", lossy_decode_err);
    }

    if roundtrip_mismatch > 0 || roundtrip_dict_mismatch > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
