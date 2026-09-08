//! IW44 wavelet image encoder (re-export shim).
//!
//! The encoder implementation lives in the standalone `djvu-iw44` crate
//! ([`djvu_iw44::encode`]); this module re-exports it so the historical
//! `djvu_rs::iw44_encode::*` paths keep working.
//!
//! Unlike [`crate::jb2_encode`], IW44 has no multi-page container helpers and
//! no experiment-only types, so this shim is a pure re-export with nothing
//! left behind in the monolith.

pub use djvu_iw44::encode::*;
