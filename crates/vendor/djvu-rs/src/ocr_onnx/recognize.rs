//! Cyrillic CTC text recognition with verified PP-OCRv5 weights (#693).
//!
//! [`TextRecognizer`] loads the pinned Cyrillic recognizer and its companion
//! config through [`manifest`](super::manifest) (both SHA-256-verified). The
//! config's `character_dict` is the decode dictionary — model and dictionary
//! are pinned to the same upstream commit, and a class-count mismatch between
//! them is a hard error.
//!
//! Per line box (from [`detect`](super::detect)) the pipeline is:
//!
//! 1. Crop the line from the page pixmap (clamped to the page).
//! 2. Resize to height [`REC_HEIGHT`] keeping aspect ratio (the same
//!    fixed-point bilinear resampler the detector uses), pad the width up to
//!    a bucket of [`REC_WIDTH_BUCKET`] so a handful of tract plans serve all
//!    line lengths.
//! 3. Normalize to `(v/255 − 0.5)/0.5` in **BGR** channel order (the pinned
//!    config's `DecodeImage: img_mode: BGR`); padded columns hold 0.0
//!    (mid-gray), matching the PP-OCR deployment pipeline.
//! 4. Run the model and greedy-decode CTC: argmax per time step, collapse
//!    repeats, skip blank (class 0); class `dict_len + 1` is the space the
//!    upstream decoder appends (`use_space_char`).
//!
//! Dictionary parsing and CTC decoding are pure functions locked by unit
//! tests; tests that run the real model skip silently when the weights have
//! not been fetched (`scripts/fetch_ocr_models.sh`).

use std::path::Path;

use crate::ocr::OcrError;
use crate::pixmap::Pixmap;
use crate::text::Rect;

use super::manifest::{ModelManifest, REC_CYRILLIC_CONFIG, REC_CYRILLIC_MODEL, default_models_dir};
use super::preprocess;

type OnnxPlan = tract_onnx::prelude::SimplePlan<
    tract_onnx::prelude::TypedFact,
    Box<dyn tract_onnx::prelude::TypedOp>,
    tract_onnx::prelude::Graph<
        tract_onnx::prelude::TypedFact,
        Box<dyn tract_onnx::prelude::TypedOp>,
    >,
>;

/// Fixed recognizer input height (the model's trained line height).
pub const REC_HEIGHT: u32 = 48;

/// Upper bound on the recognizer input width (the upstream deployment
/// config's own maximum dynamic width).
pub const REC_MAX_WIDTH: u32 = 3200;

/// Input widths are padded up to a multiple of this, so a few cached tract
/// plans cover all line lengths.
const REC_WIDTH_BUCKET: u32 = 32;

/// Compiled plans kept per input width.
const PLAN_CACHE_CAPACITY: usize = 6;

/// CTC decode dictionary parsed from the pinned recognizer config.
///
/// Class layout (PP-OCR `CTCLabelDecode` convention): class 0 is the CTC
/// blank, classes `1..=dict_len` map to dictionary entries, and class
/// `dict_len + 1` is the space character the upstream decoder appends.
pub struct Vocabulary {
    chars: Vec<String>,
}

impl Vocabulary {
    /// Parse the `PostProcess.CTCLabelDecode.character_dict` list out of the
    /// pinned recognizer config (a small, fixed YAML subset: one `- item`
    /// line per character, single-quote escaping).
    ///
    /// # Errors
    ///
    /// [`OcrError::InitFailed`] if the config has no non-empty
    /// `character_dict` list.
    pub fn parse_from_config(config: &str) -> Result<Self, OcrError> {
        let mut chars = Vec::new();
        let mut in_dict = false;
        for line in config.lines() {
            if in_dict {
                if let Some(item) = line.strip_prefix("  - ") {
                    chars.push(unquote_yaml_single(item));
                    continue;
                }
                break; // end of the list block
            }
            if line.trim_end() == "  character_dict:" {
                in_dict = true;
            }
        }
        if chars.is_empty() {
            return Err(OcrError::InitFailed(
                "recognizer config has no character_dict entries".into(),
            ));
        }
        Ok(Self { chars })
    }

    /// Number of dictionary characters (excluding blank and space).
    pub fn dict_len(&self) -> usize {
        self.chars.len()
    }

    /// Total model classes: blank + dictionary + appended space.
    pub fn class_count(&self) -> usize {
        self.chars.len() + 2
    }

    /// The text a CTC class index decodes to; `None` for the blank class or
    /// out-of-range indices.
    pub fn decode_class(&self, class: usize) -> Option<&str> {
        match class {
            0 => None,
            c if c <= self.chars.len() => Some(self.chars[c - 1].as_str()),
            c if c == self.chars.len() + 1 => Some(" "),
            _ => None,
        }
    }
}

/// Strip YAML single-quoting: `'x'` → `x`, with `''` unescaping to `'`;
/// unquoted scalars pass through unchanged.
fn unquote_yaml_single(item: &str) -> String {
    let trimmed = item.trim_end();
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        trimmed[1..trimmed.len() - 1].replace("''", "'")
    } else {
        trimmed.to_string()
    }
}

/// One recognized line of text.
#[derive(Debug, Clone, PartialEq)]
pub struct LineText {
    /// Decoded text (may be empty for blank crops).
    pub text: String,
    /// Mean model probability over the emitted characters; 0.0 when nothing
    /// was emitted.
    pub confidence: f32,
}

/// Greedy CTC decode of a `[steps, classes]` probability matrix.
///
/// Pure function (exposed for unit tests): per time step take the argmax
/// class, collapse consecutive repeats, skip blanks, and map the rest
/// through `vocab`. Confidence is the mean probability of the emitted
/// characters.
pub fn ctc_greedy_decode(
    probs: &[f32],
    steps: usize,
    classes: usize,
    vocab: &Vocabulary,
) -> LineText {
    debug_assert_eq!(probs.len(), steps * classes);
    let mut text = String::new();
    let mut conf_sum = 0.0f64;
    let mut emitted = 0u32;
    let mut prev = usize::MAX;
    for t in 0..steps {
        let row = &probs[t * classes..(t + 1) * classes];
        let (best, best_p) =
            row.iter()
                .enumerate()
                .fold((0, f32::NEG_INFINITY), |(bi, bp), (i, &p)| {
                    if p > bp { (i, p) } else { (bi, bp) }
                });
        if best != prev
            && let Some(s) = vocab.decode_class(best)
        {
            text.push_str(s);
            conf_sum += f64::from(best_p);
            emitted += 1;
        }
        prev = best;
    }
    let confidence = if emitted == 0 {
        0.0
    } else {
        (conf_sum / f64::from(emitted)) as f32
    };
    LineText { text, confidence }
}

/// Crop `rect` (clamped to the page) into a standalone RGBA pixmap.
fn crop_page(page: &Pixmap, rect: &Rect) -> Pixmap {
    let x0 = rect.x.min(page.width);
    let y0 = rect.y.min(page.height);
    let x1 = rect.x.saturating_add(rect.width).min(page.width);
    let y1 = rect.y.saturating_add(rect.height).min(page.height);
    let (w, h) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
    let mut out = Pixmap::white(w, h);
    for y in 0..h.min(page.height.saturating_sub(y0)) {
        let src = ((y0 + y) as usize * page.width as usize + x0 as usize) * 4;
        let dst = y as usize * w as usize * 4;
        let n = w.min(page.width - x0) as usize * 4;
        out.data[dst..dst + n].copy_from_slice(&page.data[src..src + n]);
    }
    out
}

/// Recognizer input width for a line crop of `w × h` page pixels: scale to
/// height [`REC_HEIGHT`] preserving aspect, then pad up to a
/// [`REC_WIDTH_BUCKET`] multiple, capped at [`REC_MAX_WIDTH`].
///
/// Returns `(content_width, padded_width)`.
fn rec_widths(w: u32, h: u32) -> (u32, u32) {
    let h = h.max(1);
    let scaled = (u64::from(w) * u64::from(REC_HEIGHT) + u64::from(h) / 2) / u64::from(h);
    let content = (scaled as u32).clamp(1, REC_MAX_WIDTH);
    let padded = content
        .div_ceil(REC_WIDTH_BUCKET)
        .saturating_mul(REC_WIDTH_BUCKET)
        .min(REC_MAX_WIDTH);
    (content.min(padded), padded)
}

/// Build the recognizer input tensor for one line crop: `[1, 3, 48, W]`
/// CHW f32, **BGR**, normalized `(v/255 − 0.5)/0.5`, zero-padded on the
/// right. Returns the buffer and its padded width.
fn rec_tensor(page: &Pixmap, line: &Rect) -> (Vec<f32>, u32) {
    let cropped = crop_page(page, line);
    let (content_w, padded_w) = rec_widths(cropped.width, cropped.height);
    let rgb = preprocess::resize_bilinear_rgb(&cropped, content_w, REC_HEIGHT);

    let plane = padded_w as usize * REC_HEIGHT as usize;
    // Zero-filled: padded columns stay at 0.0 (normalized mid-gray).
    let mut out = vec![0.0f32; 3 * plane];
    for y in 0..REC_HEIGHT as usize {
        for x in 0..content_w as usize {
            let px = &rgb[(y * content_w as usize + x) * 3..][..3];
            let at = y * padded_w as usize + x;
            // BGR: channel 0 ← blue, 1 ← green, 2 ← red.
            for (c, &v) in [px[2], px[1], px[0]].iter().enumerate() {
                out[c * plane + at] = (f32::from(v) / 255.0 - 0.5) / 0.5;
            }
        }
    }
    (out, padded_w)
}

/// Cyrillic PP-OCRv5 mobile CTC text recognizer.
///
/// Holds the verified model bytes, the pinned dictionary, and an LRU of
/// compiled plans keyed by padded input width. `recognize_line` takes
/// `&mut self` because a new line length may compile and cache a new plan.
pub struct TextRecognizer {
    model_bytes: Vec<u8>,
    vocab: Vocabulary,
    /// Most-recently-used first, keyed by padded input width.
    plans: Vec<(u32, OnnxPlan)>,
}

impl TextRecognizer {
    /// Load the pinned recognizer and its dictionary config from
    /// `models_dir`, verifying both against the built-in manifest.
    ///
    /// # Errors
    ///
    /// [`OcrError::Io`] if a file is missing or unreadable (fetch with
    /// `scripts/fetch_ocr_models.sh`);
    /// [`OcrError::ModelVerificationFailed`] on size/SHA-256 mismatch;
    /// [`OcrError::InitFailed`] if the verified config has no dictionary.
    pub fn load(models_dir: &Path) -> Result<Self, OcrError> {
        let manifest = ModelManifest::builtin()?;
        let model_entry = manifest.entry(REC_CYRILLIC_MODEL)?;
        let config_entry = manifest.entry(REC_CYRILLIC_CONFIG)?;
        let model_bytes = model_entry.load_verified(&model_entry.path_in(models_dir))?;
        let config_bytes = config_entry.load_verified(&config_entry.path_in(models_dir))?;
        let config = String::from_utf8(config_bytes)
            .map_err(|e| OcrError::InitFailed(format!("recognizer config is not UTF-8: {e}")))?;
        let vocab = Vocabulary::parse_from_config(&config)?;
        Ok(Self {
            model_bytes,
            vocab,
            plans: Vec::new(),
        })
    }

    /// [`TextRecognizer::load`] from the default models directory
    /// (`$DJVU_OCR_MODELS_DIR` or `models/ocr`).
    pub fn load_default() -> Result<Self, OcrError> {
        Self::load(&default_models_dir())
    }

    /// The pinned decode dictionary.
    pub fn vocabulary(&self) -> &Vocabulary {
        &self.vocab
    }

    /// Recognize one detected line box on a rendered page.
    ///
    /// # Errors
    ///
    /// [`OcrError::RecognitionFailed`] if plan compilation or inference
    /// fails, or the model's class count does not match the pinned
    /// dictionary (a model/config mismatch).
    pub fn recognize_line(&mut self, page: &Pixmap, line: &Rect) -> Result<LineText, OcrError> {
        use tract_onnx::prelude::*;

        let (data, padded_w) = rec_tensor(page, line);
        let tensor = tract_ndarray::Array4::from_shape_vec(
            (1, 3, REC_HEIGHT as usize, padded_w as usize),
            data,
        )
        .map_err(|e| OcrError::RecognitionFailed(format!("input tensor shape error: {e}")))?
        .into_tensor();

        let class_count = self.vocab.class_count();
        let plan = self.plan_for(padded_w)?;
        let result = plan.run(tvec![tensor.into()]).map_err(|e| {
            OcrError::RecognitionFailed(format!("recognizer inference failed: {e}"))
        })?;

        let output = result[0]
            .to_array_view::<f32>()
            .map_err(|e| OcrError::RecognitionFailed(format!("recognizer output error: {e}")))?;
        let shape = output.shape().to_vec();
        if shape.len() != 3 || shape[0] != 1 || shape[2] != class_count {
            return Err(OcrError::RecognitionFailed(format!(
                "recognizer output shape {shape:?}, expected [1, steps, {class_count}] — \
                 model/dictionary mismatch?"
            )));
        }
        let probs = output.as_slice().ok_or_else(|| {
            OcrError::RecognitionFailed("recognizer output is not contiguous".into())
        })?;
        Ok(ctc_greedy_decode(probs, shape[1], class_count, &self.vocab))
    }

    /// Fetch (or compile and cache) the plan for padded input width `w`.
    fn plan_for(&mut self, w: u32) -> Result<&OnnxPlan, OcrError> {
        use tract_onnx::prelude::*;

        if let Some(pos) = self.plans.iter().position(|(k, _)| *k == w) {
            let hit = self.plans.remove(pos);
            self.plans.insert(0, hit);
        } else {
            let plan = tract_onnx::onnx()
                .model_for_read(&mut &self.model_bytes[..])
                .map_err(|e| OcrError::RecognitionFailed(format!("model parse failed: {e}")))?
                .with_input_fact(
                    0,
                    InferenceFact::dt_shape(
                        f32::datum_type(),
                        tvec!(1, 3, REC_HEIGHT as usize, w as usize),
                    ),
                )
                .map_err(|e| OcrError::RecognitionFailed(format!("input fact failed: {e}")))?
                .into_optimized()
                .map_err(|e| OcrError::RecognitionFailed(format!("plan optimize failed: {e}")))?
                .into_runnable()
                .map_err(|e| OcrError::RecognitionFailed(format!("plan build failed: {e}")))?;
            self.plans.insert(0, (w, plan));
            self.plans.truncate(PLAN_CACHE_CAPACITY);
        }
        Ok(&self.plans[0].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_vocab() -> Vocabulary {
        Vocabulary {
            chars: vec!["а".into(), "б".into(), "в".into()],
        }
    }

    #[test]
    fn vocabulary_parses_quoted_and_plain_dict_items() {
        let config = "\
Global:
  model_name: x
PostProcess:
  name: CTCLabelDecode
  character_dict:
  - '!'
  - $
  - ''''
  - а
  - Ꚙ
";
        let vocab = Vocabulary::parse_from_config(config).unwrap();
        assert_eq!(vocab.dict_len(), 5);
        assert_eq!(vocab.decode_class(1), Some("!"));
        assert_eq!(vocab.decode_class(2), Some("$"));
        assert_eq!(vocab.decode_class(3), Some("'"));
        assert_eq!(vocab.decode_class(4), Some("а"));
        assert_eq!(vocab.decode_class(5), Some("Ꚙ"));
    }

    #[test]
    fn vocabulary_class_layout_matches_ctc_convention() {
        let vocab = tiny_vocab();
        assert_eq!(vocab.class_count(), 5);
        assert_eq!(vocab.decode_class(0), None, "class 0 is the CTC blank");
        assert_eq!(vocab.decode_class(4), Some(" "), "last class is space");
        assert_eq!(vocab.decode_class(5), None, "out of range");
    }

    #[test]
    fn vocabulary_rejects_config_without_dict() {
        assert!(matches!(
            Vocabulary::parse_from_config("Global:\n  model_name: x\n"),
            Err(OcrError::InitFailed(_))
        ));
    }

    #[test]
    fn ctc_greedy_decode_collapses_repeats_and_blanks() {
        let vocab = tiny_vocab();
        // Classes: 0 blank, 1 'а', 2 'б', 3 'в', 4 space.
        // Steps: а а blank б б space в → "аб в"
        let steps = [1usize, 1, 0, 2, 2, 4, 3];
        let classes = vocab.class_count();
        let mut probs = vec![0.0f32; steps.len() * classes];
        for (t, &c) in steps.iter().enumerate() {
            probs[t * classes + c] = 0.9;
        }
        let line = ctc_greedy_decode(&probs, steps.len(), classes, &vocab);
        assert_eq!(line.text, "аб в");
        assert!((line.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn ctc_greedy_decode_empty_on_all_blanks() {
        let vocab = tiny_vocab();
        let classes = vocab.class_count();
        let mut probs = vec![0.0f32; 4 * classes];
        for t in 0..4 {
            probs[t * classes] = 1.0;
        }
        let line = ctc_greedy_decode(&probs, 4, classes, &vocab);
        assert_eq!(line.text, "");
        assert_eq!(line.confidence, 0.0);
    }

    #[test]
    fn rec_widths_scale_and_bucket() {
        // Square crop → width 48 → padded to 64.
        assert_eq!(rec_widths(100, 100), (48, 64));
        // Wide line: 10:1 aspect at height 48 → 480, already a multiple of 32.
        assert_eq!(rec_widths(1000, 100), (480, 480));
        // Degenerate and huge crops stay in range.
        assert_eq!(rec_widths(1, 1000), (1, 32));
        let (c, p) = rec_widths(1_000_000, 10);
        assert_eq!((c, p), (REC_MAX_WIDTH, REC_MAX_WIDTH));
    }

    #[test]
    fn crop_page_clamps_to_page_bounds() {
        let mut page = Pixmap::white(10, 10);
        for (i, b) in page.data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let rect = Rect {
            x: 6,
            y: 6,
            width: 100,
            height: 100,
        };
        let crop = crop_page(&page, &rect);
        assert_eq!((crop.width, crop.height), (4, 4));
        // Top-left crop pixel equals page pixel (6, 6).
        let src = (6 * 10 + 6) * 4;
        assert_eq!(&crop.data[..4], &page.data[src..src + 4]);
    }

    #[test]
    fn rec_tensor_is_bgr_normalized_and_padded() {
        // Uniform color: R=255, G=127.5-ish, B=0.
        let mut page = Pixmap::white(64, 32);
        for px in page.data.as_chunks_mut::<4>().0 {
            px[0] = 255; // R
            px[1] = 128; // G
            px[2] = 0; // B
        }
        let rect = Rect {
            x: 0,
            y: 0,
            width: 64,
            height: 32,
        };
        let (data, padded_w) = rec_tensor(&page, &rect);
        // 64×32 crop → content width 96, padded to 96 (multiple of 32).
        assert_eq!(padded_w, 96);
        let plane = padded_w as usize * REC_HEIGHT as usize;
        assert_eq!(data.len(), 3 * plane);
        // Channel 0 is BLUE (=0 → normalized -1.0), channel 2 is RED (=255 → 1.0).
        assert!((data[0] - (-1.0)).abs() < 1e-6);
        assert!((data[2 * plane] - 1.0).abs() < 1e-6);
        // No padded columns here (content == padded): all of channel 0 is -1.
        assert!(data[..plane].iter().all(|&v| (v + 1.0).abs() < 1e-6));
    }

    // ── Model-gated tests (skip silently when weights are absent) ────────────

    fn recognizer_if_models_present() -> Option<TextRecognizer> {
        let dir = default_models_dir();
        let manifest = ModelManifest::builtin().unwrap();
        for name in [REC_CYRILLIC_MODEL, REC_CYRILLIC_CONFIG] {
            if !manifest.entry(name).unwrap().path_in(&dir).exists() {
                return None; // weights not fetched — skip (CI fetches them in the ocr-onnx job)
            }
        }
        Some(TextRecognizer::load(&dir).expect("pinned weights must verify and load"))
    }

    #[test]
    fn pinned_dictionary_matches_model_class_count() {
        let Some(rec) = recognizer_if_models_present() else {
            return;
        };
        // The real model emits 852 classes; the pinned dict must agree.
        assert_eq!(rec.vocabulary().class_count(), 852);
        assert_eq!(rec.vocabulary().dict_len(), 850);
    }

    #[test]
    fn blank_line_recognizes_as_empty() {
        let Some(mut rec) = recognizer_if_models_present() else {
            return;
        };
        let page = Pixmap::white(400, 60);
        let rect = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 60,
        };
        let line = rec.recognize_line(&page, &rect).expect("inference");
        assert_eq!(line.text.trim(), "", "blank crop must decode to nothing");
        // Plan-cache exercise: a different line length, then the first again.
        let short = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 60,
        };
        rec.recognize_line(&page, &short).expect("second width");
        rec.recognize_line(&page, &rect).expect("cached plan reuse");
    }
}
