//! Synthetic metrics corpus (#693 slice 4) — test-only.
//!
//! Renders deterministic ground-truth pages with the pinned PT Sans font
//! (manifest key [`METRICS_FONT`]) and gates the pinned model versions with
//! CER/WER/IoU thresholds. The recorded baseline and the threshold policy
//! live in `docs/ocr-model-metrics.md`; a model or font bump must re-measure
//! and update that file in the same PR.
//!
//! Everything here is `#[cfg(test)]`: the corpus is a fixture harness, not
//! library API. Pages are rendered from versioned parameters below —
//! same font bytes, same layout, same grayscale coverage mapping — so the
//! fixture set is reproducible without committing images.

use ab_glyph::{Font, FontVec, ScaleFont};

use crate::ocr_onnx::manifest::{METRICS_FONT, ModelManifest, default_models_dir};
use crate::pixmap::Pixmap;
use crate::text::Rect;

/// Font size in pixels — close to the recognizer's native line height
/// (`REC_HEIGHT` = 48) after the detector's box padding.
const FONT_PX: f32 = 40.0;
/// Left text margin in pixels.
const MARGIN_X: u32 = 60;
/// Baseline of the first line, then every `LINE_STEP` pixels.
const FIRST_BASELINE: f32 = 90.0;
const LINE_STEP: f32 = 90.0;
/// Page width; height follows the line count.
const PAGE_W: u32 = 1000;

/// One rendered corpus page with exact per-line ground truth.
pub struct CorpusPage {
    pub pixmap: Pixmap,
    /// Ink-tight bounding box and text of every line, top to bottom.
    pub lines: Vec<(Rect, String)>,
}

/// The Cyrillic sample: two pangram-style excerpts, wrapped by hand.
pub const CYRILLIC_LINES: [&str; 4] = [
    "Съешь же ещё этих мягких",
    "французских булок, да выпей чаю.",
    "Широкая электрификация южных",
    "губерний даст мощный толчок.",
];

/// The Latin + digits sample.
pub const LATIN_LINES: [&str; 4] = [
    "The quick brown fox jumps",
    "over the lazy dog. 0123456789",
    "Sphinx of black quartz,",
    "judge my vow.",
];

/// Load the pinned corpus font, verified against the manifest.
///
/// Returns `None` when the font file is absent (not fetched) — corpus tests
/// skip silently then, same policy as the model-gated tests.
pub fn font_if_present() -> Option<FontVec> {
    let dir = default_models_dir();
    let manifest = ModelManifest::builtin().unwrap();
    let entry = manifest.entry(METRICS_FONT).unwrap();
    let path = entry.path_in(&dir);
    if !path.exists() {
        return None;
    }
    let bytes = entry.load_verified(&path).expect("pinned font must verify");
    Some(FontVec::try_from_vec(bytes).expect("pinned font must parse"))
}

/// Render `lines` into a white page, returning ink-tight line rects.
///
/// Deterministic by construction: fixed layout constants, integer pixel
/// centers, and a pure coverage-to-gray mapping `v = 255 - round(cov * 255)`
/// darkening all three RGB channels.
pub fn render_page(font: &FontVec, lines: &[&str]) -> CorpusPage {
    let scaled = font.as_scaled(FONT_PX);
    let page_h = (FIRST_BASELINE + LINE_STEP * lines.len() as f32) as u32;
    let mut page = Pixmap::white(PAGE_W, page_h);
    let mut truth = Vec::with_capacity(lines.len());
    for (li, text) in lines.iter().enumerate() {
        let baseline = FIRST_BASELINE + LINE_STEP * li as f32;
        let mut pen_x = MARGIN_X as f32;
        let mut prev_glyph = None;
        let (mut min_x, mut min_y) = (u32::MAX, u32::MAX);
        let (mut max_x, mut max_y) = (0u32, 0u32);
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            if let Some(prev) = prev_glyph {
                pen_x += scaled.kern(prev, id);
            }
            prev_glyph = Some(id);
            let glyph = id.with_scale_and_position(FONT_PX, ab_glyph::point(pen_x, baseline));
            pen_x += scaled.h_advance(id);
            let Some(outlined) = font.outline_glyph(glyph) else {
                continue; // whitespace has no outline
            };
            let bounds = outlined.px_bounds();
            outlined.draw(|dx, dy, cov| {
                let x = bounds.min.x as i32 + dx as i32;
                let y = bounds.min.y as i32 + dy as i32;
                if x < 0 || y < 0 || x >= PAGE_W as i32 || y >= page_h as i32 {
                    return;
                }
                let v = 255 - (cov.clamp(0.0, 1.0) * 255.0).round() as u8;
                let (x, y) = (x as u32, y as u32);
                let i = ((y * PAGE_W + x) * 4) as usize;
                // Min: overlapping antialiased edges keep the darker ink.
                let dark = page.data[i].min(v);
                page.data[i..i + 3].fill(dark);
                if v < 255 {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            });
        }
        assert!(min_x < u32::MAX, "corpus line must leave ink: {text}");
        truth.push((
            Rect {
                x: min_x,
                y: min_y,
                width: max_x - min_x + 1,
                height: max_y - min_y + 1,
            },
            (*text).to_string(),
        ));
    }
    CorpusPage {
        pixmap: page,
        lines: truth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{OcrBackend, OcrOptions};
    use crate::ocr_onnx::metrics::{cer, mean_line_iou, wer};
    use crate::ocr_onnx::pipeline::NeuralOcrBackend;
    use crate::text::TextZoneKind;

    #[test]
    fn corpus_pages_render_deterministically() {
        let Some(font) = font_if_present() else {
            return; // font not fetched — skip
        };
        let a = render_page(&font, &CYRILLIC_LINES);
        let b = render_page(&font, &CYRILLIC_LINES);
        assert_eq!(a.pixmap.data, b.pixmap.data);
        assert_eq!(a.lines.len(), CYRILLIC_LINES.len());
        // Ground truth is ordered top-to-bottom with sane boxes.
        for pair in a.lines.windows(2) {
            assert!(pair[0].0.y < pair[1].0.y);
        }
        for (rect, _) in &a.lines {
            assert!(rect.x >= MARGIN_X - 5 && rect.height > 20 && rect.height < 60);
        }
    }

    /// Measure one page: returns (CER, WER, mean line IoU).
    fn evaluate(backend: &NeuralOcrBackend, page: &CorpusPage) -> (f64, f64, f64) {
        let layer = backend
            .recognize(&page.pixmap, &OcrOptions::default())
            .expect("corpus page must recognize");
        let reference: Vec<&str> = page.lines.iter().map(|(_, t)| t.as_str()).collect();
        let reference = reference.join("\n");
        let truth_rects: Vec<_> = page.lines.iter().map(|(r, _)| r.clone()).collect();
        let detected: Vec<_> = layer.zones[0]
            .children
            .iter()
            .filter(|z| z.kind == TextZoneKind::Line)
            .map(|z| z.rect.clone())
            .collect();
        (
            cer(&reference, &layer.text),
            wer(&reference, &layer.text),
            mean_line_iou(&truth_rects, &detected),
        )
    }

    /// Baseline gate for the pinned models — thresholds are the recorded
    /// baseline (docs/ocr-model-metrics.md) plus a safety margin, so a
    /// silent model/preprocessing regression fails this test while normal
    /// cross-platform float jitter does not.
    #[test]
    fn corpus_baseline_holds_for_pinned_models() {
        let Some(font) = font_if_present() else {
            return; // font not fetched — skip
        };
        let dir = default_models_dir();
        let backend = match NeuralOcrBackend::load(&dir) {
            Ok(b) => b,
            Err(_) => return, // models not fetched — skip
        };

        let cyr = evaluate(&backend, &render_page(&font, &CYRILLIC_LINES));
        let lat = evaluate(&backend, &render_page(&font, &LATIN_LINES));
        eprintln!("corpus metrics cyr (cer, wer, iou): {cyr:?}");
        eprintln!("corpus metrics lat (cer, wer, iou): {lat:?}");

        let (cer_cyr, wer_cyr, iou_cyr) = cyr;
        let (cer_lat, wer_lat, iou_lat) = lat;
        assert!(cer_cyr <= 0.05, "Cyrillic CER regressed: {cer_cyr}");
        assert!(wer_cyr <= 0.10, "Cyrillic WER regressed: {wer_cyr}");
        assert!(iou_cyr >= 0.80, "Cyrillic line IoU regressed: {iou_cyr}");
        assert!(cer_lat <= 0.05, "Latin CER regressed: {cer_lat}");
        assert!(wer_lat <= 0.10, "Latin WER regressed: {wer_lat}");
        assert!(iou_lat >= 0.70, "Latin line IoU regressed: {iou_lat}");
    }
}
