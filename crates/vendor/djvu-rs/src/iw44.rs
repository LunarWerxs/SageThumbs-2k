//! IW44 decoder re-export.
//!
//! The implementation lives in the standalone `djvu-iw44` crate; this module
//! re-exports it as `djvu_rs::iw44::*`. (Formerly `iw44_new`, kept as a
//! compatibility alias in `lib.rs`.)

pub use djvu_iw44::*;
