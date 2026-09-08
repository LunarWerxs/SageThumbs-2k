//! Backwards-compatible BZZ compressor module.
//!
//! The implementation lives in the standalone `djvu-bzz` crate. This module
//! preserves the historical `djvu_rs::bzz_encode::bzz_encode` path.

pub use djvu_bzz::bzz_encode;
