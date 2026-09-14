//! Allocation-profiling harness (#600): dhat as the global allocator over the
//! canonical scenarios. The output `dhat-heap.json` loads into DHAT's viewer
//! (<https://nnethercote.github.io/dh_view/dh_view.html>); the run also prints
//! dhat's summary (total blocks/bytes, t-gmax) per scenario.
//!
//! One scenario per process run — dhat profiles the whole process, so mixing
//! scenarios would blur attribution:
//!
//! ```sh
//! cargo run --release --features alloc-profile --example alloc_profile -- cold-open
//! cargo run --release --features alloc-profile --example alloc_profile -- warm-render
//! cargo run --release --features alloc-profile --example alloc_profile -- thumbnails
//! cargo run --release --features alloc-profile --example alloc_profile -- encode
//! cargo run --release --features alloc-profile --example alloc_profile -- encode-streaming
//! cargo run --release --features alloc-profile --example alloc_profile -- pdf-export
//! ```
//!
//! Each writes `dhat-<scenario>.json` in the working directory.

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use djvu_rs::Document;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

fn opts_for(page: &djvu_rs::djvu_document::DjVuPage, dpi: f32) -> RenderOptions {
    let scale = dpi / page.dpi().max(1) as f32;
    RenderOptions {
        width: ((page.width() as f32 * scale).round() as u32).max(1),
        height: ((page.height() as f32 * scale).round() as u32).max(1),
        ..Default::default()
    }
}

fn main() {
    let scenario = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "cold-open".to_string());
    let file = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "tests/corpus/watchmaker.djvu".to_string());

    let _profiler = dhat::Profiler::builder()
        .file_name(format!("dhat-{scenario}.json"))
        .build();

    let data = std::fs::read(&file).unwrap();
    match scenario.as_str() {
        // Cold open + first full-page render at 150 dpi.
        "cold-open" => {
            let doc = DjVuDocument::parse(&data).unwrap();
            let page = doc.page(0).unwrap();
            let _ = render_pixmap(page, &opts_for(page, 150.0)).unwrap();
        }
        // One cold render, then 10 warm re-renders (cache-hit path).
        "warm-render" => {
            let doc = DjVuDocument::parse(&data).unwrap();
            let page = doc.page(0).unwrap();
            let opts = opts_for(page, 150.0);
            for _ in 0..11 {
                let _ = render_pixmap(page, &opts).unwrap();
            }
        }
        // 128 px thumbnail sweep over every page.
        "thumbnails" => {
            let doc = Document::from_bytes(data.clone()).unwrap();
            let thumbs = doc.thumbnails(128, 128);
            assert!(thumbs.iter().all(|t| t.is_ok()));
        }
        // Layered quality encode of every page's render (bounded to 12 pages).
        "encode" => {
            let doc = DjVuDocument::parse(&data).unwrap();
            let n = doc.page_count().min(12);
            let mut pixmaps = Vec::with_capacity(n);
            for i in 0..n {
                let page = doc.page(i).unwrap();
                let opts = RenderOptions {
                    width: page.width() as u32,
                    height: page.height() as u32,
                    ..Default::default()
                };
                pixmaps.push(render_pixmap(page, &opts).unwrap());
            }
            let _ = djvu_rs::djvu_encode::encode_djvm_layered_shared(
                &pixmaps,
                djvu_rs::djvu_encode::EncodeQuality::Quality,
                300,
                None,
                2,
            )
            .unwrap();
        }
        // Same scenario as "encode", but through the bounded-window
        // streaming entry point (encoder peak-memory step 4): each page's
        // pixmap is rendered lazily by the closure and dropped after phase 1
        // instead of all 12 being resident in a `Vec<Pixmap>` up front. This
        // is the number that must clear the step-4 keep bar — see
        // `PERF_EXPERIMENTS.md`'s `ENCODE_STREAMING_WINDOW` entry.
        "encode-streaming" => {
            let doc = DjVuDocument::parse(&data).unwrap();
            let n = doc.page_count().min(12);
            let _ = djvu_rs::djvu_encode::encode_djvm_layered_shared_streaming(
                n,
                |i| -> Result<_, djvu_rs::djvu_render::RenderError> {
                    let page = doc.page(i).unwrap();
                    let opts = RenderOptions {
                        width: page.width() as u32,
                        height: page.height() as u32,
                        ..Default::default()
                    };
                    render_pixmap(page, &opts)
                },
                djvu_rs::djvu_encode::EncodeQuality::Quality,
                300,
                None,
                2,
                false,
                None,
                None,
            )
            .unwrap();
        }
        // Whole-document PDF export (streams to a sink).
        #[cfg(feature = "pdf")]
        "pdf-export" => {
            let doc = DjVuDocument::parse(&data).unwrap();
            let mut sink = std::io::sink();
            djvu_rs::pdf::djvu_to_pdf_to_writer(
                &doc,
                &djvu_rs::pdf::PdfOptions::default(),
                &mut sink,
            )
            .unwrap();
        }
        other => {
            eprintln!("unknown scenario: {other}");
            std::process::exit(2);
        }
    }
    // Profiler drop prints the summary and writes dhat-<scenario>.json.
}
