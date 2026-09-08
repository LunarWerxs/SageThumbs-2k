//! OCR-calibrated lossy-preset grid (#572): sweep
//! `lossy_threshold × despeckle` per content class and record Sjbz bytes +
//! the *minimum* per-page OCR char agreement vs the lossless mask — the
//! preset derivation wants "100% agreement across ALL pages", not an average.
//!
//! ```sh
//! OCR_PAGES=2 cargo run --release --features ocr-tesseract --example ocr_preset_grid
//! ```

use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::ocr::{OcrBackend, OcrOptions};
use djvu_rs::ocr_tesseract::TesseractBackend;
use djvu_rs::{Bitmap, Pixmap};

fn mask_to_pixmap(bm: &Bitmap) -> Pixmap {
    let mut pm = Pixmap::white(bm.width, bm.height);
    for y in 0..bm.height {
        for x in 0..bm.width {
            if bm.get(x, y) {
                let i = ((y * bm.width + x) * 4) as usize;
                pm.data[i] = 0;
                pm.data[i + 1] = 0;
                pm.data[i + 2] = 0;
            }
        }
    }
    pm
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
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

fn char_agreement(a: &str, b: &str) -> f64 {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (a, b) = (norm(a), norm(b));
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let denom = ac.len().max(bc.len()).max(1);
    1.0 - levenshtein(&ac, &bc) as f64 / denom as f64
}

fn ocr(backend: &TesseractBackend, bm: &Bitmap) -> Option<String> {
    backend
        .recognize(&mask_to_pixmap(bm), &OcrOptions::default())
        .ok()
        .map(|l| l.text)
}

fn main() {
    let pages: usize = std::env::var("OCR_PAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let thresholds = [0.02f32, 0.04, 0.06, 0.08, 0.10];
    let despeckles: [Option<u32>; 4] = [None, Some(4), Some(8), Some(16)];

    let backend = TesseractBackend::new();

    for (class, path) in [
        ("text", "tests/corpus/watchmaker.djvu"),
        ("scan-600dpi", "tests/corpus/pathogenic_bacteria_1896.djvu"),
    ] {
        let data = std::fs::read(path).unwrap();
        let doc = djvu_rs::DjVuDocument::parse(&data).unwrap();
        let mut masks = Vec::new();
        for i in 0..doc.page_count() {
            if masks.len() >= pages {
                break;
            }
            if let Ok(page) = doc.page(i)
                && let Ok(Some(m)) = page.extract_mask()
                && m.width > 400
            {
                masks.push(m);
            }
        }
        let base_bytes: usize = masks
            .iter()
            .map(|m| encode_jb2_dict_with_options(m, &[], &Jb2EncodeOptions::default()).len())
            .sum();
        let base_texts: Vec<String> = masks
            .iter()
            .map(|m| ocr(&backend, m).unwrap_or_default())
            .collect();
        println!(
            "== class {class} ({path}, {} pages, lossless Sjbz {base_bytes} B) ==",
            masks.len()
        );

        for &despeckle in &despeckles {
            for &t in &thresholds {
                let opts = Jb2EncodeOptions {
                    lossy_threshold: t,
                    despeckle,
                    ..Jb2EncodeOptions::default()
                };
                let mut total = 0usize;
                let mut min_agree = 1.0f64;
                for (m, base_text) in masks.iter().zip(&base_texts) {
                    let enc = encode_jb2_dict_with_options(m, &[], &opts);
                    total += enc.len();
                    let dec = djvu_rs::jb2::decode(&enc, None).unwrap();
                    let text = ocr(&backend, &dec).unwrap_or_default();
                    min_agree = min_agree.min(char_agreement(base_text, &text));
                }
                println!(
                    "  t={:>4.0}% despeckle={:<4} Sjbz {total:>7} B ({:+6.2}%)  min-char-agree {:7.3}%",
                    t * 100.0,
                    despeckle.map(|d| d.to_string()).unwrap_or("off".into()),
                    100.0 * (total as f64 - base_bytes as f64) / base_bytes as f64,
                    100.0 * min_agree,
                );
            }
        }
    }
}
