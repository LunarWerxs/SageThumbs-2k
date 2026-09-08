//! D_AA_ZOOM quality harness: is bilinear mask-coverage AA actually closer to
//! the "ideal" upscale than the current hard-edged nearest-neighbour mask?
//!
//! Method (standard downsample/upsample fidelity test):
//!   1. Render the real page at its native resolution — this *is* the ground
//!      truth (a JB2 mask has no higher-resolution information to recover;
//!      the native decode is as good as it gets).
//!   2. Render the same page at a reduced size (native / divisor). This uses
//!      the existing (always-on) `mask_box_coverage` downscale AA, so the
//!      low-res image already has anti-aliased edges — a reasonable proxy for
//!      "a lower-resolution scan of the same source".
//!   3. Threshold that low-res render back to a bilevel `Bitmap` and
//!      re-encode it as a standalone lossless JB2 DjVu page (`PageEncoder`).
//!   4. Render *that* synthetic low-res page back up to the native size twice:
//!      once with `mask_aa: false` (today's default, hard nearest edges) and
//!      once with `mask_aa: true` (bilinear coverage AA).
//!   5. Compare both against the step-1 ground truth with the D1 SSIM/PSNR
//!      harness (`djvu_rs::quality`). Whichever upscale is perceptually
//!      closer to the true edges wins.
//!
//! Usage:
//!   cargo run --release --example mask_aa_quality -- <file.djvu> [page] [divisors...]
//!   (divisors default to 2 4 — i.e. 2× and 4× zoom reconstruction)
#![allow(deprecated)]

use djvu_rs::Bitmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{self, RenderOptions, Resampling, UserRotation};
use djvu_rs::quality;

fn opts(w: u32, h: u32, mask_aa: bool) -> RenderOptions {
    RenderOptions {
        width: w,
        height: h,
        scale: 1.0,
        bold: 0,
        aa: false,
        rotation: UserRotation::None,
        permissive: false,
        resampling: Resampling::Bilinear,
        mask_aa,
    }
}

/// Threshold an RGBA pixmap's luminance to a bilevel `Bitmap` (dark = foreground).
fn threshold_to_bitmap(pm: &djvu_rs::Pixmap) -> Bitmap {
    let mut bm = Bitmap::new(pm.width, pm.height);
    for y in 0..pm.height {
        for x in 0..pm.width {
            let (r, g, b) = pm.get_rgb(x, y);
            let luma = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000;
            if luma < 128 {
                bm.set(x, y, true);
            }
        }
    }
    bm
}

fn print_row(label: &str, rep: quality::QualityReport) {
    let psnr = if rep.psnr_db.is_infinite() {
        "   inf".to_string()
    } else {
        format!("{:6.2}", rep.psnr_db)
    };
    println!(
        "    {label:<22}  SSIM {:.4}   PSNR {psnr} dB   MSE {:8.2}",
        rep.ssim, rep.mse
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mask_aa_quality <file.djvu> [page] [divisors...]");
        std::process::exit(2);
    }
    let file = &args[0];
    let page_idx: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let divisors: Vec<u32> = args[2.min(args.len())..]
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    let divisors = if divisors.is_empty() {
        vec![2, 4]
    } else {
        divisors
    };

    let data = std::fs::read(file).unwrap_or_else(|e| {
        eprintln!("cannot read {file}: {e}");
        std::process::exit(1);
    });
    let doc = DjVuDocument::parse(&data).unwrap_or_else(|e| {
        eprintln!("parse failed: {e}");
        std::process::exit(1);
    });
    let page = doc.page(page_idx).unwrap_or_else(|e| {
        eprintln!("page {page_idx}: {e}");
        std::process::exit(1);
    });
    let (w, h) = (page.width() as u32, page.height() as u32);
    println!("D_AA_ZOOM ideal-upscale evaluation: {file} (page {page_idx}, native {w}x{h})");

    // Step 1: ground truth = native-resolution render.
    let reference = djvu_render::render_pixmap(page, &opts(w, h, false))
        .expect("native reference render should succeed");

    for divisor in divisors {
        let (dw, dh) = ((w / divisor).max(1), (h / divisor).max(1));
        println!(
            "\n  --- {divisor}x zoom reconstruction (low-res {dw}x{dh} -> native {w}x{h}) ---"
        );

        // Step 2: reduced-size render (existing downscale AA already applies).
        let lowres = djvu_render::render_pixmap(page, &opts(dw, dh, false))
            .expect("low-res render should succeed");

        // Step 3: threshold + re-encode as a standalone lossless JB2 page.
        let bm = threshold_to_bitmap(&lowres);
        let synth_bytes = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Lossless)
            .encode()
            .expect("synthetic low-res page should encode");
        let synth_doc = DjVuDocument::parse(&synth_bytes).expect("synthetic page should parse");
        let synth_page = synth_doc.page(0).expect("synthetic page 0");

        // Step 4: upscale the synthetic low-res page back to native size.
        let nearest = djvu_render::render_pixmap(synth_page, &opts(w, h, false))
            .expect("nearest upscale should succeed");
        let aa = djvu_render::render_pixmap(synth_page, &opts(w, h, true))
            .expect("AA upscale should succeed");

        // Step 5: compare both against the native-resolution ground truth.
        print_row(
            "nearest (mask_aa=false)",
            quality::compare(&nearest, &reference),
        );
        print_row("AA (mask_aa=true)", quality::compare(&aa, &reference));

        // Perf guard: AA upscale should stay within a small constant factor of
        // nearest. Timed against the same synthetic low-res page so both sides
        // decode identical input; reps chosen to keep this harness quick while
        // still averaging out noise.
        let reps = 20;
        let t0 = std::time::Instant::now();
        for _ in 0..reps {
            let _ = djvu_render::render_pixmap(synth_page, &opts(w, h, false));
        }
        let nearest_dur = t0.elapsed();
        let t1 = std::time::Instant::now();
        for _ in 0..reps {
            let _ = djvu_render::render_pixmap(synth_page, &opts(w, h, true));
        }
        let aa_dur = t1.elapsed();
        println!(
            "    perf: nearest {:.2}ms/render   AA {:.2}ms/render   ratio {:.2}x",
            nearest_dur.as_secs_f64() * 1000.0 / reps as f64,
            aa_dur.as_secs_f64() * 1000.0 / reps as f64,
            aa_dur.as_secs_f64() / nearest_dur.as_secs_f64()
        );
    }
    println!(
        "\n  -> higher SSIM / PSNR = perceptually closer to the native-resolution ground truth"
    );
}
