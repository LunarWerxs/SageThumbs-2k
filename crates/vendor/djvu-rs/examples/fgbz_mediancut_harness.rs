//! FGBZ_MEDIANCUT measurement harness.
//!
//! Loads a multicolour-foreground `.djvu` fixture, renders its native page
//! (our "ground truth" — the best reference available without the raw
//! pre-compression scan), then **re-encodes** that render through our own
//! layered `Quality` pipeline twice: once with the current
//! [`FgbzPaletteOptions::Exact`] (default, byte-identical to pre-experiment
//! behaviour) and once per requested [`FgbzPaletteOptions::MedianCut`] size.
//! Each candidate is decoded back and scored against the ground truth with
//! the D1 perceptual harness (`djvu_rs::quality`), alongside FGbz chunk size
//! and palette colour count.
//!
//! Usage:
//!   cargo run --release --example fgbz_mediancut_harness -- <fixture.djvu> [page] [k1,k2,...]
//!
//! With `--crop`, also writes before/after PNG crops of the highest-chroma
//! region (the "coloured text") to `_pr_assets/`.

#![allow(deprecated)]

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, FgbzPaletteOptions, PageEncoder};
use djvu_rs::quality;

fn write_png(path: &str, pm: &Pixmap) {
    let file = std::fs::File::create(path).expect("create png");
    let mut w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(&mut w, pm.width, pm.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(&pm.data)
        .unwrap();
}

/// Bounding box (x0, y0, x1, y1) of the highest-chroma region: the window
/// (of `win` pixels) with the most pixels whose max-min channel spread
/// exceeds a saturation threshold, i.e. the densest patch of *strongly*
/// coloured ink — a proxy for "coloured text" without hardcoding
/// fixture-specific coordinates. Counting saturated pixels (rather than
/// averaging spread) avoids being fooled by low-amplitude background
/// gradient noise, which has small but nonzero spread everywhere.
fn most_colorful_window(pm: &Pixmap, win: u32) -> (u32, u32, u32, u32) {
    const SAT_THRESHOLD: i32 = 60;
    let step = (win / 4).max(1);
    let mut best = (0u32, 0u32, -1i64);
    let mut y = 0;
    while y + win <= pm.height {
        let mut x = 0;
        while x + win <= pm.width {
            let mut count = 0i64;
            // Sub-sample every 2nd pixel in the window for speed.
            let mut wy = 0;
            while wy < win {
                let mut wx = 0;
                while wx < win {
                    let (r, g, b) = pm.get_rgb(x + wx, y + wy);
                    let (r, g, b) = (r as i32, g as i32, b as i32);
                    let spread = r.max(g).max(b) - r.min(g).min(b);
                    if spread > SAT_THRESHOLD {
                        count += 1;
                    }
                    wx += 2;
                }
                wy += 2;
            }
            if count > best.2 {
                best = (x, y, count);
            }
            x += step;
        }
        y += step;
    }
    (
        best.0,
        best.1,
        (best.0 + win).min(pm.width),
        (best.1 + win).min(pm.height),
    )
}

fn crop(pm: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> Pixmap {
    let w = x1 - x0;
    let h = y1 - y0;
    let mut out = Pixmap::new(w, h, 0, 0, 0, 255);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = pm.get_rgb(x0 + x, y0 + y);
            out.set_rgb(x, y, r, g, b);
        }
    }
    out
}

/// Count distinct RGB colours in a pixmap (used as the "68 vs 28 colors"
/// style evidence from COLOR_AA).
fn distinct_colors(pm: &Pixmap) -> usize {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    for px in pm.data.chunks_exact(4) {
        set.insert((px[0], px[1], px[2]));
    }
    set.len()
}

/// Tiny deterministic xorshift32 PRNG (no `rand` dependency needed for a
/// reproducible synthetic fixture).
struct Xorshift32(u32);
impl Xorshift32 {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    /// Signed jitter in `-amp..=amp`.
    fn jitter(&mut self, amp: i32) -> i32 {
        if amp == 0 {
            return 0;
        }
        (self.next() % (2 * amp as u32 + 1)) as i32 - amp
    }
}

/// Builds a synthetic "coloured annotations on a scanned page" fixture: a
/// white page with several hundred small glyph-like blits across 6 ink
/// hues, each blit's fill jittered by scanner/JPEG-style per-pixel noise and
/// blended toward white at its border (anti-aliasing) — the exact scenario
/// PERF_EXPERIMENTS.md's FGBZ_MEDIANCUT note describes: many blits that are
/// "the same" ink colour to a human eye, but whose per-blit *average* colour
/// differs by a few LSBs because of noise/AA, bloating the `Exact` palette.
fn synthetic_multicolor_text_page() -> Pixmap {
    const HUES: [(u8, u8, u8); 6] = [
        (200, 20, 20),  // red
        (20, 140, 20),  // green
        (20, 40, 200),  // blue
        (180, 120, 10), // orange
        (140, 20, 160), // magenta
        (10, 140, 150), // teal
    ];
    let (w, h) = (900u32, 700u32);
    let mut pm = Pixmap::white(w, h);
    let mut rng = Xorshift32(0x9E3779B9);

    let (glyph_w, glyph_h) = (10u32, 14u32);
    let (col_pitch, row_pitch) = (14u32, 20u32);
    let margin = 20u32;
    let mut hue_idx = 0usize;

    let mut y = margin;
    while y + glyph_h + margin <= h {
        let mut x = margin;
        while x + glyph_w + margin <= w {
            let (br, bg, bb) = HUES[hue_idx % HUES.len()];
            hue_idx += 1;
            // Per-glyph solid fill with per-pixel jitter (±8, scan noise),
            // anti-aliased 1px border blended 50% toward white.
            for gy in 0..glyph_h {
                for gx in 0..glyph_w {
                    let is_border = gx == 0 || gy == 0 || gx == glyph_w - 1 || gy == glyph_h - 1;
                    let jr = (br as i32 + rng.jitter(8)).clamp(0, 255) as u8;
                    let jg = (bg as i32 + rng.jitter(8)).clamp(0, 255) as u8;
                    let jb = (bb as i32 + rng.jitter(8)).clamp(0, 255) as u8;
                    let (fr, fg, fb) = if is_border {
                        (
                            ((jr as u16 + 255) / 2) as u8,
                            ((jg as u16 + 255) / 2) as u8,
                            ((jb as u16 + 255) / 2) as u8,
                        )
                    } else {
                        (jr, jg, jb)
                    };
                    pm.set_rgb(x + gx, y + gy, fr, fg, fb);
                }
            }
            x += col_pitch;
        }
        y += row_pitch;
    }
    pm
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: fgbz_mediancut_harness <fixture.djvu> [page] [k1,k2,...] [--crop]");
        std::process::exit(2);
    }
    let path = &args[0];
    let page_idx: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let ks: Vec<u16> = args
        .get(2)
        .map(|s| {
            s.split(',')
                .filter_map(|v| v.parse().ok())
                .collect::<Vec<u16>>()
        })
        .unwrap_or_else(|| vec![4, 8, 16, 32, 64]);
    let save_crop = args.iter().any(|a| a == "--crop");

    let (source_pm, dpi, stem_override) = if path == "--synthetic" {
        (synthetic_multicolor_text_page(), 300u16, Some("synthetic"))
    } else {
        let data = std::fs::read(path).expect("read fixture");
        let doc = DjVuDocument::parse(&data).expect("parse fixture");
        let page = doc.page(page_idx).expect("page");
        let pm = djvu_rs::djvu_render::render_pixmap(
            page,
            &djvu_rs::djvu_render::RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                scale: 1.0,
                bold: 0,
                aa: false,
                rotation: djvu_rs::djvu_render::UserRotation::None,
                permissive: false,
                resampling: djvu_rs::djvu_render::Resampling::default(),
                mask_aa: false,
            },
        )
        .expect("render source");
        (pm, page.dpi(), None)
    };

    println!(
        "== {path} page {page_idx} — {}x{} — {} distinct colours (source render) ==",
        source_pm.width,
        source_pm.height,
        distinct_colors(&source_pm)
    );

    let encode_and_score = |label: String, opts: FgbzPaletteOptions| {
        let bytes = PageEncoder::from_pixmap(&source_pm)
            .with_dpi(dpi)
            .with_quality(EncodeQuality::Quality)
            .with_fgbz_options(opts)
            .encode()
            .expect("encode candidate");

        let cand_doc = DjVuDocument::parse(&bytes).expect("parse candidate");
        let cand_page = cand_doc.page(0).expect("candidate page");
        let fgbz_len = cand_page.raw_chunk(b"FGbz").map_or(0, |d| d.len());
        let palette_len = cand_page
            .raw_chunk(b"FGbz")
            .and_then(|d| djvu_rs::fgbz_encode::decode_fgbz(d).ok())
            .map_or(0, |(p, _)| p.len());

        let rendered = djvu_rs::djvu_render::render_pixmap(
            cand_page,
            &djvu_rs::djvu_render::RenderOptions {
                width: source_pm.width,
                height: source_pm.height,
                scale: 1.0,
                bold: 0,
                aa: false,
                rotation: djvu_rs::djvu_render::UserRotation::None,
                permissive: false,
                resampling: djvu_rs::djvu_render::Resampling::default(),
                mask_aa: false,
            },
        )
        .expect("render candidate");

        let rep = quality::compare(&source_pm, &rendered);
        let color_rep = quality::compare_color(&source_pm, &rendered);
        println!(
            "  {label:<24}  bytes {:>9}  FGbz {:>7}B  palette {:>5}  SSIM {:.5}  PSNR {:>7}  MSE {:8.3}",
            bytes.len(),
            fgbz_len,
            palette_len,
            rep.ssim,
            if rep.psnr_db.is_infinite() {
                "inf".to_string()
            } else {
                format!("{:.2}", rep.psnr_db)
            },
            rep.mse
        );
        // QUALITY_COLOR: this is exactly the scenario the D1 harness's luma
        // blindness bit us on (round-31 note) — print the colour-aware companion
        // so a wash-out in coloured text no longer needs manual crop inspection.
        println!(
            "  {:<24}  SSIM Y {:.5}  Cb {:.5}  Cr {:.5}  comb {:.5}   ΔE mean {:6.2}  max {:6.2}",
            "    ↳ colour",
            color_rep.ssim_y,
            color_rep.ssim_cb,
            color_rep.ssim_cr,
            color_rep.ssim_combined,
            color_rep.delta_e_mean,
            color_rep.delta_e_max
        );
        (bytes, rendered, fgbz_len, palette_len, rep)
    };

    let (exact_bytes, exact_render, ..) =
        encode_and_score("Exact (baseline)".to_string(), FgbzPaletteOptions::Exact);

    let mut candidates: Vec<(u16, Vec<u8>, Pixmap)> = Vec::new();
    for &k in &ks {
        let (bytes, rendered, _, _, _) = encode_and_score(
            format!("MedianCut{{{k}}}"),
            FgbzPaletteOptions::MedianCut { max_colors: k },
        );
        candidates.push((k, bytes, rendered));
    }

    if save_crop {
        std::fs::create_dir_all("_pr_assets").ok();
        let (x0, y0, x1, y1) =
            most_colorful_window(&source_pm, 200.min(source_pm.width).min(source_pm.height));
        let stem = stem_override.map(|s| s.to_string()).unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        });

        write_png(
            &format!("_pr_assets/{stem}_source_crop.png"),
            &crop(&source_pm, x0, y0, x1, y1),
        );
        write_png(
            &format!("_pr_assets/{stem}_exact_crop.png"),
            &crop(&exact_render, x0, y0, x1, y1),
        );
        for (k, _, rendered) in &candidates {
            write_png(
                &format!("_pr_assets/{stem}_mediancut{k}_crop.png"),
                &crop(rendered, x0, y0, x1, y1),
            );
        }
        println!("crop region: ({x0},{y0})-({x1},{y1}) saved to _pr_assets/");
    }
    let _ = exact_bytes;
}
