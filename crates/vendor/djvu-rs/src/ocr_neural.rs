//! Experimental neural OCR placeholder (requires `ocr-neural` feature).
//!
//! This module intentionally does **not** expose a working OCR engine. The
//! previous Candle/TrOCR scaffold accepted a model path and then failed during
//! `recognize`, which made the feature look supported when it was not. Until a
//! specific model family, preprocessing contract, decoder, and fixture are wired
//! up, [`CandleBackend::load`] returns a clear [`OcrError::InitFailed`] instead.
//!
//! Use the `ocr-tesseract` feature for the supported OCR backend. The
//! `ocr-neural-candle` feature name is kept as a no-op compatibility alias and
//! no longer pulls Candle/tokenizers into `--all-features` builds.

use std::path::Path;

use crate::ocr::{OcrBackend, OcrError, OcrOptions};
use crate::pixmap::Pixmap;
use crate::text::TextLayer;

/// Placeholder for a future Candle-backed OCR implementation.
///
/// There is currently no supported model-specific neural OCR contract in
/// `djvu-rs`. Constructing this backend through [`Self::load`] always returns a
/// clear error instead of producing a value whose `recognize` method fails later.
pub struct CandleBackend {
    _private: (),
}

impl CandleBackend {
    /// Return an explicit unsupported-backend error.
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self, OcrError> {
        Err(OcrError::InitFailed(format!(
            "Candle OCR backend is experimental and has no supported model-specific \
             implementation yet (requested model dir: {}); use the ocr-tesseract \
             feature for supported OCR",
            model_dir.as_ref().display()
        )))
    }
}

impl OcrBackend for CandleBackend {
    fn recognize(&self, _pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
        Err(OcrError::RecognitionFailed(
            "Candle OCR backend is experimental and unavailable; use the ocr-tesseract feature"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{OcrBackend, OcrOptions};
    use crate::pixmap::Pixmap;

    #[test]
    fn load_always_returns_init_failed() {
        let err = CandleBackend::load("/some/model/dir").unwrap_err();
        assert!(matches!(err, OcrError::InitFailed(_)));
    }

    #[test]
    fn recognize_always_returns_recognition_failed() {
        // Construct via the private field directly — load() always errors,
        // so bypass it to reach recognize().
        let backend = CandleBackend { _private: () };
        let pm = Pixmap::white(1, 1);
        let opts = OcrOptions::default();
        let err = backend.recognize(&pm, &opts).unwrap_err();
        assert!(matches!(err, OcrError::RecognitionFailed(_)));
    }
}
