//! Backwards-compatible bitmap module.
//!
//! The implementation lives in the standalone `djvu-bitmap` crate. This module
//! preserves the historical `djvu_rs::bitmap::Bitmap` path.

pub use djvu_bitmap::Bitmap;
