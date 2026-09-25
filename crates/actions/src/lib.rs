//! The `actions` layer of SageThumbs 2K's library (see scripts/refactor/crate_layers.py).
//! It names only the layers below it, so an edit above it never recompiles it.

#![allow(non_snake_case)]
// Compiled into the shell-extension DLL, which runs inside explorer.exe under
// `panic = "abort"`: no `.unwrap()`/`.expect()` outside tests (see the core crate).
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod topdf;
pub mod verbs;
