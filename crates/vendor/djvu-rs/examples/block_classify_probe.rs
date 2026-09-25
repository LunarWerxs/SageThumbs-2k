//! Block-classifier probe (#562): a synthetic mixed layout (text page with a
//! continuous-tone photo region pasted in) segmented with and without
//! `SegmentOptions::block_classify` — Sjbz bytes, decoded-composite colour
//! fidelity, and the graceful-degradation check on pure-text pages.
//!
//! ```sh
//! cargo run --release --example block_classify_probe
//! ```

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::quality::compare_color;
use djvu_rs::segment::{Binarization, SegmentOptions, segment_page};

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

/// Paste `photo` (scaled 1:1, cropped as needed) into `dst` at (x0, y0).
fn paste(dst: &mut Pixmap, photo: &Pixmap, x0: u32, y0: u32) {
    let w = photo.width.min(dst.width.saturating_sub(x0));
    let h = photo.height.min(dst.height.saturating_sub(y0));
    for y in 0..h {
        for x in 0..w {
            let si = ((y * photo.width + x) * 4) as usize;
            let di = (((y0 + y) * dst.width + (x0 + x)) * 4) as usize;
            dst.data[di..di + 4].copy_from_slice(&photo.data[si..si + 4]);
        }
    }
}

fn sjbz_bytes(mask: &djvu_rs::Bitmap) -> usize {
    encode_jb2_dict_with_options(mask, &[], &Jb2EncodeOptions::default()).len()
}

fn main() {
    // Text base: watchmaker page 0. Photo region: boy.djvu (a real photo)
    // rendered at 3x — 576x768 of continuous tone.
    let text = render_page("tests/corpus/watchmaker.djvu", 0);
    let photo = {
        let data = std::fs::read("tests/fixtures/boy.djvu").unwrap();
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        render_pixmap(
            page,
            &RenderOptions {
                width: page.width() as u32 * 3,
                height: page.height() as u32 * 3,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let (pw, _ph) = (photo.width, photo.height);
    // Darken the patch into the mid-tone range (real photos are rarely this
    // bright): keeps it continuous-tone while giving the fixed threshold
    // something to (wrongly) claim as ink.
    let mut photo = photo;
    for px in photo.data.as_chunks_mut::<4>().0 {
        px[0] = (px[0] as u32 * 6 / 10) as u8;
        px[1] = (px[1] as u32 * 6 / 10) as u8;
        px[2] = (px[2] as u32 * 6 / 10) as u8;
    }

    let mut mixed = text.clone();
    let px = mixed.width - pw - 96;
    paste(&mut mixed, &photo, px, 128);

    // Halftone variant: the same patch through an 8x8 Bayer ordered dither —
    // the newspaper-photo case where per-pixel binarization shreds the region
    // into thousands of dot components.
    let mut halftone = photo.clone();
    const BAYER: [[u8; 8]; 8] = [
        [0, 32, 8, 40, 2, 34, 10, 42],
        [48, 16, 56, 24, 50, 18, 58, 26],
        [12, 44, 4, 36, 14, 46, 6, 38],
        [60, 28, 52, 20, 62, 30, 54, 22],
        [3, 35, 11, 43, 1, 33, 9, 41],
        [51, 19, 59, 27, 49, 17, 57, 25],
        [15, 47, 7, 39, 13, 45, 5, 37],
        [63, 31, 55, 23, 61, 29, 53, 21],
    ];
    for y in 0..halftone.height {
        for x in 0..halftone.width {
            let i = ((y * halftone.width + x) * 4) as usize;
            let p = &halftone.data[i..i + 3];
            let l = (306 * p[0] as u32 + 601 * p[1] as u32 + 117 * p[2] as u32) >> 10;
            let t = (BAYER[(y % 8) as usize][(x % 8) as usize] as u32) * 4 + 2;
            let v = if l < t { 0u8 } else { 255 };
            halftone.data[i] = v;
            halftone.data[i + 1] = v;
            halftone.data[i + 2] = v;
        }
    }
    let mut mixed_ht = text.clone();
    paste(&mut mixed_ht, &halftone, px, 128);

    let fixed = SegmentOptions::default();
    let fixed_cls = SegmentOptions {
        block_classify: true,
        ..fixed
    };
    let sauvola = SegmentOptions {
        binarization: Binarization::Sauvola {
            window: 25,
            k: 0.34,
        },
        ..SegmentOptions::default()
    };
    let classified = SegmentOptions {
        block_classify: true,
        ..sauvola
    };

    for (page_label, mixed) in [("continuous-tone", &mixed), ("halftone", &mixed_ht)] {
        println!(
            "== synthetic mixed page, {page_label} ({}x{}) ==",
            mixed.width, mixed.height
        );
        let classified_adaptive = SegmentOptions {
            block_classify: true,
            adaptive_bg_subsample: true,
            ..sauvola
        };
        for (label, opts) in [
            ("fixed", &fixed),
            ("fixed+classify", &fixed_cls),
            ("sauvola", &sauvola),
            ("sauvola+classify", &classified),
            ("cls+adaptive-bg", &classified_adaptive),
        ] {
            let seg = segment_page(mixed, opts);
            let bytes = sjbz_bytes(&seg.mask);
            let ink_in_photo: u64 = (128..128 + photo.height)
                .map(|y| (px..px + pw).filter(|&x| seg.mask.get(x, y)).count() as u64)
                .sum();
            // Full round-trip fidelity: encode + decode + compare to the source.
            let enc = PageEncoder::from_pixmap(mixed)
                .with_quality(EncodeQuality::Quality)
                .with_segment_options(*opts)
                .encode()
                .unwrap();
            let doc = DjVuDocument::parse(&enc).unwrap();
            let dec = render_pixmap(
                doc.page(0).unwrap(),
                &RenderOptions {
                    width: mixed.width,
                    height: mixed.height,
                    ..Default::default()
                },
            )
            .unwrap();
            let q = compare_color(mixed, &dec);
            // The honest photo-region metric: the decoded patch vs the TRUE
            // continuous-tone photo (for the halftone page the source dots are
            // themselves an artefact — descreening toward the real photo is the
            // desired outcome, not a penalty).
            let crop = |pm: &Pixmap| -> Pixmap {
                let mut out = Pixmap::white(pw, photo.height);
                for y in 0..photo.height {
                    for x in 0..pw {
                        let si = (((y + 128) * pm.width + px + x) * 4) as usize;
                        let di = ((y * pw + x) * 4) as usize;
                        out.data[di..di + 4].copy_from_slice(&pm.data[si..si + 4]);
                    }
                }
                out
            };
            let qp = compare_color(&photo, &crop(&dec));
            println!(
                "  {label:<17} Sjbz {bytes:>7} B  total {:>7} B  dE_mean {:.3}  ssim_y {:.4}  ink@photo {ink_in_photo}  vs-true-photo dE {:.2} ssim {:.3}",
                enc.len(),
                q.delta_e_mean,
                q.ssim_y,
                qp.delta_e_mean,
                qp.ssim_y
            );
        }
    }

    // Graceful degradation: pure text page must be byte-identical.
    println!("== pure-text degradation ==");
    for (path, page_no) in [
        ("tests/corpus/watchmaker.djvu", 0usize),
        ("tests/corpus/cable_1973_100133.djvu", 0),
        ("tests/fixtures/DjVu3Spec_bundled.djvu", 2),
    ] {
        let pm = render_page(path, page_no);
        let a = segment_page(&pm, &sauvola).mask;
        let b = segment_page(&pm, &classified).mask;
        println!("  {path}: mask identical = {}", a.data == b.data);
    }
}
