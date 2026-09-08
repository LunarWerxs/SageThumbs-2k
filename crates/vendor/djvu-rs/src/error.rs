//! Typed error hierarchy for the djvu-rs crate.
//!
//! This module provides:
//! - [`DjVuError`] — the new top-level error type for phase-1+ code
//! - [`IffError`] — errors from the new IFF container parser
//! - [`BzzError`] — errors from the BZZ decompressor (phase 2a)
//! - [`Jb2Error`] — errors from the JB2 bilevel image decoder
//! - [`Iw44Error`] — errors from the IW44 wavelet image decoder
//! - `LegacyError` — the original error type, kept for backward compatibility
//! - `TextError` — errors from the text layer parser (phase 4, see `text` module)
//! - `AnnotationError` — errors from the annotation parser (phase 4, see `annotation` module)

#[cfg(not(feature = "std"))]
use alloc::borrow::Cow;

// ---- New phase-1 typed errors -----------------------------------------------

/// Top-level error type for all DjVu decoding operations.
#[derive(Debug, thiserror::Error)]
pub enum DjVuError {
    /// An error in the IFF container format.
    #[error("IFF error: {0}")]
    Iff(#[from] IffError),

    /// A JB2 bitonal image decoding error.
    #[error("JB2 error: {0}")]
    Jb2(#[from] Jb2Error),

    /// An IW44 wavelet image decoding error.
    #[error("IW44 error: {0}")]
    Iw44(#[from] Iw44Error),

    /// A BZZ compression decoding error.
    #[error("BZZ error: {0}")]
    Bzz(#[from] BzzError),

    /// A page number was not found in the document.
    #[error("page {0} not found")]
    PageNotFound(usize),

    /// The document structure is invalid or unexpected.
    #[error("invalid structure: {0}")]
    InvalidStructure(&'static str),

    /// A feature or format variant that is not yet supported.
    #[error("unsupported: {0}")]
    #[cfg(feature = "std")]
    Unsupported(std::borrow::Cow<'static, str>),
    /// A feature or format variant that is not yet supported.
    #[error("unsupported: {0}")]
    #[cfg(not(feature = "std"))]
    Unsupported(Cow<'static, str>),

    /// An I/O error (only available with the `std` feature).
    #[cfg(feature = "std")]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub use djvu_iff::IffError;

pub use djvu_jb2::Jb2Error;

pub use djvu_iw44::Iw44Error;

pub use djvu_bzz::BzzError;

// ---- Legacy error type (kept for backward compatibility) --------------------

pub use djvu_iff::LegacyError;

/// Alias for [`LegacyError`] at the path `crate::error::Error`.
///
/// This allows the legacy modules (document.rs, render.rs) which use
/// `crate::error::Error` to continue resolving correctly.
pub use LegacyError as Error;
