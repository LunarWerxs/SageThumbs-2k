//! Flameshot-style screen capture + annotation, kept self-contained in its own
//! module so it stays in one lane rather than spread through the app:
//!
//! - [`tools`]   — the `Tool`/`Shape` model + the (GDI+ anti-aliased) rendering
//! - [`gdip`]    — thin GDI+ wrappers giving the tools anti-aliased lines/shapes
//! - [`overlay`] — the capture window: freeze screen, region select, annotate
//! - [`toolbar`] — the owner-drawn floating action bar under the selection
//! - [`output`]  — finished capture → clipboard (CF_DIB) + timestamped/temp PNG
//! - [`upload`]  — keyless POST to a no-account host → URL on the clipboard
//! - [`daemon`]  — the opt-in tray + global-hotkey helper that spawns captures
//!
//! `main.rs` only needs `mod screenshot;` and these entry points.

mod daemon;
mod enable;
mod gdip;
mod output;
mod overlay;
mod prefs;
mod toolbar;
mod tools;
mod upload;

pub(crate) use daemon::run_daemon;
pub(crate) use enable::{is_daemon_running, is_enabled, reload_hotkey, set_enabled};
pub(crate) use overlay::{capture_instant, run_capture};
pub(crate) use upload::run_upload;

/// The folder Ctrl+S auto-saves to when the "fixed save folder" option is on: the
/// user's configured folder, or the Desktop when unset — so the default follows the
/// real (known-folder) Desktop instead of a baked-in path. Used by the capture
/// overlay (autosave + the Save-As starting folder) and the Settings display.
pub(crate) fn effective_save_dir() -> String {
    let d = sagethumbs2k::settings::screenshot_save_dir();
    if d.trim().is_empty() {
        unsafe { crate::win::desktop_dir() }
    } else {
        d
    }
}

use std::os::windows::process::CommandExt;

// Don't flash a console + don't inherit the spawner's stdio handles — otherwise a
// detached background child (the daemon, a pin window) keeps a parent's handle
// alive and can hang a `Start-Process -Wait` (and is just unclean).
use sagethumbs2k::CREATE_NO_WINDOW;

/// Spawn another instance of ourselves with `args`, fully detached (null stdio, no
/// console). Used everywhere the feature launches a sibling process (capture,
/// daemon, pin, upload) so each truly outlives its spawner.
pub(super) fn spawn_self(args: &[&str]) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}
