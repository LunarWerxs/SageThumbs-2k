//! D3_BICUBIC — bicubic-vs-bilinear FG44 upsampling quality measurement.
//!
//! ## Outcome: Rejected (see PERF_EXPERIMENTS.md)
//!
//! This experiment prototyped an opt-in `RenderOptions::fg_bicubic` flag that
//! swapped the compositor's `sample_bilinear` call for a Catmull-Rom bicubic
//! sampler in the non-1:1 (upscale) branch of `composite_rows_bilinear_one` —
//! the code path that reconstructs the FG44 colour layer, which is stored
//! heavily subsampled (typically ~12× per axis) and upsampled per-pixel onto
//! the page grid. The hypothesis was that bilinear over a 12× gap smears
//! colour across glyph clusters, and a sharper 4×4-tap kernel would visibly
//! help colour text.
//!
//! Measured with the real in-compositor implementation on `colorbook.djvu`
//! (multicolour FG) at 2×/2.4× zoom, pages 0-1, whole-page render vs `ddjvu`:
//!   - Bilinear vs ddjvu:  SSIM 0.9895-0.9924, PSNR 36.4-38.5 dB
//!   - Bicubic  vs ddjvu:  SSIM 0.9895-0.9923, PSNR 36.4-38.5 dB  (same, to 4 decimals)
//!   - Bilinear vs Bicubic (direct, whole page): SSIM 0.9999, PSNR 58.8-61.0 dB
//!   - Saved crops of colour text were visually indistinguishable
//!   - Enabling bicubic cost ~17% more time in the affected path (285ms vs
//!     334ms/frame for a 2× colorbook render), for zero perceptible gain
//!
//! Conclusion: the reconstruction gap bilinear leaves at a 12× subsample ratio
//! is already below the perceptual floor at realistic zoom levels — the flag
//! was reverted from `src/djvu_render.rs` (kept: default bilinear path, byte-
//! identical, no risk).
//!
//! This example is kept as a standalone, buildable reproduction of the same
//! question using only public API (`DjVuPage::extract_foreground` +
//! `decoded_mask`), now that the in-compositor flag is gone. Rather than
//! resizing the *whole composited page* (which would conflate the FG44
//! reconstruction with mask/text-edge sharpening — a confound that makes
//! bicubic look artificially better against `ddjvu`, contradicting the real,
//! isolated finding above), it isolates exactly the layer in question: decode
//! the small native FG44 colour plane, upsample it bilinear vs bicubic, and
//! compare directly — plus save mask-composited crops (FG over white) for a
//! visual check.
//!
//! ## Usage
//!   cargo run --release --example fg_bicubic_quality -- <file.djvu> [page] [zoom]
#![allow(deprecated)]

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::quality;

// ---- standalone resamplers (whole-pixmap, library-independent) ----

#[inline]
fn catmull_rom_weights(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

/// Resize `src` to `(dw, dh)` using bilinear interpolation, clamped to edges.
/// Mirrors the compositor's `sample_bilinear` semantics closely enough for an
/// apples-to-apples comparison against `bicubic_resize` below.
fn bilinear_resize(src: &Pixmap, dw: u32, dh: u32) -> Pixmap {
    let mut out = Pixmap::new(dw, dh, 255, 255, 255, 255);
    let sw = src.width.saturating_sub(1);
    let sh = src.height.saturating_sub(1);
    let xscale = src.width as f32 / dw as f32;
    let yscale = src.height as f32 / dh as f32;
    for dy in 0..dh {
        let sy = ((dy as f32 + 0.5) * yscale - 0.5).max(0.0);
        let y0 = (sy.floor() as u32).min(sh);
        let ty = sy - y0 as f32;
        let y1 = (y0 + 1).min(sh);
        for dx in 0..dw {
            let sx = ((dx as f32 + 0.5) * xscale - 0.5).max(0.0);
            let x0 = (sx.floor() as u32).min(sw);
            let tx = sx - x0 as f32;
            let x1 = (x0 + 1).min(sw);
            let (r00, g00, b00) = src.get_rgb(x0, y0);
            let (r10, g10, b10) = src.get_rgb(x1, y0);
            let (r01, g01, b01) = src.get_rgb(x0, y1);
            let (r11, g11, b11) = src.get_rgb(x1, y1);
            let lerp = |a: u8, b: u8, t: f32| a as f32 + (b as f32 - a as f32) * t;
            let top_r = lerp(r00, r10, tx);
            let bot_r = lerp(r01, r11, tx);
            let top_g = lerp(g00, g10, tx);
            let bot_g = lerp(g01, g11, tx);
            let top_b = lerp(b00, b10, tx);
            let bot_b = lerp(b01, b11, tx);
            let r = lerp(top_r as u8, bot_r as u8, ty).round().clamp(0.0, 255.0) as u8;
            let g = lerp(top_g as u8, bot_g as u8, ty).round().clamp(0.0, 255.0) as u8;
            let b = lerp(top_b as u8, bot_b as u8, ty).round().clamp(0.0, 255.0) as u8;
            out.set_rgb(dx, dy, r, g, b);
        }
    }
    out
}

/// Resize `src` to `(dw, dh)` using a separable 4x4-tap Catmull-Rom kernel
/// (a = -0.5), clamped to edges. Standalone copy of the reverted in-compositor
/// `sample_bicubic`.
fn bicubic_resize(src: &Pixmap, dw: u32, dh: u32) -> Pixmap {
    let mut out = Pixmap::new(dw, dh, 255, 255, 255, 255);
    let sw = src.width.saturating_sub(1);
    let sh = src.height.saturating_sub(1);
    let xscale = src.width as f32 / dw as f32;
    let yscale = src.height as f32 / dh as f32;
    for dy in 0..dh {
        let sy = ((dy as f32 + 0.5) * yscale - 0.5).max(0.0);
        let y1f = sy.floor();
        let ty = sy - y1f;
        let y1 = (y1f as i64).clamp(0, sh as i64) as u32;
        let wy = catmull_rom_weights(ty);
        let ys = [y1.saturating_sub(1), y1, (y1 + 1).min(sh), (y1 + 2).min(sh)];
        for dx in 0..dw {
            let sx = ((dx as f32 + 0.5) * xscale - 0.5).max(0.0);
            let x1f = sx.floor();
            let tx = sx - x1f;
            let x1 = (x1f as i64).clamp(0, sw as i64) as u32;
            let wx = catmull_rom_weights(tx);
            let xs = [x1.saturating_sub(1), x1, (x1 + 1).min(sw), (x1 + 2).min(sw)];
            let mut r = 0.0_f32;
            let mut g = 0.0_f32;
            let mut b = 0.0_f32;
            for (j, &yy) in ys.iter().enumerate() {
                let mut rr = 0.0_f32;
                let mut gg = 0.0_f32;
                let mut bb = 0.0_f32;
                for (i, &xx) in xs.iter().enumerate() {
                    let (pr, pg, pb) = src.get_rgb(xx, yy);
                    rr += pr as f32 * wx[i];
                    gg += pg as f32 * wx[i];
                    bb += pb as f32 * wx[i];
                }
                r += rr * wy[j];
                g += gg * wy[j];
                b += bb * wy[j];
            }
            out.set_rgb(
                dx,
                dy,
                r.round().clamp(0.0, 255.0) as u8,
                g.round().clamp(0.0, 255.0) as u8,
                b.round().clamp(0.0, 255.0) as u8,
            );
        }
    }
    out
}

fn print_row(label: &str, rep: quality::QualityReport) {
    let psnr = if rep.psnr_db.is_infinite() {
        "   inf".to_string()
    } else {
        format!("{:6.2}", rep.psnr_db)
    };
    println!(
        "  {label:<24}  SSIM {:.4}   PSNR {psnr} dB   MSE {:8.2}",
        rep.ssim, rep.mse
    );
}

/// Copy a clamped sub-rectangle of `pm` into a new owned `Pixmap`.
fn crop(pm: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Pixmap {
    let x = x.min(pm.width.saturating_sub(1));
    let y = y.min(pm.height.saturating_sub(1));
    let w = w.min(pm.width - x).max(1);
    let h = h.min(pm.height - y).max(1);
    let mut out = Pixmap::new(w, h, 255, 255, 255, 255);
    for row in 0..h {
        let src_off = ((y + row) * pm.width + x) as usize * 4;
        let dst_off = (row * w) as usize * 4;
        out.data[dst_off..dst_off + w as usize * 4]
            .copy_from_slice(&pm.data[src_off..src_off + w as usize * 4]);
    }
    out
}

fn save_png(pm: &Pixmap, path: &str) {
    let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}"));
    let mut w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(&mut w, pm.width, pm.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut h| h.write_image_data(&pm.data))
        .unwrap_or_else(|e| panic!("write {path}: {e}"));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = args
        .first()
        .cloned()
        .unwrap_or_else(|| "references/djvujs/library/assets/colorbook.djvu".to_string());
    let page_idx: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let zoom: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2.0);

    let data = std::fs::read(&file).unwrap_or_else(|e| panic!("read {file}: {e}"));
    let doc = DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse {file}: {e}"));
    let page = doc
        .page(page_idx)
        .unwrap_or_else(|e| panic!("page {page_idx}: {e}"));
    let nw = page.width() as u32;
    let nh = page.height() as u32;
    let (tw, th) = (
        ((nw as f64) * zoom).round() as u32,
        ((nh as f64) * zoom).round() as u32,
    );

    println!(
        "D3_BICUBIC quality (REJECTED — see module doc): {file} (page {page_idx})  native {nw}×{nh} → {tw}×{th} ({zoom}× zoom)"
    );

    let fg_native = page
        .extract_foreground()
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("page {page_idx} has no FG44 layer"));
    println!(
        "  FG44 native plane: {}×{}  (≈{:.1}× subsample vs page)",
        fg_native.width,
        fg_native.height,
        nw as f32 / fg_native.width as f32
    );

    // Isolated comparison: upsample the FG44 plane alone, bilinear vs bicubic,
    // straight to the target size — no mask, no background, no ddjvu (which
    // can't isolate FG either). This is the actual quantity in question.
    let fg_bilinear = bilinear_resize(&fg_native, tw, th);
    let fg_bicubic = bicubic_resize(&fg_native, tw, th);
    print_row(
        "FG-only Bilinear vs Bicubic",
        quality::compare(&fg_bilinear, &fg_bicubic),
    );

    // Visual crop: composite over white via the JB2 mask (nearest-scaled by
    // just decoding at target size isn't available publicly, so we scale the
    // mask coordinates by nearest lookup instead).
    if let Some(mask) = page.decoded_mask() {
        let (mw, mh) = (mask.width, mask.height);
        // Build a `(tw, th)`-sized bitmap-like composite by nearest-mapping
        // each target pixel back into mask space.
        let mut composite_bilinear = Pixmap::new(tw, th, 255, 255, 255, 255);
        let mut composite_bicubic = Pixmap::new(tw, th, 255, 255, 255, 255);
        for y in 0..th {
            let my = ((y as u64 * mh as u64) / th as u64).min(mh as u64 - 1) as u32;
            for x in 0..tw {
                let mx = ((x as u64 * mw as u64) / tw as u64).min(mw as u64 - 1) as u32;
                if mask.get(mx, my) {
                    let (r, g, b) = fg_bilinear.get_rgb(x, y);
                    composite_bilinear.set_rgb(x, y, r, g, b);
                    let (r, g, b) = fg_bicubic.get_rgb(x, y);
                    composite_bicubic.set_rgb(x, y, r, g, b);
                }
            }
        }

        std::fs::create_dir_all("_pr_assets").ok();
        let cw = (tw / 6).max(200);
        let ch = (th / 24).max(100);
        let cx = tw / 4;
        let cy = th / 6;
        let stem = std::path::Path::new(&file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page");
        let bilinear_path =
            format!("_pr_assets/fg_bicubic_{stem}_p{page_idx}_{zoom}x_bilinear.png");
        let bicubic_path = format!("_pr_assets/fg_bicubic_{stem}_p{page_idx}_{zoom}x_bicubic.png");
        save_png(&crop(&composite_bilinear, cx, cy, cw, ch), &bilinear_path);
        save_png(&crop(&composite_bicubic, cx, cy, cw, ch), &bicubic_path);
        println!("  wrote crops: {bilinear_path}  {bicubic_path}");
    } else {
        println!("  (no JB2 mask — skipping visual crop)");
    }
}
