//! New document model for DjVu files — phase 3.
//!
//! This module provides the high-level `DjVuDocument` API built on top of the
//! clean-room IFF parser (phase 1), BZZ decompressor (phase 2a), and IW44 decoder
//! (phase 2c).
//!
//! ## Key public types
//!
//! - [`DjVuDocument`] — opened DjVu document (single-page or multi-page)
//! - [`DjVuPage`] — lazy page handle (raw chunks stored until `thumbnail()` is called)
//! - [`DjVuBookmark`] — table-of-contents entry from the NAVM chunk
//! - [`DocError`] — typed errors for this module
//!
//! ## Document kinds
//!
//! - **FORM:DJVU** — single-page document
//! - **FORM:BM44** / **FORM:PM44** — legacy standalone IW44 photo documents
//!   (grayscale / color); exposed as one-page documents without an INFO chunk
//! - **FORM:DJVM + DIRM** — bundled multi-page document with an in-file page index
//! - **FORM:DJVM + DIRM (indirect)** — components live in separate files; the
//!   typed [`ComponentResolver`] contract identifies each page, shared, or
//!   thumbnail component
//!
//! ## Lazy decoding contract
//!
//! `DjVuPage` stores only the raw chunk bytes. No image decoding happens until
//! the caller explicitly calls `thumbnail()` (which invokes the IW44 decoder).

#[cfg(not(feature = "std"))]
use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::{
    annotation::{Annotation, AnnotationError, MapArea},
    bzz::bzz_decode,
    dirm::{DirmComponent, DirmComponentKind, DirmPayload},
    error::{BzzError, IffError, Iw44Error, Jb2Error},
    iff::{IffChunk, parse_form, parse_form_body},
    info::PageInfo,
    iw44::Iw44Image,
    jb2::Jb2Dict,
    metadata::{DjVuMetadata, MetadataError},
    pixmap::Pixmap,
    text::{TextError, TextLayer},
};

#[cfg(not(feature = "std"))]
use alloc::sync::Arc;
#[cfg(feature = "std")]
use std::sync::Arc;

pub(crate) mod assembly;

// ---- Submodules -------------------------------------------------------------
//
// One file per concern (#893). Every item a sibling needs is `pub(super)`:
// the same reach a private item had when this module was one file.

mod bookmark;
mod component;
mod document;
mod error;
#[cfg(feature = "mmap")]
mod mmap;
mod page;
mod parse;

pub use bookmark::*;
pub use component::*;
pub use document::*;
pub use error::*;
#[cfg(feature = "mmap")]
pub use mmap::*;
pub use page::*;
pub(crate) use parse::*;

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests;
