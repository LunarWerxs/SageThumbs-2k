//! PGO training driver.
//!
//! Exercises the hot decode/render paths over a broad spread of the corpus so a
//! profile-guided rebuild can lay out the ZP arithmetic decoder, IW44 IDWT, JB2
//! symbol decode and the compositor for the branch pattern of real documents.
//!
//! Not a benchmark — it runs each workload a fixed number of times purely to
//! accumulate representative profile counters. Run under `-Cprofile-generate`;
//! see the `pgo` target in the `Makefile`.

// The RenderOptions literals still name the deprecated `scale` field (ignored by
// the pipeline; kept until the field is removed), same as the bench harnesses.
#![allow(deprecated)]

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{self, RenderOptions, Resampling, UserRotation};
use std::path::PathBuf;

fn assets() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}
fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

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

/// Render one document at several scales (fresh parse each time so the cold
/// ZP-decode + IDWT paths are exercised, not just warm cache hits).
fn train_render(data: &[u8], scales: &[f32], resampling: Resampling, reps: usize) {
    for _ in 0..reps {
        let Ok(doc) = DjVuDocument::parse(data) else {
            return;
        };
        let Ok(page) = doc.page(0) else { return };
        let (pw, ph) = (page.width() as f32, page.height() as f32);
        for &s in scales {
            let w = ((pw * s).round() as u32).max(1);
            let h = ((ph * s).round() as u32).max(1);
            let _ = djvu_render::render_pixmap(page, &opts(w, h, resampling));
        }
    }
}

/// Decode the JB2 masks of the first `n` pages of a bundled document (fresh
/// parse each rep) — exercises shared-dictionary JB2 decode.
fn train_masks(data: &[u8], n: usize, reps: usize) {
    for _ in 0..reps {
        let Ok(doc) = DjVuDocument::parse(data) else {
            return;
        };
        for i in 0..n.min(doc.page_count()) {
            if let Ok(page) = doc.page(i) {
                let _ = page.extract_mask();
            }
        }
    }
}

fn read(path: PathBuf) -> Option<Vec<u8>> {
    std::fs::read(&path).ok()
}

fn main() {
    let scales = [0.375_f32, 0.5, 1.0, 1.5];

    // Bilevel JB2 (ZP + symbol decode + bilevel compositor).
    if let Some(d) = read(corpus().join("cable_1973_100133.djvu")) {
        train_render(&d, &scales, Resampling::Bilinear, 4);
        train_render(&d, &[0.5], Resampling::Lanczos3, 2);
    }
    // Color IW44 + JB2 mask (IDWT + YCbCr + color compositor).
    if let Some(d) = read(corpus().join("watchmaker.djvu")) {
        train_render(&d, &scales, Resampling::Bilinear, 4);
    }
    // Large color at heavy downscale (sub=4 compact IDWT path).
    if let Some(d) = read(assets().join("colorbook.djvu")) {
        train_render(&d, &[0.375, 0.5], Resampling::Bilinear, 2);
    }
    // FGbz palette page (indexed mask + blit map).
    if let Some(d) = read(assets().join("navm_fgbz.djvu")) {
        train_render(&d, &[1.0, 0.5], Resampling::Bilinear, 2);
    }
    // Large scanned bilevel page (BZZ + JB2 at high DPI).
    if let Some(d) = read(assets().join("big-scanned-page.djvu")) {
        train_render(&d, &[1.0, 0.5], Resampling::Bilinear, 2);
    }
    // Multi-page shared-dictionary JB2 decode.
    if let Some(d) = read(assets().join("DjVu3Spec_bundled.djvu")) {
        train_masks(&d, 30, 3);
    }
    // A few pages of the big multi-page mixed document (INCL + BZZ DIRM).
    if let Some(d) = read(corpus().join("pathogenic_bacteria_1896.djvu")) {
        train_masks(&d, 8, 2);
    }

    println!("pgo_train done");
}
