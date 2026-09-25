//! The `base` layer of SageThumbs 2K's library (see scripts/refactor/crate_layers.py).
//! It names only the layers below it, so an edit above it never recompiles it.

#![allow(non_snake_case)]
// Compiled into the shell-extension DLL, which runs inside explorer.exe under
// `panic = "abort"`: no `.unwrap()`/`.expect()` outside tests (see the core crate).
#![warn(clippy::unwrap_used, clippy::expect_used)]

pub mod checkerpx;
pub mod clipboard;
pub mod dib;
// Bounded, process-local memory of thumbnail decode failures (a circuit breaker), so a
// hostile or broken file already known to fail is not re-decoded on every redraw.
pub mod failmemo;
pub mod formats;
// `pub` (hidden) so the app EXE's settings export path (`settings_io::export_settings_to_path`,
// 2026-09-05 audit, F13) can reuse `write_atomically` rather than growing its own copy -
// same arrangement as `ocr`/`parallel`: an internal helper, not a stable public API.
#[doc(hidden)]
pub mod fsutil;
pub mod guids;
pub mod hex;
// The DLL's own module state and the helpers every layer shares.
pub mod host;
pub mod i18n;
pub mod licence_state;
// Internal batch thread pool (Convert dialog / Combine / multi-file context-menu
// verbs). `pub` so the companion `SageThumbs2K` app bin can drive it, `doc(hidden)`
// because it isn't a stable public API — just a shared helper across our own crates.
#[doc(hidden)]
pub mod parallel;
pub mod safety;
pub mod settings;
pub mod shellcmd;
// Shared read-only SQLite low-level primitives (varint/serial-size/overflow local-size) — one
// copy used by both `container::clip` and the app EXE's `preview::dbdoc`. `pub` (hidden) for the
// same reason as `ocr`/`parallel`/`flv`: `dbdoc` lives in the companion binary crate and needs
// `st2k_base::sqlite_prim` to reach it.
#[doc(hidden)]
pub mod sqlite_prim;
// The one place test code learns where `..\test-corpus` is; public for the integration
// tests and the `vdec` bin, and the pre-push gate's switch for making the corpus vanish.
#[doc(hidden)]
pub mod testcorpus;
pub mod unixtime;
pub mod upload_config;
// The local list of uploaded links and when each one expires (app window + `st2k`).
pub mod upload_history;
