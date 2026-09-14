//! Perceptual render-quality harness (D1).
//!
//! Upgrades the `interop_pixdiff` idea from arithmetic mean-pixel-diff to the
//! two standard perceptual metrics (PSNR + SSIM, see `djvu_rs::quality`), so the
//! deferred quality experiments have a ground-truth number to optimise: linear-
//! light blending, mask-upscale AA, Lanczos-vs-bilinear downscale, bicubic FG
//! upsampling, gamma-correct downscale (PERF_EXPERIMENTS.md round-8 QUALITY-GATED).
//!
//! ## Reference
//!
//! DjVuLibre's `ddjvu` is the "quality floor" reference (same choice as
//! `interop_pixdiff`). We render a page our way and with `ddjvu` at the same
//! size and report SSIM/PSNR of ours vs the reference. When comparing two of our
//! own render modes (e.g. Bilinear vs Lanczos3 downscale), the one with the
//! higher SSIM-vs-`ddjvu` is perceptually closer to the reference decoder.
//!
//! If `ddjvu` is not on PATH the harness still reports the perceptual *delta*
//! between our two resampling modes (how much they differ), and the `--pair`
//! mode compares two files — both need no external tool.
//!
//! ## Usage
//!   cargo run --release --example quality_harness -- <file.djvu> [page] [--half|--third|--quarter]
//!   cargo run --release --example quality_harness -- --pair <a.djvu> <b.djvu> [page]
#![allow(deprecated)]

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{self, RenderOptions, Resampling, UserRotation};
use djvu_rs::quality;
use std::process::Command;

fn opts(w: u32, h: u32, resampling: Resampling) -> RenderOptions {
    RenderOptions {
        width: w,
        height: h,
        scale: 1.0,
        bold: 0,
        aa: false,
        rotation: UserRotation::None,
        permissive: false,
        resampling,
        mask_aa: false,
    }
}

fn render(path: &str, page_idx: usize, w: u32, h: u32, r: Resampling) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let doc = DjVuDocument::parse(&data).ok()?;
    let page = doc.page(page_idx).ok()?;
    djvu_render::render_pixmap(page, &opts(w, h, r)).ok()
}

fn geometry(path: &str, page_idx: usize) -> Option<(u32, u32)> {
    let data = std::fs::read(path).ok()?;
    let doc = DjVuDocument::parse(&data).ok()?;
    let page = doc.page(page_idx).ok()?;
    Some((page.width() as u32, page.height() as u32))
}

/// Parse a binary PPM (P6) into a Pixmap (alpha filled 255).
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
    pos += 1; // single whitespace after maxval
    let (w, h) = (nums[0], nums[1]);
    let rgb = data.get(pos..pos + w * h * 3)?;
    let mut pm = Pixmap::new(w as u32, h as u32, 0, 0, 0, 255);
    for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
        pm.data[i * 4] = px[0];
        pm.data[i * 4 + 1] = px[1];
        pm.data[i * 4 + 2] = px[2];
        pm.data[i * 4 + 3] = 255;
    }
    Some(pm)
}

/// Render `path` page via `ddjvu` at the given size. Returns None if ddjvu is
/// absent or fails.
fn ddjvu_render(path: &str, page_idx: usize, w: u32, h: u32) -> Option<Pixmap> {
    let out = Command::new("ddjvu")
        .args([
            "-format=ppm",
            &format!("-page={}", page_idx + 1),
            &format!("-size={w}x{h}"),
            path,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    ppm_to_pixmap(&out.stdout)
}

fn print_row(label: &str, rep: quality::QualityReport) {
    let psnr = if rep.psnr_db.is_infinite() {
        "   inf".to_string()
    } else {
        format!("{:6.2}", rep.psnr_db)
    };
    println!(
        "  {label:<26}  SSIM {:.4}   PSNR {psnr} dB   MSE {:8.2}",
        rep.ssim, rep.mse
    );
}

/// Colour-aware companion row (QUALITY_COLOR) — per-channel Y/Cb/Cr SSIM, the
/// luma-weighted combined score, and mean/max CIE76 ΔE. Printed right under
/// the luma-only `print_row` for the same pair so a reader can see at a glance
/// whether a quality delta is structural (luma) or colour (chroma/ΔE).
fn print_color_row(rep: quality::ColorQualityReport) {
    println!(
        "  {:<26}  SSIM Y {:.4}  Cb {:.4}  Cr {:.4}  comb {:.4}   ΔE mean {:6.2}  max {:6.2}",
        "  ↳ colour",
        rep.ssim_y,
        rep.ssim_cb,
        rep.ssim_cr,
        rep.ssim_combined,
        rep.delta_e_mean,
        rep.delta_e_max
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: quality_harness <file.djvu> [page] [--half|--third|--quarter]");
        eprintln!("       quality_harness --pair <a.djvu> <b.djvu> [page]");
        std::process::exit(2);
    }

    if args[0] == "--pair" {
        if args.len() < 3 {
            eprintln!("--pair needs two files");
            std::process::exit(2);
        }
        let (a, b) = (&args[1], &args[2]);
        let page_idx: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let Some((w, h)) = geometry(a, page_idx) else {
            eprintln!("cannot read {a}");
            std::process::exit(1);
        };
        let (Some(pa), Some(pb)) = (
            render(a, page_idx, w, h, Resampling::Bilinear),
            render(b, page_idx, w, h, Resampling::Bilinear),
        ) else {
            eprintln!("render failed");
            std::process::exit(1);
        };
        if (pa.width, pa.height) != (pb.width, pb.height) {
            eprintln!("renders differ in size, cannot compare");
            std::process::exit(1);
        }
        println!("pair: {a}  vs  {b}  (page {page_idx}, {w}×{h})");
        print_row("a vs b", quality::compare(&pa, &pb));
        print_color_row(quality::compare_color(&pa, &pb));
        return;
    }

    let file = &args[0];
    let page_idx: usize = args
        .get(1)
        .filter(|s| !s.starts_with("--"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let divisor: u32 = if args.iter().any(|a| a == "--quarter") {
        4
    } else if args.iter().any(|a| a == "--third") {
        3
    } else {
        2
    };

    let Some((nw, nh)) = geometry(file, page_idx) else {
        eprintln!("cannot read {file}");
        std::process::exit(1);
    };
    let (dw, dh) = ((nw / divisor).max(1), (nh / divisor).max(1));

    println!(
        "resampling quality: {file} (page {page_idx})  native {nw}×{nh} → ~{dw}×{dh} (1/{divisor})"
    );

    // Prefer the ddjvu (DjVuLibre) reference. Match our render to ddjvu's exact
    // output dimensions so the perceptual comparison is size-aligned.
    if let Some(reference) = ddjvu_render(file, page_idx, dw, dh) {
        let (rw, rh) = (reference.width, reference.height);
        let (Some(bilinear), Some(lanczos)) = (
            render(file, page_idx, rw, rh, Resampling::Bilinear),
            render(file, page_idx, rw, rh, Resampling::Lanczos3),
        ) else {
            eprintln!("render failed");
            std::process::exit(1);
        };
        println!("  reference: ddjvu (DjVuLibre) at {rw}×{rh}");
        print_row("Bilinear vs ddjvu", quality::compare(&bilinear, &reference));
        print_color_row(quality::compare_color(&bilinear, &reference));
        print_row("Lanczos3 vs ddjvu", quality::compare(&lanczos, &reference));
        print_color_row(quality::compare_color(&lanczos, &reference));
        println!("  → higher SSIM = perceptually closer to the reference decoder");
        return;
    }

    // No ddjvu: report the perceptual delta between our two resampling modes.
    let (Some(bilinear), Some(lanczos)) = (
        render(file, page_idx, dw, dh, Resampling::Bilinear),
        render(file, page_idx, dw, dh, Resampling::Lanczos3),
    ) else {
        eprintln!("render failed");
        std::process::exit(1);
    };
    println!("  (ddjvu not available — reporting perceptual delta between our modes)");
    print_row(
        "Bilinear vs Lanczos3",
        quality::compare(&bilinear, &lanczos),
    );
    print_color_row(quality::compare_color(&bilinear, &lanczos));
}
