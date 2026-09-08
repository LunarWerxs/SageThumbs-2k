//! Generation-loss study (#601): drift across repeated decode → re-encode
//! cycles, per encode profile.
//!
//! For each fixture and profile, run N generations of
//! render-at-native-resolution → re-encode → parse, recording per generation:
//! quality vs generation 0 and vs the previous generation
//! (`quality::compare_color`), output bytes, and the Sjbz record count proxy
//! (mask instability shows up there first).
//!
//! ```sh
//! cargo run --release --example generation_loss -- [gens] [fixture.djvu ...]
//! ```

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::quality::compare_color;

fn render_native(doc: &DjVuDocument) -> Pixmap {
    let page = doc.page(0).unwrap();
    render_pixmap(
        page,
        &RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..Default::default()
        },
    )
    .unwrap()
}

/// Total Sjbz payload bytes of page 0 — a cheap mask-instability proxy.
fn sjbz_bytes(doc: &DjVuDocument) -> usize {
    doc.page(0)
        .unwrap()
        .find_chunk(b"Sjbz")
        .map(|c| c.len())
        .unwrap_or(0)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let gens: usize = args
        .first()
        .and_then(|a| a.parse().ok())
        .inspect(|_| {
            args.remove(0);
        })
        .unwrap_or(5);
    let fixtures = if args.is_empty() {
        vec![
            "tests/corpus/watchmaker.djvu".to_string(),
            "tests/fixtures/colorbook.djvu".to_string(),
            "tests/corpus/cable_1973_100133.djvu".to_string(),
        ]
    } else {
        args
    };

    for f in &fixtures {
        let data = std::fs::read(f).unwrap_or_else(|_| panic!("read {f}"));
        let doc0 = DjVuDocument::parse(&data).unwrap();
        let dpi = doc0.page(0).unwrap().dpi();

        for quality in [EncodeQuality::Quality, EncodeQuality::Archival] {
            println!("== {f} — {quality:?} ({gens} generations) ==");
            let gen0 = render_native(&doc0);
            let mut prev_render = gen0.clone();
            let mut current = data.clone();
            for g in 1..=gens {
                let doc = DjVuDocument::parse(&current).unwrap();
                let pm = render_native(&doc);
                let encoder = PageEncoder::from_pixmap(&pm)
                    .with_quality(quality)
                    .with_dpi(dpi as u16);
                let bytes = match encoder.encode() {
                    Ok(b) => b,
                    Err(e) => {
                        println!("  gen{g}: encode failed: {e}");
                        break;
                    }
                };
                let doc_next = DjVuDocument::parse(&bytes).unwrap();
                let render_next = render_native(&doc_next);
                let vs0 = compare_color(&gen0, &render_next);
                let vsp = compare_color(&prev_render, &render_next);
                println!(
                    "  gen{g}: {} B sjbz={} | vs gen0: dE {:.3} ssim_y {:.4} | vs prev: dE {:.3} ssim_y {:.4}",
                    bytes.len(),
                    sjbz_bytes(&doc_next),
                    vs0.delta_e_mean,
                    vs0.ssim_y,
                    vsp.delta_e_mean,
                    vsp.ssim_y,
                );
                prev_render = render_next;
                current = bytes;
            }
        }
    }
}
