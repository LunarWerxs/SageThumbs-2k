//! Block-classifier corpus validation (#562 follow-up): the tier-2 real
//! mixed-layout fixtures (#558) that the synthetic-only round 97 lacked.
//! Re-encode scenario: render each page's composite at native size, segment
//! with the Quality profile's defaults with and without
//! `SegmentOptions::block_classify`, and compare Sjbz bytes, total encoded
//! bytes, and decoded colour fidelity vs the rendered source.
//!
//! ```sh
//! cargo run --release --example block_classify_corpus
//! ```

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::quality::compare_color;
use djvu_rs::segment::{SegmentOptions, segment_page};

fn render_page(path: &str, page_no: usize) -> Pixmap {
    let data = std::fs::read(path).unwrap();
    let doc = DjVuDocument::parse(&data).unwrap();
    let page = doc.page(page_no).unwrap();
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

fn main() {
    // (path, 0-based page, class label)
    let pages: &[(&str, usize, &str)] = &[
        ("tests/corpus/war_1812.djvu", 0, "newspaper photo page"),
        ("tests/corpus/war_1812.djvu", 2, "newspaper text+photos"),
        ("tests/corpus/goody_twoshoes.djvu", 0, "illustration page"),
        ("tests/corpus/goody_twoshoes.djvu", 1, "book text page"),
        (
            "tests/corpus/map_atlas_sample.djvu",
            0,
            "map line-art (adversarial)",
        ),
        (
            "tests/corpus/chinese_cookbook_sample.djvu",
            3,
            "mixed CJK+photo",
        ),
        (
            "tests/corpus/chinese_cookbook_sample.djvu",
            1,
            "CJK text page",
        ),
        (
            "tests/corpus/pathogenic_bacteria_1896.djvu",
            146,
            "halftone plate (bilevel src)",
        ),
        ("tests/corpus/cable_1973_100133.djvu", 0, "typewriter text"),
    ];

    let base = EncodeQuality::Quality.default_segment_options();
    let cls = SegmentOptions {
        block_classify: true,
        ..base
    };
    // The round-97 follow-up: with photo blocks cleared from the mask their
    // pixels count as background detail, so the #569 adaptive subsample can
    // densify the BG grid exactly where the classifier routed a photo.
    let cls_adaptive = SegmentOptions {
        adaptive_bg_subsample: true,
        ..cls
    };

    for &(path, page_no, label) in pages {
        let pm = render_page(path, page_no);
        println!(
            "== {label}: {path} p{page_no} ({}x{}) ==",
            pm.width, pm.height
        );
        let mut base_mask: Option<djvu_rs::Bitmap> = None;
        for (name, opts) in [
            ("quality", &base),
            ("quality+classify", &cls),
            ("cls+adaptive-bg", &cls_adaptive),
        ] {
            let seg = segment_page(&pm, opts);
            let sjbz = encode_jb2_dict_with_options(&seg.mask, &[], &Jb2EncodeOptions::default());
            let ink: u64 = seg.mask.data.iter().map(|&b| b.count_ones() as u64).sum();
            let enc = PageEncoder::from_pixmap(&pm)
                .with_quality(EncodeQuality::Quality)
                .with_segment_options(*opts)
                .encode()
                .unwrap();
            let doc = DjVuDocument::parse(&enc).unwrap();
            let dec = render_pixmap(
                doc.page(0).unwrap(),
                &RenderOptions {
                    width: pm.width,
                    height: pm.height,
                    ..Default::default()
                },
            )
            .unwrap();
            let q = compare_color(&pm, &dec);
            let identical = match &base_mask {
                None => {
                    base_mask = Some(seg.mask);
                    String::new()
                }
                Some(m) => format!("  mask==base {}", m.data == seg.mask.data),
            };
            println!(
                "  {name:<17} Sjbz {:>8} B  total {:>8} B  ink {ink:>9}  dE_mean {:.3}  ssim_y {:.4}{identical}",
                sjbz.len(),
                enc.len(),
                q.delta_e_mean,
                q.ssim_y,
            );
        }
    }
}
