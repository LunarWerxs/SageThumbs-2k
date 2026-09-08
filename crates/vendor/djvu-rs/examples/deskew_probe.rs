//! Skew-sensitivity probe (#592, step 1): synthetically rotate corpus pages by
//! small angles before binarization and record what skew costs — Sjbz bytes
//! (JB2 dictionary matching degrades on rotated glyphs) and, with the
//! `ocr-tesseract` feature, OCR agreement vs the upright text.
//!
//! ```sh
//! cargo run --release --example deskew_probe
//! cargo run --release --features ocr-tesseract --example deskew_probe
//! ```

use djvu_rs::Pixmap;
use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::segment::{SegmentOptions, segment_page};

/// Rotate `src` by `deg` degrees around its centre (bilinear inverse mapping,
/// white background) — the classic small-angle skew model.
fn rotate_small(src: &Pixmap, deg: f32) -> Pixmap {
    let rad = deg.to_radians();
    let (s, c) = rad.sin_cos();
    let (w, h) = (src.width as i32, src.height as i32);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let mut out = Pixmap::white(src.width, src.height);
    for y in 0..h {
        for x in 0..w {
            // Inverse map: destination (x,y) → source coordinates.
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let sx = c * dx + s * dy + cx;
            let sy = -s * dx + c * dy + cy;
            let x0 = sx.floor();
            let y0 = sy.floor();
            if x0 < 0.0 || y0 < 0.0 || x0 as i32 + 1 >= w || y0 as i32 + 1 >= h {
                continue; // keep white background
            }
            let (fx, fy) = (sx - x0, sy - y0);
            let (x0, y0) = (x0 as usize, y0 as usize);
            let idx = |xx: usize, yy: usize| (yy * src.width as usize + xx) * 4;
            let di = (y as usize * src.width as usize + x as usize) * 4;
            for ch in 0..3 {
                let p00 = src.data[idx(x0, y0) + ch] as f32;
                let p10 = src.data[idx(x0 + 1, y0) + ch] as f32;
                let p01 = src.data[idx(x0, y0 + 1) + ch] as f32;
                let p11 = src.data[idx(x0 + 1, y0 + 1) + ch] as f32;
                let v = p00 * (1.0 - fx) * (1.0 - fy)
                    + p10 * fx * (1.0 - fy)
                    + p01 * (1.0 - fx) * fy
                    + p11 * fx * fy;
                out.data[di + ch] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

#[cfg(feature = "ocr-tesseract")]
mod ocr {
    use djvu_rs::Pixmap;
    use djvu_rs::ocr::{OcrBackend, OcrOptions};
    use djvu_rs::ocr_tesseract::TesseractBackend;

    pub fn text(pm: &Pixmap, dpi: u32) -> Option<String> {
        let backend = TesseractBackend::new();
        let opts = OcrOptions {
            dpi,
            ..Default::default()
        };
        backend
            .recognize(pm, &opts)
            .ok()
            .map(|l| l.text.split_whitespace().collect::<Vec<_>>().join(" "))
    }

    pub fn char_agreement(a: &str, b: &str) -> f64 {
        let ac: Vec<char> = a.chars().collect();
        let bc: Vec<char> = b.chars().collect();
        let denom = ac.len().max(bc.len()).max(1);
        1.0 - levenshtein(&ac, &bc) as f64 / denom as f64
    }

    fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
        let (n, m) = (a.len(), b.len());
        if n == 0 || m == 0 {
            return n.max(m);
        }
        let mut prev: Vec<usize> = (0..=m).collect();
        let mut cur = vec![0usize; m + 1];
        for i in 1..=n {
            cur[0] = i;
            for j in 1..=m {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            }
            core::mem::swap(&mut prev, &mut cur);
        }
        prev[m]
    }
}

fn main() {
    let fixtures = [
        ("tests/corpus/watchmaker.djvu", 0usize),
        ("tests/corpus/cable_1973_100133.djvu", 0),
        ("tests/fixtures/DjVu3Spec_bundled.djvu", 2),
    ];
    let angles = [0.0f32, 0.02, 0.3, 0.5, 1.0, 2.0, 3.0];

    for (path, page_no) in fixtures {
        let Ok(data) = std::fs::read(path) else {
            println!("{path}: missing");
            continue;
        };
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(page_no).unwrap();
        let dpi = page.dpi() as u32;
        let src = render_pixmap(
            page,
            &RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                ..Default::default()
            },
        )
        .unwrap();

        println!("== {path} p{page_no} ({dpi} dpi) ==");
        let mut base_bytes = 0usize;
        #[cfg(feature = "ocr-tesseract")]
        let mut base_text: Option<String> = None;
        for &deg in &angles {
            let rotated = if deg == 0.0 {
                src.clone()
            } else {
                rotate_small(&src, deg)
            };
            let seg = segment_page(&rotated, &SegmentOptions::default());
            let sjbz = encode_jb2_dict_with_options(&seg.mask, &[], &Jb2EncodeOptions::default());
            if deg == 0.0 {
                base_bytes = sjbz.len();
            }
            let delta = 100.0 * (sjbz.len() as f64 - base_bytes as f64) / base_bytes as f64;

            #[cfg(feature = "ocr-tesseract")]
            let ocr_col = {
                let t = ocr::text(&rotated, dpi);
                if deg == 0.0 {
                    base_text = t.clone();
                }
                match (&base_text, &t) {
                    (Some(b), Some(t)) => {
                        format!(
                            "  ocr-char-agree {:6.2}%",
                            100.0 * ocr::char_agreement(b, t)
                        )
                    }
                    _ => "  ocr n/a".to_string(),
                }
            };
            #[cfg(not(feature = "ocr-tesseract"))]
            let ocr_col = String::new();

            // Recovery: opt-in deskew on the skewed input (#592 step 2) —
            // estimate + counter-rotate before binarization.
            let deskew_col = if deg > 0.0 {
                let est = djvu_rs::segment::estimate_skew(&rotated);
                let seg_fixed = segment_page(
                    &rotated,
                    &SegmentOptions {
                        deskew: true,
                        ..SegmentOptions::default()
                    },
                );
                let fixed = encode_jb2_dict_with_options(
                    &seg_fixed.mask,
                    &[],
                    &Jb2EncodeOptions::default(),
                );
                let fdelta = 100.0 * (fixed.len() as f64 - base_bytes as f64) / base_bytes as f64;
                format!(
                    "  est {est:+5.2}°  deskewed {:>8} B ({fdelta:+6.2}%)",
                    fixed.len()
                )
            } else {
                String::new()
            };
            println!(
                "  {deg:>4.1}°  Sjbz {:>8} B ({delta:+6.2}%){ocr_col}{deskew_col}",
                sjbz.len()
            );
        }
    }
}
