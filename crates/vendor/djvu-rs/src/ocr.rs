//! Pluggable OCR backend trait and error types.
//!
//! Provides [`OcrBackend`] — an abstraction over OCR engines.
//!
//! Supported user-facing OCR is currently limited to:
//! - `ocr-tesseract` — system Tesseract via `tesseract-rs`.
//!
//! Experimental scaffolding is kept separate and is not exposed as a supported
//! CLI backend:
//! - `ocr-onnx` — generic tract/CTC helper with no stable model contract yet.
//! - `ocr-neural` — placeholder Candle backend; `load()` returns a clear
//!   unsupported-backend error until a concrete model implementation lands.
//!
//! ## Design
//!
//! The trait is a deliberate runtime seam, not a speculative abstraction: the
//! `djvu ocr --backend` CLI selector builds a `Box<dyn OcrBackend>` and drives
//! it polymorphically. It is retained by decision even though only Tesseract is
//! fully wired today. See `docs/ocr-backend-seam.md` (issue #382) for the
//! rationale and the deferred plan for `OcrOptions`.
//!
//! [`OcrBackend`]: crate::ocr::OcrBackend

use crate::pixmap::Pixmap;
use crate::text::TextLayer;

/// Error type for OCR operations.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The OCR engine failed to initialize.
    #[error("OCR init failed: {0}")]
    InitFailed(String),

    /// Recognition failed on a page image.
    #[error("OCR recognition failed: {0}")]
    RecognitionFailed(String),

    /// The specified language or model is not available.
    #[error("OCR model/language not found: {0}")]
    ModelNotFound(String),

    /// I/O error (e.g. loading model file).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for an OCR run.
///
/// These are **advisory hints**: each backend honours the fields that apply to
/// it and ignores the rest. The Tesseract backend uses both; an ONNX model fixes
/// its own input size and vocabulary at load time and ignores them. A
/// model-neutral recast is deferred until a second CLI-live backend needs to be
/// configured through the trait — see `docs/ocr-backend-seam.md`.
#[derive(Debug, Clone)]
pub struct OcrOptions {
    /// Languages to recognize (e.g. "eng", "rus+eng"). Advisory; ignored by
    /// backends whose model is not language-parameterized.
    pub languages: String,
    /// Page DPI (helps OCR engines scale internally). Advisory; ignored by
    /// backends with a fixed input resolution.
    pub dpi: u32,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            languages: "eng".into(),
            dpi: 300,
        }
    }
}

/// Trait for pluggable OCR backends.
///
/// Implementations receive a rendered page pixmap and return a structured
/// text layer that can be written back into the DjVu file as a TXTz chunk.
pub trait OcrBackend {
    /// Recognize text in the given page image.
    ///
    /// Returns a [`TextLayer`] whose bounding boxes are in the pixmap's
    /// coordinate system (top-left origin).
    ///
    /// **Minimum granularity guarantee.** Callers may rely only on a populated
    /// top-level [`TextLayer`]`::text` string and at least one page-level zone;
    /// the richer `page -> line -> word` hierarchy is best-effort and
    /// backend-dependent. The Tesseract backend produces the full hierarchy;
    /// the experimental `ocr-onnx`/`ocr-neural` backends (see the module docs)
    /// may emit a coarser tree or none at all. Consumers that need word-level
    /// rects (e.g. the hOCR/ALTO exporters) must tolerate a flatter layer.
    fn recognize(&self, pixmap: &Pixmap, options: &OcrOptions) -> Result<TextLayer, OcrError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_options_default_values() {
        let opts = OcrOptions::default();
        assert_eq!(opts.languages, "eng");
        assert_eq!(opts.dpi, 300);
    }
}
