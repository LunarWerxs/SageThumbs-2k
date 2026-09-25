//! Full neural OCR pipeline (#693): DBNet detection + CTC recognition
//! assembled into a [`TextLayer`].
//!
//! [`NeuralOcrBackend`] is the CLI-facing composition of the pinned-manifest
//! pipeline: [`TextDetector`] finds line boxes in page coordinates,
//! [`TextRecognizer`] decodes each box against the pinned Cyrillic
//! dictionary, and [`assemble_text_layer`] builds the `page → line → word`
//! zone tree.
//!
//! Word rectangles are a **proportional heuristic**: the recognizer emits one
//! string per line (with spaces), not per-character geometry, so each word's
//! rect is its character span scaled across the line box. Consumers already
//! must tolerate best-effort word geometry (see the granularity note on
//! [`OcrBackend::recognize`]).
//!
//! [`OcrOptions`] is advisory here and ignored: the language is fixed by the
//! pinned dictionary, and the detector scales with the page extent, so `dpi`
//! has no effect.

use std::path::Path;
use std::sync::Mutex;

use crate::ocr::{OcrBackend, OcrError, OcrOptions};
use crate::ocr_onnx::detect::TextDetector;
use crate::ocr_onnx::recognize::{LineText, TextRecognizer};
use crate::pixmap::Pixmap;
use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

/// Detection + recognition engines behind one lock: both keep interior
/// plan caches and need `&mut`, while [`OcrBackend::recognize`] takes `&self`.
struct Engines {
    detector: TextDetector,
    recognizer: TextRecognizer,
}

/// Neural OCR backend combining pinned DBNet detection and CTC recognition.
///
/// Loading verifies every model artifact against the embedded manifest
/// (size + SHA-256) before use; missing or tampered files are typed errors.
pub struct NeuralOcrBackend {
    inner: Mutex<Engines>,
}

impl NeuralOcrBackend {
    /// Load both pipeline models from `models_dir`, verifying each against
    /// the embedded manifest.
    pub fn load(models_dir: &Path) -> Result<Self, OcrError> {
        Ok(Self {
            inner: Mutex::new(Engines {
                detector: TextDetector::load(models_dir)?,
                recognizer: TextRecognizer::load(models_dir)?,
            }),
        })
    }

    /// Load from the default models directory (`DJVU_OCR_MODELS_DIR` or
    /// `models/ocr`).
    pub fn load_default() -> Result<Self, OcrError> {
        Self::load(&crate::ocr_onnx::manifest::default_models_dir())
    }
}

impl OcrBackend for NeuralOcrBackend {
    fn recognize(&self, pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
        let mut engines = self
            .inner
            .lock()
            .map_err(|_| OcrError::RecognitionFailed("neural OCR engines poisoned".into()))?;
        let boxes = engines.detector.detect(pixmap)?;
        let mut lines = Vec::with_capacity(boxes.len());
        for rect in boxes {
            let line = engines.recognizer.recognize_line(pixmap, &rect)?;
            if !line.text.trim().is_empty() {
                lines.push((rect, line));
            }
        }
        Ok(assemble_text_layer(pixmap.width, pixmap.height, &lines))
    }
}

/// Assemble recognized lines into a [`TextLayer`].
///
/// Pure function (exposed for unit tests). Lines whose text is blank are
/// dropped; the rest keep the order given (the detector already sorts boxes
/// top-to-bottom, then left-to-right). Always emits exactly one page-level
/// zone — the minimum granularity [`OcrBackend::recognize`] guarantees —
/// even when no text was found.
pub fn assemble_text_layer(page_w: u32, page_h: u32, lines: &[(Rect, LineText)]) -> TextLayer {
    let mut page_text = String::new();
    let mut line_zones = Vec::new();
    for (rect, line) in lines {
        if line.text.trim().is_empty() {
            continue;
        }
        if !page_text.is_empty() {
            page_text.push('\n');
        }
        page_text.push_str(&line.text);
        line_zones.push(TextZone {
            kind: TextZoneKind::Line,
            rect: rect.clone(),
            text: line.text.clone(),
            children: word_zones(&line.text, rect),
        });
    }
    let zones = vec![TextZone {
        kind: TextZoneKind::Page,
        rect: Rect {
            x: 0,
            y: 0,
            width: page_w,
            height: page_h,
        },
        text: page_text.clone(),
        children: line_zones,
    }];
    TextLayer {
        text: page_text,
        zones,
    }
}

/// Split a line's text into word zones with proportional rectangles.
///
/// Pure function (exposed for unit tests). Words are maximal runs of
/// non-space characters; each word's rect is its character span `[start,
/// end)` mapped linearly across the line box (the recognizer yields no
/// per-character geometry). Every word rect is at least 1 pixel wide and
/// spans the full line height.
pub fn word_zones(text: &str, line: &Rect) -> Vec<TextZone> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Vec::new();
    }
    let x_at = |char_idx: usize| -> u32 {
        (u64::from(line.x) + u64::from(line.width) * char_idx as u64 / total as u64) as u32
    };
    let mut zones = Vec::new();
    let mut start = None;
    for (i, &c) in chars.iter().chain(std::iter::once(&' ')).enumerate() {
        match (c == ' ', start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                let x0 = x_at(s);
                let x1 = x_at(i).max(x0 + 1);
                zones.push(TextZone {
                    kind: TextZoneKind::Word,
                    rect: Rect {
                        x: x0,
                        y: line.y,
                        width: x1 - x0,
                        height: line.height,
                    },
                    text: chars[s..i].iter().collect(),
                    children: Vec::new(),
                });
                start = None;
            }
            _ => {}
        }
    }
    zones
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr_onnx::manifest::{
        DET_MODEL, ModelManifest, REC_CYRILLIC_CONFIG, REC_CYRILLIC_MODEL, default_models_dir,
    };

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn line(text: &str, confidence: f32) -> LineText {
        LineText {
            text: text.to_string(),
            confidence,
        }
    }

    #[test]
    fn word_zones_split_proportionally() {
        // "ab cd" = 5 chars over width 100: "ab" spans [0,2), "cd" spans [3,5).
        let zones = word_zones("ab cd", &rect(10, 20, 100, 16));
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].text, "ab");
        assert_eq!(zones[0].rect, rect(10, 20, 40, 16));
        assert_eq!(zones[1].text, "cd");
        assert_eq!(zones[1].rect, rect(70, 20, 40, 16));
        assert!(zones.iter().all(|z| z.kind == TextZoneKind::Word));
    }

    #[test]
    fn word_zones_skip_repeated_spaces() {
        let zones = word_zones("  а  б  ", &rect(0, 0, 80, 10));
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].text, "а");
        assert_eq!(zones[1].text, "б");
    }

    #[test]
    fn word_zones_single_word_spans_line() {
        let zones = word_zones("слово", &rect(5, 7, 50, 12));
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].rect, rect(5, 7, 50, 12));
    }

    #[test]
    fn word_zones_empty_text_yields_nothing() {
        assert!(word_zones("", &rect(0, 0, 100, 10)).is_empty());
    }

    #[test]
    fn word_zones_never_zero_width() {
        // Width 1 line: both words still get ≥ 1 px.
        let zones = word_zones("a b", &rect(0, 0, 1, 10));
        assert_eq!(zones.len(), 2);
        assert!(zones.iter().all(|z| z.rect.width >= 1));
    }

    #[test]
    fn assemble_builds_page_line_word_tree() {
        let lines = vec![
            (rect(10, 10, 100, 16), line("первая строка", 0.9)),
            (rect(10, 40, 80, 16), line("вторая", 0.8)),
        ];
        let layer = assemble_text_layer(640, 480, &lines);
        assert_eq!(layer.text, "первая строка\nвторая");
        assert_eq!(layer.zones.len(), 1);
        let page = &layer.zones[0];
        assert_eq!(page.kind, TextZoneKind::Page);
        assert_eq!(page.rect, rect(0, 0, 640, 480));
        assert_eq!(page.text, layer.text);
        assert_eq!(page.children.len(), 2);
        assert_eq!(page.children[0].kind, TextZoneKind::Line);
        assert_eq!(page.children[0].children.len(), 2); // "первая", "строка"
        assert_eq!(page.children[1].children.len(), 1); // "вторая"
    }

    #[test]
    fn assemble_drops_blank_lines() {
        let lines = vec![
            (rect(0, 0, 50, 10), line("   ", 0.5)),
            (rect(0, 20, 50, 10), line("текст", 0.9)),
        ];
        let layer = assemble_text_layer(100, 100, &lines);
        assert_eq!(layer.text, "текст");
        assert_eq!(layer.zones[0].children.len(), 1);
    }

    #[test]
    fn assemble_empty_input_keeps_page_zone() {
        let layer = assemble_text_layer(200, 300, &[]);
        assert!(layer.text.is_empty());
        assert_eq!(layer.zones.len(), 1);
        assert_eq!(layer.zones[0].kind, TextZoneKind::Page);
        assert!(layer.zones[0].children.is_empty());
    }

    // ── Model-gated tests (skip silently when weights are absent) ────────────

    fn backend_if_models_present() -> Option<NeuralOcrBackend> {
        let dir = default_models_dir();
        let manifest = ModelManifest::builtin().unwrap();
        for name in [DET_MODEL, REC_CYRILLIC_MODEL, REC_CYRILLIC_CONFIG] {
            if !manifest.entry(name).unwrap().path_in(&dir).exists() {
                return None; // weights not fetched — skip (CI fetches them in the ocr-onnx job)
            }
        }
        Some(NeuralOcrBackend::load(&dir).expect("pinned artifacts must verify and load"))
    }

    #[test]
    fn blank_page_yields_empty_layer() {
        let Some(backend) = backend_if_models_present() else {
            return;
        };
        let page = Pixmap::white(400, 300);
        let layer = backend
            .recognize(&page, &OcrOptions::default())
            .expect("pipeline on a blank page");
        assert!(layer.text.is_empty());
        assert_eq!(layer.zones.len(), 1);
        assert!(layer.zones[0].children.is_empty());
    }

    #[test]
    fn text_like_page_yields_well_formed_layer() {
        let Some(backend) = backend_if_models_present() else {
            return;
        };
        // Text-like pattern: rows of short dark dashes on white (same as the
        // detector test). Content is not asserted — only structure.
        let mut page = Pixmap::white(640, 480);
        for row in 0..8 {
            let y0 = 40 + row * 50;
            for seg in 0..12 {
                let x0 = 30 + seg * 48;
                for y in y0..y0 + 14 {
                    for x in x0..x0 + 34 {
                        let i = (y * 640 + x) * 4;
                        page.data[i..i + 3].fill(20);
                    }
                }
            }
        }
        let layer = backend
            .recognize(&page, &OcrOptions::default())
            .expect("pipeline on a synthetic page");
        assert_eq!(layer.zones.len(), 1);
        let page_zone = &layer.zones[0];
        assert_eq!(page_zone.kind, TextZoneKind::Page);
        let mut collected = Vec::new();
        for line in &page_zone.children {
            assert_eq!(line.kind, TextZoneKind::Line);
            assert!(line.rect.x + line.rect.width <= 640);
            assert!(line.rect.y + line.rect.height <= 480);
            for word in &line.children {
                assert_eq!(word.kind, TextZoneKind::Word);
                assert!(word.rect.x >= line.rect.x);
                assert!(word.rect.x + word.rect.width <= line.rect.x + line.rect.width);
            }
            collected.push(line.text.clone());
        }
        assert_eq!(layer.text, collected.join("\n"));
    }
}
