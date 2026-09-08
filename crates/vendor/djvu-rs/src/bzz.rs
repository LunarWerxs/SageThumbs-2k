//! BZZ decompressor re-export.
//!
//! The implementation lives in the standalone `djvu-bzz` crate; this module
//! re-exports it as `djvu_rs::bzz::*`. (Formerly `bzz_new`, kept as a
//! compatibility alias in `lib.rs`.)

pub use djvu_bzz::{bzz_decode, decode};

#[cfg(feature = "parallel")]
pub use djvu_bzz::bzz_decode_parallel;
