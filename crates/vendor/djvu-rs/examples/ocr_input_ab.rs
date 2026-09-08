//! OCR input-selection A/B (#603): which input recognizes best —
//! (A) the raw composite render (today's behaviour), (B) our Sauvola
//! segmentation mask, or (C) the document's decoded JB2 mask?
//!
//! Reference = the document's embedded text layer (the original OCR that
//! shipped with the scan). Reports char/word agreement vs that reference and
//! wall-clock per variant.
//!
//! ```sh
//! cargo run --release --features ocr-tesseract --example ocr_input_ab [file ...]
//! ```

use std::time::Instant;

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};
use djvu_rs::ocr::{OcrBackend, OcrOptions};
use djvu_rs::ocr_tesseract::TesseractBackend;
use djvu_rs::segment::{SegmentOptions, segment_page};
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

// Agreement metrics — same formulas as examples/ocr_qa.rs (OCR_QA method).

fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Character-level agreement in `[0.0, 1.0]`: `1 - edit_distance / max_len`.
fn char_agreement(a: &str, b: &str) -> f64 {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let denom = ac.len().max(bc.len()).max(1);
    1.0 - levenshtein(&ac, &bc) as f64 / denom as f64
}

/// Word-level agreement in `[0.0, 1.0]`, same formula over whitespace tokens.
fn word_agreement(a: &str, b: &str) -> f64 {
    let aw: Vec<&str> = a.split_whitespace().collect();
    let bw: Vec<&str> = b.split_whitespace().collect();
    let denom = aw.len().max(bw.len()).max(1);
    1.0 - levenshtein(&aw, &bw) as f64 / denom as f64
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fixtures = if args.is_empty() {
        vec![
            "tests/corpus/watchmaker.djvu".to_string(),
            "tests/corpus/cable_1973_100133.djvu".to_string(),
            "tests/fixtures/malliavin.djvu".to_string(),
            "tests/fixtures/DjVu3Spec_bundled.djvu".to_string(),
        ]
    } else {
        args
    };

    let backend = TesseractBackend::new();

    for f in &fixtures {
        let Ok(data) = std::fs::read(f) else {
            println!("{f}: missing");
            continue;
        };
        let doc = DjVuDocument::parse(&data).unwrap();
        let page = doc.page(0).unwrap();
        let Ok(Some(reference)) = page.text_layer() else {
            println!("{f}: no embedded text layer — skipping");
            continue;
        };
        let reference = normalize(&reference.text);
        if reference.len() < 40 {
            println!("{f}: reference text too short — skipping");
            continue;
        }
        let dpi = page.dpi();
        let opts = OcrOptions {
            dpi: dpi as u32,
            ..Default::default()
        };

        // (A) raw composite render at native resolution.
        let raw = render_pixmap(
            page,
            &RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                ..Default::default()
            },
        )
        .unwrap();
        // (B) Sauvola segmentation mask of the render.
        let seg = segment_page(&raw, &SegmentOptions::default());
        let sauvola_pm = mask_to_pixmap(&seg.mask);
        // (C) the document's decoded JB2 mask.
        let jb2_pm = page.extract_mask().unwrap().map(|m| mask_to_pixmap(&m));

        println!(
            "== {f} (page 0, {dpi} dpi, ref {} chars) ==",
            reference.len()
        );
        let run = |label: &str, pm: &Pixmap| {
            let t0 = Instant::now();
            let out = backend.recognize(pm, &opts);
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            match out {
                Ok(layer) => {
                    let text = normalize(&layer.text);
                    println!(
                        "  {label:<22} {ms:7.0} ms  char-agree {:6.2}%  word-agree {:6.2}%  ({} chars)",
                        100.0 * char_agreement(&reference, &text),
                        100.0 * word_agreement(&reference, &text),
                        text.len()
                    );
                }
                Err(e) => println!("  {label:<22} {ms:7.0} ms  OCR error: {e}"),
            }
        };
        run("A raw render", &raw);
        run("B sauvola mask", &sauvola_pm);
        if let Some(pm) = &jb2_pm {
            run("C decoded JB2 mask", pm);
        } else {
            println!("  C decoded JB2 mask     — page has no JB2 mask");
        }
    }
}
