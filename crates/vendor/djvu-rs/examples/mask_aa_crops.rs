//! D_AA_ZOOM visual evidence: save small before/after PNG crops of real text
//! at 2x/4x zoom, nearest (`mask_aa: false`, today's default) vs bilinear
//! coverage AA (`mask_aa: true`), so the aesthetic difference can be judged
//! by eye rather than only by a numeric metric (see `mask_aa_quality`).
//!
//! Auto-locates a text-dense window in the page's decoded JB2 mask (highest
//! foreground-bit density over a coarse grid of candidate windows) instead of
//! a hand-picked offset, so the crop is reproducible on any bilevel corpus
//! file passed on the command line.
//!
//! Usage:
//!   cargo run --release --example mask_aa_crops -- <file.djvu> [page] [out_dir]
#![allow(deprecated)]

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{self, RenderOptions, RenderRect, Resampling, UserRotation};
use std::path::PathBuf;

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

/// Coarse grid search over the decoded mask for the window with the most
/// foreground (black) bits — a proxy for "densest text", so the crop shows
/// glyph edges rather than blank margin.
fn find_text_window(mask: &djvu_rs::Bitmap, win_w: u32, win_h: u32) -> (u32, u32) {
    let step_x = (win_w / 2).max(1);
    let step_y = (win_h / 2).max(1);
    let mut best = (0u32, 0u32);
    let mut best_count = -1i64;
    let mut y = 0u32;
    while y + win_h <= mask.height {
        let mut x = 0u32;
        while x + win_w <= mask.width {
            let mut count = 0i64;
            for dy in (0..win_h).step_by(4) {
                for dx in (0..win_w).step_by(2) {
                    if mask.get(x + dx, y + dy) {
                        count += 1;
                    }
                }
            }
            if count > best_count {
                best_count = count;
                best = (x, y);
            }
            x += step_x;
        }
        y += step_y;
    }
    best
}

fn write_png(path: &std::path::Path, pm: &djvu_rs::Pixmap) {
    let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("create {path:?}: {e}"));
    let mut encoder = png::Encoder::new(file, pm.width, pm.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&pm.data).expect("png data");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mask_aa_crops <file.djvu> [page] [out_dir]");
        std::process::exit(2);
    }
    let file = &args[0];
    let page_idx: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let out_dir = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("_pr_assets"));
    std::fs::create_dir_all(&out_dir).expect("create out_dir");

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
    let (pw, ph) = (page.width() as u32, page.height() as u32);
    println!("{file} page {page_idx}: native {pw}x{ph}");

    let mask = page
        .extract_mask()
        .expect("mask decode")
        .expect("page should have a JB2/Smmr mask for a text crop demo");

    // Native-pixel crop window (small; zoomed up per factor below).
    let (win_w, win_h) = (110u32.min(pw), 80u32.min(ph));
    let (x0, y0) = find_text_window(&mask, win_w, win_h);
    println!("text-dense window: native ({x0},{y0}) size {win_w}x{win_h}");

    for &zoom in &[2u32, 4u32] {
        let full_w = pw * zoom;
        let full_h = ph * zoom;
        let region = RenderRect {
            x: x0 * zoom,
            y: y0 * zoom,
            width: win_w * zoom,
            height: win_h * zoom,
        };

        let nearest_opts = opts(full_w, full_h, false);
        let aa_opts = opts(full_w, full_h, true);

        let nearest = djvu_render::render_region(page, region, &nearest_opts)
            .expect("nearest region render should succeed");
        let aa = djvu_render::render_region(page, region, &aa_opts)
            .expect("AA region render should succeed");

        let nearest_path = out_dir.join(format!("mask_aa_{zoom}x_nearest.png"));
        let aa_path = out_dir.join(format!("mask_aa_{zoom}x_aa.png"));
        write_png(&nearest_path, &nearest);
        write_png(&aa_path, &aa);
        println!(
            "  {zoom}x: {} x {}  ->  {} , {}",
            region.width,
            region.height,
            nearest_path.display(),
            aa_path.display()
        );
    }
}
