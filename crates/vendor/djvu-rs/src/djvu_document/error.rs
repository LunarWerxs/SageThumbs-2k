//! The document error type.

use super::*;

// ---- Error type -------------------------------------------------------------

/// Errors that can occur when working with the DjVuDocument API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DocError {
    /// IFF container parse error.
    #[error("IFF error: {0}")]
    Iff(#[from] IffError),

    /// BZZ decompression error.
    #[error("BZZ error: {0}")]
    Bzz(#[from] BzzError),

    /// IW44 wavelet decoding error.
    #[error("IW44 error: {0}")]
    Iw44(#[from] Iw44Error),

    /// JB2 bilevel image decoding error.
    #[error("JB2 error: {0}")]
    Jb2(#[from] Jb2Error),

    /// The file is not a supported DjVu format.
    #[error("not a DjVu file: found form type {0:?}")]
    NotDjVu([u8; 4]),

    /// A required chunk is missing.
    #[error("missing required chunk: {0}")]
    MissingChunk(&'static str),

    /// The document is malformed (description included).
    #[error("malformed DjVu document: {0}")]
    Malformed(&'static str),

    /// An indirect page reference could not be resolved.
    #[error("failed to resolve indirect page '{0}'")]
    IndirectResolve(String),

    /// A typed resolver could not provide one indirect component.
    #[error("component resolution failed: {0}")]
    ComponentResolve(#[from] ComponentResolveError),

    /// A resolved component's FORM type disagrees with its DIRM classification.
    #[error("indirect component {component:?} has FORM:{found:?}, expected kind {expected:?}")]
    ComponentKindMismatch {
        /// Identity from the DIRM entry.
        component: ComponentId,
        /// FORM type found in the resolved bytes.
        found: [u8; 4],
        /// FORM type required by the DIRM kind.
        expected: ComponentKind,
    },

    /// Page index is out of range.
    #[error("page index {index} is out of range (document has {count} pages)")]
    PageOutOfRange { index: usize, count: usize },

    /// Invalid UTF-8 in a string field.
    ///
    /// No longer produced since #524: NAVM bookmark strings are decoded
    /// leniently (CP1252 fallback). Kept so matching code keeps compiling.
    #[error("invalid UTF-8 in DjVu metadata")]
    InvalidUtf8,

    /// The resolver callback is required for indirect documents but was not provided.
    #[error("indirect DjVu document requires a resolver callback")]
    NoResolver,

    /// I/O error when reading file data (only with `std` feature).
    #[cfg(feature = "std")]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// G4/MMR mask decoding error.
    #[error("Smmr decode error: {0}")]
    Smmr(String),

    /// Text layer parse error.
    #[error("text layer error: {0}")]
    Text(#[from] TextError),

    /// Annotation parse error.
    #[error("annotation error: {0}")]
    Annotation(#[from] AnnotationError),

    /// Metadata parse error.
    #[error("metadata error: {0}")]
    Metadata(#[from] MetadataError),

    /// A configured resource limit was exceeded during document parse/open.
    #[error("{0}")]
    ResourceLimit(#[from] crate::resource_limits::ResourceLimitExceeded),
}
