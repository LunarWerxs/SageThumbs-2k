//! The `codecs` layer of SageThumbs 2K's library (see scripts/refactor/crate_layers.py).
//! It names only the layers below it, so an edit above it never recompiles it.

#![allow(non_snake_case)]
// Compiled into the shell-extension DLL, which runs inside explorer.exe under
// `panic = "abort"`: no `.unwrap()`/`.expect()` outside tests (see the core crate).
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod app_image;
pub mod container;
pub mod decode;
// `pub` (hidden) because the `st2k` bin's `flv-frame` child verb reuses the FLV tag walk
// (`flv::scan_flash_keyframe`) — one parser, so the parent's probe and the child's
// extraction can never disagree about what counts as the first Flash-codec keyframe.
#[doc(hidden)]
pub mod flv;
// Structure-aware mutation fuzzing of the pure-Rust parsers, compiled only for tests.
#[cfg(test)]
mod fuzz;
pub mod isobmff;
pub mod jpegtran;
mod mkv;
mod mp4;
pub mod mpeg12;
// In-box WinRT OCR (`Windows.Media.Ocr`). `pub` so the companion `SageThumbs2K` app bin
// can read text out of a screen capture it already holds in memory, `doc(hidden)` because
// it isn't a stable public API — same arrangement as `parallel` below.
#[doc(hidden)]
pub mod ocr;
pub mod pdf;
pub mod streamsrc;
pub mod strip;
// `pub` only so the app EXE's preview player can ask `media_foundation_available()`
// before touching the delay-loaded MF imports; the decode entry points stay internal.
pub mod vcodec;
pub mod video;
// Hidden like `flv`: public only so the `st2k vp9-frame` child (a separate bin crate) and
// its tests can reach the shared caps + keyframe extraction — not a stable API.
#[doc(hidden)]
pub mod vp9;
mod vstream;
