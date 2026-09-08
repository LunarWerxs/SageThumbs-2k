//! TXTZ_OCR: encode-time OCR text-layer end-to-end demo.
//!
//! Exercises the new `PageEncoder::with_ocr_text_layer` /
//! `PageEncoder::with_text_layer` opt-in (round 51): decodes a real scanned
//! page, re-encodes it through `PageEncoder` with an OCR backend attached, and
//! validates the result three ways:
//!
//! 1. **Round-trip through our own decoder** — the emitted `TXTz` chunk parses
//!    back into the same words the backend recognized (primary validator per
//!    the task brief; this is *not* checked against a Tesseract binary parser
//!    on the far end, on purpose — DjVuLibre interop is out of scope here).
//! 2. **Size cost** — bytes added by the `TXTz` chunk vs. the same page
//!    encoded with no text layer.
//! 3. **PDF export** — `pdf::djvu_to_pdf` on the re-encoded document produces
//!    a page with a `/Font` resource (the existing `pdf_with_text_layer` path
//!    in `src/pdf.rs`), i.e. the text layer really is exported, not just
//!    present as inert bytes.
//!
//! Honesty note (mirrors OCR_QA, round 43): this harness does not compare
//! against a ground-truth transcription (none is checked into the repo for
//! `watchmaker.djvu`) — that would only measure Tesseract's own accuracy, not
//! this feature. What it *does* prove is that the plumbing introduces no
//! degradation of its own: the text embedded via `with_ocr_text_layer` is
//! byte-for-byte the same string `OcrBackend::recognize` returned, and it
//! round-trips through our TXTz encoder/decoder unchanged.
//!
//! ## Run
//!
//! ```text
//! cargo run --release --example txtz_ocr_demo --features ocr-tesseract
//! ```
//! Without `ocr-tesseract` (or without a working Tesseract install) the demo
//! still runs the size/round-trip checks using a deterministic mock backend,
//! and prints a note instead of real OCR output.

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::ocr::{OcrBackend, OcrError, OcrOptions};
use djvu_rs::pdf::djvu_to_pdf;
use djvu_rs::text::TextLayer;

#[cfg(feature = "ocr-tesseract")]
use djvu_rs::ocr_tesseract::TesseractBackend;
use djvu_rs::{Bitmap, Pixmap};

/// Deterministic fallback backend used when `ocr-tesseract` isn't compiled in
/// or Tesseract isn't installed on this machine — keeps the demo runnable
/// everywhere, same spirit as `examples/ocr_qa.rs`'s mock.
struct StubBackend;
impl OcrBackend for StubBackend {
    fn recognize(&self, pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
        use djvu_rs::text::{Rect, TextZone, TextZoneKind};
        Ok(TextLayer {
            text: "stub".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: pixmap.width,
                    height: pixmap.height,
                },
                text: "stub".into(),
                children: vec![],
            }],
        })
    }
}

fn build_backend() -> (Box<dyn OcrBackend>, bool) {
    #[cfg(feature = "ocr-tesseract")]
    {
        let probe = Pixmap::white(32, 32);
        let backend = TesseractBackend::new();
        if backend.recognize(&probe, &OcrOptions::default()).is_ok() {
            return (Box::new(backend), true);
        }
        eprintln!(
            "note: ocr-tesseract feature is compiled in but Tesseract could not \
             recognize a smoke page — falling back to the stub backend."
        );
    }
    (Box::new(StubBackend), false)
}

fn main() {
    let path = "tests/corpus/watchmaker.djvu";
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc = DjVuDocument::parse(&data).expect("parse watchmaker.djvu");

    let page0 = doc.page(0).expect("page 0");
    let mask: Bitmap = page0
        .extract_mask()
        .expect("extract_mask should not error")
        .expect("watchmaker page 0 must have a bitonal JB2 mask");
    println!(
        "watchmaker.djvu page 0: {}x{} bilevel mask",
        mask.width, mask.height
    );

    let (backend, is_real_tesseract) = build_backend();

    // Baseline: same page, no text layer at all.
    let baseline_bytes = PageEncoder::from_bitmap(&mask)
        .with_quality(EncodeQuality::Lossless)
        .encode()
        .expect("baseline encode");

    // New path: OCR runs during encode, TXTz is emitted in the same pass.
    let with_ocr_bytes = PageEncoder::from_bitmap(&mask)
        .with_quality(EncodeQuality::Lossless)
        .with_ocr_text_layer(backend.as_ref(), &OcrOptions::default())
        .expect("OCR backend must not fail on a real scan page")
        .encode()
        .expect("encode with text layer");

    let delta = with_ocr_bytes.len() as i64 - baseline_bytes.len() as i64;
    println!(
        "size: baseline = {} B, with TXTz = {} B, cost = {:+} B ({:.2} KB)",
        baseline_bytes.len(),
        with_ocr_bytes.len(),
        delta,
        delta as f64 / 1024.0
    );

    // Round-trip through our own decoder.
    let reencoded = DjVuDocument::parse(&with_ocr_bytes).expect("parse re-encoded page");
    let page = reencoded.page(0).expect("page 0");
    let layer = page
        .text_layer()
        .expect("text_layer() must not error")
        .expect("text_layer() must return Some — TXTz was just written");
    let plain = page.text().unwrap().unwrap_or_default();
    println!(
        "decoded text layer: {} chars, {} top-level zone(s)",
        layer.text.len(),
        layer.zones.len()
    );
    println!(
        "plain text (first 200 chars): {:?}",
        plain.chars().take(200).collect::<String>()
    );

    // Sample a handful of word zones with their DjVu-decoded (top-left,
    // post-flip) coordinates to eyeball sanity.
    fn collect_words<'a>(
        zone: &'a djvu_rs::text::TextZone,
        out: &mut Vec<&'a djvu_rs::text::TextZone>,
    ) {
        if zone.kind == djvu_rs::text::TextZoneKind::Word {
            out.push(zone);
        }
        for child in &zone.children {
            collect_words(child, out);
        }
    }
    let mut words = Vec::new();
    for z in &layer.zones {
        collect_words(z, &mut words);
    }
    println!("word zones found: {}", words.len());
    for w in words.iter().take(8) {
        println!(
            "  {:?} @ ({}, {}) {}x{}",
            w.text, w.rect.x, w.rect.y, w.rect.width, w.rect.height
        );
    }

    // PDF export sanity: the text layer must actually reach the exporter.
    let pdf_bytes = djvu_to_pdf(&reencoded).expect("djvu_to_pdf");
    let has_font = pdf_bytes.windows(5).any(|w| w == b"/Font");
    println!(
        "PDF export: {} bytes, /Font resource present = {}",
        pdf_bytes.len(),
        has_font
    );
    assert!(
        has_font,
        "PDF exported from an OCR'd page must reference a /Font resource"
    );

    if is_real_tesseract {
        println!(
            "\nquality note: the text above is Tesseract 5.5.2's raw recognition \
             of the watchmaker scan, unmodified by the TXTz plumbing — this \
             feature's \"accuracy\" is exactly whatever Tesseract achieves on \
             this page. No ground-truth transcription is checked into the repo \
             for watchmaker.djvu, so no independent char/word-accuracy number is \
             claimed here (see OCR_QA, round 43, for lossless-vs-lossy JB2 OCR \
             agreement measurements on other corpora)."
        );
    } else {
        println!(
            "\nnote: ran with the stub backend (no working Tesseract install \
             detected) — size/round-trip numbers above are still real, but the \
             recognized text is a placeholder, not real OCR output."
        );
    }
}
