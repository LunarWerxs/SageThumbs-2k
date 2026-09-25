//! The `screenshot` layer of SageThumbs 2K's app (see scripts/refactor/crate_layers.py).
//! It names only the layers below it, so an edit above it never recompiles it.

#![allow(non_snake_case)]
// The app's Win32 UI code: its `unsafe fn`s share one contract (a live window or device context
// on the thread that owns it), stated where it matters, not repeated on every helper the
// binary calls. They became `pub` only because the binary is a separate crate now.
#![allow(clippy::missing_safety_doc)]

pub mod eyedropper;
pub mod hotkey;
pub mod ocr_result;
pub mod screenshot;
pub mod upload_history_dlg;
pub mod upload_result;
