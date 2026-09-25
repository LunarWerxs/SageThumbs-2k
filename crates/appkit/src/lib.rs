//! The `appkit` layer of SageThumbs 2K's app (see scripts/refactor/crate_layers.py).
//! It names only the layers below it, so an edit above it never recompiles it.

#![allow(non_snake_case)]
// The app's Win32 UI code: its `unsafe fn`s share one contract (a live window or device context
// on the thread that owns it), stated where it matters, not repeated on every helper the
// binary calls. They became `pub` only because the binary is a separate crate now.
#![allow(clippy::missing_safety_doc)]

pub mod cred_store;
pub mod dark;
pub mod dialog_hook;
pub mod explorer_selection;
pub mod gdip;
pub mod gif_frames;
pub mod http;
/// Offline licence certificates: the signed, network-free FLOOR under `license`'s relay
/// check. Never a replacement for it — see that module's own docs for why both exist.
mod licence_cert;
pub mod license;
pub mod sponsors;
/// Shared UI Automation scaffolding for a surface built from real child windows (the Settings
/// nav rail today): each item overrides its own `WM_GETOBJECT` on top of its native provider
/// rather than growing a virtual fragment tree. See the module doc for what a second surface
/// (one with no child windows of its own, e.g. an owner-drawn toolbar) would need instead.
pub mod uia;
pub mod update;
pub mod win;
