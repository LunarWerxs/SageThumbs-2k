//! TH44_GRID: speed + quality harness for `Document::thumbnails*` (D1 gate).
//!
//! No corpus file embeds a decodable `TH44` (`D5_TH44_PREVIEW`, round-9), so
//! this harness synthesizes a small colour-layered bundle in memory — via the
//! #476 encoder (`encode_djvm_layered_shared_with_thumbnails`) fed pages
//! rendered from `colorbook.djvu` — to exercise the real fast-vs-fallback
//! trade the `thumbnails()` grid API makes:
//!
//! - **Speed**: cold (fresh-parse) `Th44Only` vs `RenderOnly` over the whole
//!   grid, matching D5's methodology (a real page-open + decode, not a
//!   warm-cache re-decode).
//! - **Quality**: SSIM (`djvu_rs::quality`) of the `Th44Only` output vs the
//!   `RenderOnly` output at several grid box sizes, including the 128 px-class
//!   size the API caps embedded thumbnails at.
//!
//! ## Usage
//!   cargo run --release --example thumbnail_grid_quality [--features parallel]

use djvu_rs::djvu_encode::{EncodeQuality, encode_djvm_layered_shared_with_thumbnails};
use djvu_rs::{Document, Pixmap, ThumbnailStrategy, quality};
use std::time::{Duration, Instant};

const SOURCE_PAGES: usize = 6;
const COLD_REPS: u32 = 8;

/// Build an in-memory colour-layered DJVM bundle (BG44 + JB2 mask, the
/// scenario D5 measured its 20–30× on) with embedded `TH44` thumbnails,
/// sourced from real page renders of `colorbook.djvu`.
fn synthesize_th44_color_bundle() -> Option<Vec<u8>> {
    let path = "references/djvujs/library/assets/colorbook.djvu";
    let data = std::fs::read(path).ok()?;
    let doc = Document::from_bytes(data).ok()?;
    let n = SOURCE_PAGES.min(doc.page_count());
    let mut pixmaps = Vec::with_capacity(n);
    for i in 0..n {
        let page = doc.page(i).ok()?;
        // Half native resolution keeps the one-time synth/encode step (not
        // what's being measured) fast while still a realistic large scan.
        let pm = page
            .render_to_size(page.width() / 2, page.height() / 2)
            .ok()?;
        pixmaps.push(pm);
    }
    encode_djvm_layered_shared_with_thumbnails(&pixmaps, EncodeQuality::Quality, 300, None, 2, true)
        .ok()
}

/// Minimal bilinear resample, used only to align two independently
/// box-fitted pixmaps that differ by a rounding pixel before computing SSIM
/// (the production API does not need this — each strategy's own box-fit
/// output is already correct on its own terms).
fn resample_bilinear(src: &Pixmap, dst_w: u32, dst_h: u32) -> Pixmap {
    if src.width == dst_w && src.height == dst_h {
        return Pixmap {
            width: src.width,
            height: src.height,
            data: src.data.clone(),
        };
    }
    let mut data = vec![0u8; (dst_w * dst_h * 4) as usize];
    let sx_ratio = src.width as f64 / dst_w as f64;
    let sy_ratio = src.height as f64 / dst_h as f64;
    for dy in 0..dst_h {
        let sy = ((dy as f64 + 0.5) * sy_ratio - 0.5).clamp(0.0, (src.height - 1) as f64);
        let y0 = sy.floor() as u32;
        let y1 = (y0 + 1).min(src.height - 1);
        let fy = sy - y0 as f64;
        for dx in 0..dst_w {
            let sx = ((dx as f64 + 0.5) * sx_ratio - 0.5).clamp(0.0, (src.width - 1) as f64);
            let x0 = sx.floor() as u32;
            let x1 = (x0 + 1).min(src.width - 1);
            let fx = sx - x0 as f64;
            let px = |x: u32, y: u32, c: usize| -> f64 {
                src.data[((y * src.width + x) * 4) as usize + c] as f64
            };
            for c in 0..4 {
                let top = px(x0, y0, c) * (1.0 - fx) + px(x1, y0, c) * fx;
                let bot = px(x0, y1, c) * (1.0 - fx) + px(x1, y1, c) * fx;
                let v = top * (1.0 - fy) + bot * fy;
                data[((dy * dst_w + dx) * 4) as usize + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Pixmap {
        width: dst_w,
        height: dst_h,
        data,
    }
}

fn cold_grid_time(bytes: &[u8], box_side: u32, strategy: ThumbnailStrategy) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..COLD_REPS {
        let doc = Document::from_bytes(bytes.to_vec()).expect("parse synthesized bundle");
        let t = Instant::now();
        let _ = doc.thumbnails_with_strategy(box_side, box_side, strategy);
        total += t.elapsed();
    }
    total / COLD_REPS
}

fn main() {
    let Some(bytes) = synthesize_th44_color_bundle() else {
        eprintln!(
            "colorbook.djvu not found or synth failed — run from the repo root with the corpus checked out"
        );
        std::process::exit(1);
    };
    let doc = Document::from_bytes(bytes.clone()).expect("parse synthesized bundle");
    let n = doc.page_count();
    println!(
        "Synthesized {n}-page colour-layered TH44 bundle ({} bytes)\n",
        bytes.len()
    );

    println!("== Speed: cold thumbnails() grid build, Th44Only vs RenderOnly ==");
    let th44 = cold_grid_time(&bytes, 128, ThumbnailStrategy::Th44Only);
    let render = cold_grid_time(&bytes, 128, ThumbnailStrategy::RenderOnly);
    println!(
        "  128px grid, {n} pages: TH44Only {:.3?}  RenderOnly {:.3?}  speedup {:.1}x",
        th44,
        render,
        render.as_secs_f64() / th44.as_secs_f64()
    );

    println!("\n== Quality: SSIM(Th44Only, RenderOnly) at grid box size ==");
    for box_side in [48u32, 64, 96, 128, 256] {
        let mut ssim_sum = 0.0f64;
        let mut cnt = 0u32;
        for i in 0..n {
            let page = doc.page(i).unwrap();
            let render = page
                .thumbnail_with_strategy(box_side, box_side, ThumbnailStrategy::RenderOnly)
                .unwrap();
            let th44 = page
                .thumbnail_with_strategy(box_side, box_side, ThumbnailStrategy::Th44Only)
                .unwrap();
            let th44_aligned = resample_bilinear(&th44, render.width, render.height);
            ssim_sum += quality::ssim(&th44_aligned, &render);
            cnt += 1;
        }
        println!(
            "  box {box_side:>4}px: mean SSIM over {cnt} pages = {:.4}",
            ssim_sum / cnt as f64
        );
    }
    println!(
        "\n(Reference range from D5_TH44_PREVIEW on a single colorbook page: 0.50-0.68.\n\
         A TH44 thumbnail is baked in at encode time as a separately-lossy ~128px preview,\n\
         not a faithful downscale of the real page — see Document::thumbnails docs.)"
    );
}
