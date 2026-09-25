//! Standalone release-mode workload for profiling the high-DPI render path
//! with samply. Not part of any CI gate.
//!
//! Usage: `profile_high_dpi <boy600|color|bilevel> [iterations]`
#![allow(deprecated)]

use std::path::PathBuf;

fn assets_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "boy600".to_string());
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);

    match mode.as_str() {
        "boy600" => {
            let path = assets_path().join("boy.djvu");
            let data = std::fs::read(&path).expect("boy.djvu");
            let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
            let page = doc.page(0).expect("page 0");
            let native_w = page.width() as u32;
            let native_h = page.height() as u32;
            let native_dpi = page.dpi() as f32;
            let scale = 600.0f32 / native_dpi;
            let w = ((native_w as f32 * scale).round() as u32).max(1);
            let h = ((native_h as f32 * scale).round() as u32).max(1);
            let opts = djvu_rs::djvu_render::RenderOptions {
                width: w,
                height: h,
                scale,
                bold: 0,
                aa: false,
                rotation: djvu_rs::djvu_render::UserRotation::None,
                permissive: false,
                resampling: djvu_rs::djvu_render::Resampling::Bilinear,
                mask_aa: false,
            };
            eprintln!("boy.djvu @ 600dpi -> {w}x{h}, {iters} iters");
            for _ in 0..iters {
                let _ = std::hint::black_box(djvu_rs::djvu_render::render_pixmap(page, &opts));
            }
        }
        "color" => {
            let path = corpus_path().join("watchmaker.djvu");
            let data = std::fs::read(&path).expect("watchmaker.djvu");
            let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
            let page = doc.page(0).expect("page 0");
            let opts = djvu_rs::djvu_render::RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                scale: 1.0,
                bold: 0,
                aa: false,
                rotation: djvu_rs::djvu_render::UserRotation::None,
                permissive: false,
                resampling: djvu_rs::djvu_render::Resampling::Bilinear,
                mask_aa: false,
            };
            eprintln!(
                "watchmaker.djvu native {}x{}, {iters} iters",
                page.width(),
                page.height()
            );
            for _ in 0..iters {
                let _ = std::hint::black_box(djvu_rs::djvu_render::render_pixmap(page, &opts));
            }
        }
        "bilevel" => {
            let path = corpus_path().join("cable_1973_100133.djvu");
            let data = std::fs::read(&path).expect("cable_1973_100133.djvu");
            let doc = djvu_rs::DjVuDocument::parse(&data).expect("parse");
            let page = doc.page(0).expect("page 0");
            let opts = djvu_rs::djvu_render::RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                scale: 1.0,
                bold: 0,
                aa: false,
                rotation: djvu_rs::djvu_render::UserRotation::None,
                permissive: false,
                resampling: djvu_rs::djvu_render::Resampling::Bilinear,
                mask_aa: false,
            };
            eprintln!(
                "cable_1973_100133.djvu native {}x{}, {iters} iters",
                page.width(),
                page.height()
            );
            for _ in 0..iters {
                let _ = std::hint::black_box(djvu_rs::djvu_render::render_pixmap(page, &opts));
            }
        }
        other => panic!("unknown mode {other}, expected boy600|color|bilevel"),
    }
}
