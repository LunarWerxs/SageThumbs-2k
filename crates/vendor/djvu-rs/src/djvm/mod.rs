//! DJVM document merge and split operations.
//!
//! Provides [`merge`] to combine multiple DjVu documents into a single
//! bundled DJVM, and [`split`] to extract page ranges from a document.
//!
//! [`merge`]: crate::djvm::merge
//! [`split`]: crate::djvm::split

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec, vec::Vec};

use crate::dirm::{BUNDLED_FLAG, DirmComponent, DirmComponentKind, DirmPayload, is_page_form};
use crate::error::IffError;
use crate::iff;
use crate::{ComponentGraph, ComponentNodeKind};

#[cfg(test)]
use crate::djvu_document::DjVuDocument;

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// One file per concern (#907). Every item a sibling needs is `pub(super)`:
// the same reach a private item had when this module was one file.

mod bundle;
mod edit;
mod indirect;
mod merge;
mod stream;

use bundle::*;
pub(crate) use bundle::{BundlePart, build_djvm};
pub use edit::*;
pub use indirect::*;
pub use merge::*;
pub use stream::*;

/// Error type for DJVM merge, split, and conversion operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DjvmError {
    /// IFF container parse error.
    #[error("IFF parse error: {0}")]
    Iff(#[from] IffError),

    /// Document model error.
    #[error("document error: {0}")]
    Doc(#[from] crate::djvu_document::DocError),

    /// No pages to merge.
    #[error("no pages to merge")]
    EmptyMerge,

    /// Page range is out of bounds.
    #[error("page range {start}..{end} is out of bounds (document has {count} pages)")]
    PageRangeOutOfBounds {
        start: usize,
        end: usize,
        count: usize,
    },

    /// A page-removal index is out of bounds.
    #[error("page index {index} is out of bounds (document has {count} pages)")]
    PageIndexOutOfBounds {
        /// The requested page index.
        index: usize,
        /// Number of pages in the document.
        count: usize,
    },

    /// A page-removal index was supplied more than once.
    #[error("page index {index} was specified more than once")]
    DuplicatePageIndex {
        /// The duplicate page index.
        index: usize,
    },

    /// Removing the requested pages would leave the document empty.
    #[error("cannot remove all {count} pages from a document")]
    AllPagesRemoved {
        /// Number of pages in the document.
        count: usize,
    },

    /// The bundled component graph could not be built.
    #[error("component graph error: {0}")]
    ComponentGraph(String),

    /// The assembled document's FORM payload would exceed `u32::MAX` (4 GiB).
    #[error("merged document exceeds the 4 GiB IFF FORM limit")]
    OutputTooLarge,

    /// The input is not a bundled `FORM:DJVM` document.
    #[error("to_indirect requires a bundled FORM:DJVM document")]
    NotBundledDjvm,

    /// The `DIRM` payload is malformed or missing a required field.
    #[error("DIRM chunk is malformed: {0}")]
    DirmMalformed(&'static str),

    /// The bundled `DIRM` and embedded component count disagree.
    #[error("DIRM component count {dirm} does not match bundle child count {children}")]
    DirmComponentCountMismatch {
        /// Component count declared by `DIRM`.
        dirm: usize,
        /// Direct `FORM` children in the bundle.
        children: usize,
    },

    /// A streaming sink or temporary spool could not be read or written.
    #[error("stream I/O error: {0}")]
    Io(#[from] io::Error),

    /// More than `u16::MAX` components were supplied for one bundled DIRM.
    #[error("bundled DIRM supports at most 65535 components (got {count})")]
    TooManyComponents {
        /// Number of requested components.
        count: usize,
    },

    /// Two components passed to [`create_indirect_with_components`] share a name.
    #[error("component name {name:?} is used more than once")]
    DuplicateComponentName {
        /// The repeated name.
        name: String,
    },

    /// A component passed to [`create_indirect_with_components`] is not a
    /// page, shared (`DJVI`) or thumbnail (`THUM`) form.
    #[error(
        "component {name:?} is FORM:{}, not a DjVu component",
        String::from_utf8_lossy(form_type)
    )]
    UnsupportedComponentForm {
        /// The component name.
        name: String,
        /// The FORM type found.
        form_type: [u8; 4],
    },

    /// A page includes (`INCL`) a name that is not a shared component of the
    /// same document.
    #[error("page {page:?} includes {include:?}, which is not a shared component")]
    UnresolvedInclude {
        /// The page component name.
        page: String,
        /// The `INCL` target.
        include: String,
    },
}

#[cfg(test)]
mod tests;
