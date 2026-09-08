//! Experimental ONNX OCR helper via tract (requires `ocr-onnx` feature).
//!
//! This is a library-level scaffold for simple CTC-style recognizers: callers
//! must provide a compatible model, vocabulary, preprocessing expectations, and
//! output decoding contract. It is intentionally not advertised as a supported
//! CLI backend until a concrete model family and fixture are documented.

use std::path::Path;

use crate::ocr::{OcrBackend, OcrError, OcrOptions};
use crate::pixmap::Pixmap;
use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

type OnnxModel = tract_onnx::prelude::SimplePlan<
    tract_onnx::prelude::TypedFact,
    Box<dyn tract_onnx::prelude::TypedOp>,
    tract_onnx::prelude::Graph<
        tract_onnx::prelude::TypedFact,
        Box<dyn tract_onnx::prelude::TypedOp>,
    >,
>;

/// Experimental ONNX-based OCR backend using tract.
///
/// Expects a pre-trained CTC-style ONNX model that accepts a single grayscale
/// image tensor `[1, 1, H, W]` normalized to `[0, 1]` and emits character
/// probabilities compatible with [`Self::ctc_decode`]. Other architectures
/// require their own preprocessing and decoder and are not supported by this
/// helper.
pub struct OnnxBackend {
    model: OnnxModel,
    /// Character vocabulary for decoding model output.
    vocab: Vec<char>,
}

impl OnnxBackend {
    /// Load an ONNX model from the given path.
    ///
    /// The model should be a CTC-based text recognition model that accepts
    /// a grayscale or RGB image tensor and outputs character probabilities.
    pub fn load(model_path: impl AsRef<Path>, vocab_path: Option<&Path>) -> Result<Self, OcrError> {
        use tract_onnx::prelude::*;

        let model = tract_onnx::onnx()
            .model_for_path(&model_path)
            .map_err(|e| OcrError::InitFailed(format!("failed to load ONNX model: {e}")))?
            .into_optimized()
            .map_err(|e| OcrError::InitFailed(format!("failed to optimize model: {e}")))?
            .into_runnable()
            .map_err(|e| OcrError::InitFailed(format!("failed to make model runnable: {e}")))?;

        let vocab = if let Some(vp) = vocab_path {
            std::fs::read_to_string(vp)?.chars().collect()
        } else {
            // Default ASCII + common Unicode printable characters
            (' '..='~').collect()
        };

        Ok(Self { model, vocab })
    }

    /// Preprocess a pixmap into a normalized grayscale tensor for the model.
    fn preprocess(&self, pixmap: &Pixmap) -> Result<tract_onnx::prelude::Tensor, OcrError> {
        use tract_onnx::prelude::*;

        let gray = pixmap.to_gray8();
        let w = gray.width as usize;
        let h = gray.height as usize;

        // Normalize to [0, 1] float32
        let data: Vec<f32> = gray.data.iter().map(|&v| v as f32 / 255.0).collect();

        // Shape: [1, 1, H, W] (batch, channels, height, width)
        tract_ndarray::Array4::from_shape_vec((1, 1, h, w), data)
            .map_err(|e| OcrError::RecognitionFailed(format!("tensor shape error: {e}")))
            .map(|arr| arr.into_tensor())
    }

    /// Decode CTC output into text using greedy decoding.
    fn ctc_decode(&self, output: &[f32], seq_len: usize) -> String {
        let vocab_size = self.vocab.len();
        let mut result = String::new();
        let mut prev_idx = None;

        for t in 0..seq_len {
            let offset = t * (vocab_size + 1); // +1 for CTC blank
            if offset + vocab_size >= output.len() {
                break;
            }

            // Find argmax
            let mut best_idx = 0;
            let mut best_val = f32::NEG_INFINITY;
            for i in 0..=vocab_size {
                let val = output[offset + i];
                if val > best_val {
                    best_val = val;
                    best_idx = i;
                }
            }

            // Index 0 = CTC blank; skip duplicates
            if best_idx > 0 && Some(best_idx) != prev_idx {
                result.extend(self.vocab.get(best_idx - 1).copied());
            }
            prev_idx = Some(best_idx);
        }

        result
    }
}

impl OcrBackend for OnnxBackend {
    fn recognize(&self, pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
        use tract_onnx::prelude::*;

        let input = self.preprocess(pixmap)?;
        let result = self
            .model
            .run(tvec![input.into()])
            .map_err(|e| OcrError::RecognitionFailed(format!("model inference failed: {e}")))?;

        let output = result[0]
            .to_array_view::<f32>()
            .map_err(|e| OcrError::RecognitionFailed(format!("output tensor error: {e}")))?;

        let shape = output.shape();
        let seq_len = if shape.len() >= 2 { shape[1] } else { shape[0] };
        let text = self.ctc_decode(output.as_slice().unwrap_or(&[]), seq_len);

        // ONNX models typically recognize the whole image as one text block
        let zones = vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: pixmap.width,
                height: pixmap.height,
            },
            text: text.clone(),
            children: vec![TextZone {
                kind: TextZoneKind::Line,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: pixmap.width,
                    height: pixmap.height,
                },
                text: text.clone(),
                children: Vec::new(),
            }],
        }];

        Ok(TextLayer { text, zones })
    }
}
