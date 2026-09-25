//! IW44 quality diagnostic for issue-driven investigations.
//!
//! Re-encodes existing BG44 backgrounds with a small set of diagnostic variants
//! and prints JSONL.  This example is intentionally not a benchmark harness:
//! it is for localizing whether quality loss is from chroma sampling, luma
//! quantization/slice budget, or another encoder stage.
//!
//! Usage:
//!   cargo run --release --features=std --example diagnose_iw44_quality -- \
//!     tests/corpus/watchmaker.djvu tests/corpus/conquete_paix.djvu

use std::path::Path;
use std::process::ExitCode;

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color, encode_iw44_gray},
};

#[derive(Clone, Copy)]
enum Variant {
    ModelCurrent,
    ModelInverse,
    Default,
    FullChroma,
    MoreSlices,
    GrayLumaOnly,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::ModelCurrent => "model_current_ycbcr",
            Variant::ModelInverse => "model_inverse_ycbcr",
            Variant::Default => "default",
            Variant::FullChroma => "full_chroma",
            Variant::MoreSlices => "more_slices",
            Variant::GrayLumaOnly => "gray_luma_only",
        }
    }
}

#[derive(Debug)]
struct VariantResult {
    bytes: usize,
    psnr_luma_db: f64,
    psnr_rgb_db: f64,
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

fn psnr_luma(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    let mut mse = 0.0f64;
    let mut n = 0usize;
    for (ra, rb) in a
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0)
    {
        let d = luma(ra[0], ra[1], ra[2]) - luma(rb[0], rb[1], rb[2]);
        mse += d * d;
        n += 1;
    }
    psnr_from_mse(mse, n)
}

fn psnr_rgb(a: &Pixmap, b: &Pixmap) -> f64 {
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    let mut mse = 0.0f64;
    let mut n = 0usize;
    for (ra, rb) in a
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0)
    {
        for chan in 0..3 {
            let d = f64::from(ra[chan]) - f64::from(rb[chan]);
            mse += d * d;
            n += 1;
        }
    }
    psnr_from_mse(mse, n)
}

fn psnr_from_mse(sum_sq: f64, n: usize) -> f64 {
    if n == 0 {
        return f64::NAN;
    }
    let mse = sum_sq / n as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn current_encoder_ycbcr(r: u8, g: u8, b: u8) -> (i32, i32, i32) {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);
    let y = (r + (g << 1) + b) / 4 - 128;
    let cb = b - g;
    let cr = r - g;
    (y.clamp(-128, 127), cb.clamp(-256, 255), cr.clamp(-256, 255))
}

fn inverse_compatible_ycbcr(r: u8, g: u8, b: u8) -> (i32, i32, i32) {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);
    let dr = r - g;
    let db = b - g;
    let cb = ((-2 * dr + 8 * db) as f64 / 15.0).round() as i32;
    let cr = ((8 * dr - db) as f64 / 15.0).round() as i32;
    let y = (g - 128) + ((cb as f64 / 4.0) + (cr as f64 / 2.0)).round() as i32;
    (y.clamp(-128, 127), cb.clamp(-128, 127), cr.clamp(-128, 127))
}

fn ycbcr_to_rgb(y: i32, cb: i32, cr: i32) -> (u8, u8, u8) {
    let t2 = cr + (cr >> 1);
    let t3 = y + 128 - (cb >> 2);
    (
        (y + 128 + t2).clamp(0, 255) as u8,
        (t3 - (cr >> 1)).clamp(0, 255) as u8,
        (t3 + (cb << 1)).clamp(0, 255) as u8,
    )
}

fn model_roundtrip(reference: &Pixmap, model: fn(u8, u8, u8) -> (i32, i32, i32)) -> Pixmap {
    let mut out = Pixmap::white(reference.width, reference.height);
    for y in 0..reference.height {
        for x in 0..reference.width {
            let (r, g, b) = reference.get_rgb(x, y);
            let (yy, cb, cr) = model(r, g, b);
            let (rr, gg, bb) = ycbcr_to_rgb(yy, cb, cr);
            out.set_rgb(x, y, rr, gg, bb);
        }
    }
    out
}

fn json_float(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.3}")
    } else {
        "null".to_string()
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

fn decode_bg44_chunks(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for chunk in chunks {
        img.decode_chunk(chunk).ok()?;
    }
    img.to_rgb().ok()
}

fn decode_bg44_owned(chunks: &[Vec<u8>]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for chunk in chunks {
        img.decode_chunk(chunk).ok()?;
    }
    img.to_rgb().ok()
}

fn encode_variant(reference: &Pixmap, variant: Variant) -> Option<VariantResult> {
    if matches!(variant, Variant::ModelCurrent | Variant::ModelInverse) {
        let decoded = match variant {
            Variant::ModelCurrent => model_roundtrip(reference, current_encoder_ycbcr),
            Variant::ModelInverse => model_roundtrip(reference, inverse_compatible_ycbcr),
            _ => unreachable!(),
        };
        return Some(VariantResult {
            bytes: 0,
            psnr_luma_db: psnr_luma(reference, &decoded),
            psnr_rgb_db: psnr_rgb(reference, &decoded),
        });
    }

    let chunks = match variant {
        Variant::ModelCurrent | Variant::ModelInverse => unreachable!(),
        Variant::Default => encode_iw44_color(reference, &Iw44EncodeOptions::default()),
        Variant::FullChroma => {
            let opts = Iw44EncodeOptions {
                chroma_half: false,
                ..Iw44EncodeOptions::default()
            };
            encode_iw44_color(reference, &opts)
        }
        Variant::MoreSlices => {
            let opts = Iw44EncodeOptions {
                total_slices: 200,
                slices_per_chunk: 20,
                ..Iw44EncodeOptions::default()
            };
            encode_iw44_color(reference, &opts)
        }
        Variant::GrayLumaOnly => {
            let gray = reference.to_gray8();
            let opts = Iw44EncodeOptions {
                total_slices: 200,
                slices_per_chunk: 20,
                ..Iw44EncodeOptions::default()
            };
            encode_iw44_gray(&gray, &opts)
        }
    };
    let bytes = chunks.iter().map(Vec::len).sum();
    let decoded = decode_bg44_owned(&chunks)?;
    Some(VariantResult {
        bytes,
        psnr_luma_db: psnr_luma(reference, &decoded),
        psnr_rgb_db: psnr_rgb(reference, &decoded),
    })
}

fn process_file(path: &Path) -> Result<usize, String> {
    let data = std::fs::read(path).map_err(|err| err.to_string())?;
    let doc = DjVuDocument::parse(&data).map_err(|err| err.to_string())?;
    let variants = [
        Variant::ModelCurrent,
        Variant::ModelInverse,
        Variant::Default,
        Variant::FullChroma,
        Variant::MoreSlices,
        Variant::GrayLumaOnly,
    ];

    let mut pages = 0usize;
    for page_idx in 0..doc.page_count() {
        let page = doc.page(page_idx).map_err(|err| err.to_string())?;
        let bg44_chunks = page.bg44_chunks();
        if bg44_chunks.is_empty() {
            continue;
        }
        let orig_bytes = bg44_chunks.iter().map(|chunk| chunk.len()).sum::<usize>();
        let Some(reference) = decode_bg44_chunks(&bg44_chunks) else {
            eprintln!(
                "skip {} page {}: original BG44 decode failed",
                path.display(),
                page_idx
            );
            continue;
        };

        for variant in variants {
            let Some(result) = encode_variant(&reference, variant) else {
                eprintln!(
                    "skip {} page {} variant {}: decode failed",
                    path.display(),
                    page_idx,
                    variant.name()
                );
                continue;
            };
            println!(
                "{{\"file\":{},\"page\":{},\"variant\":{},\
                 \"width\":{},\"height\":{},\"orig_bg44_bytes\":{},\
                 \"rs_bg44_bytes\":{},\"byte_ratio\":{:.3},\
                 \"psnr_luma_db\":{},\"psnr_rgb_db\":{}}}",
                json_string(&path.display().to_string()),
                page_idx,
                json_string(variant.name()),
                reference.width,
                reference.height,
                orig_bytes,
                result.bytes,
                result.bytes as f64 / orig_bytes.max(1) as f64,
                json_float(result.psnr_luma_db),
                json_float(result.psnr_rgb_db)
            );
        }
        pages += 1;
    }
    Ok(pages)
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        eprintln!("usage: diagnose_iw44_quality <file.djvu> [<file2.djvu> ...]");
        return ExitCode::from(2);
    }

    let mut total = 0usize;
    for arg in args {
        match process_file(Path::new(&arg)) {
            Ok(pages) => total += pages,
            Err(err) => eprintln!("skip {arg}: {err}"),
        }
    }

    if total == 0 {
        eprintln!("no BG44 pages processed");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
