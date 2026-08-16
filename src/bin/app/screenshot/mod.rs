//! Flameshot-style screen capture + annotation, kept self-contained in its own
//! module so it stays in one lane rather than spread through the app:
//!
//! - [`tools`]   — the `Tool`/`Shape` model + the (GDI+ anti-aliased) rendering
//! - [`overlay`] — the capture window: freeze screen, region select, annotate
//! - [`toolbar`] — the owner-drawn floating action bar under the selection
//! - [`output`]  — finished capture → clipboard (CF_DIB) + timestamped/temp PNG
//! - [`upload`]  — keyless POST to a no-account host → URL on the clipboard
//! - [`daemon`]  — the opt-in tray + global-hotkey helper that spawns captures
//!
//! `main.rs` only needs `mod screenshot;` and these entry points.

mod daemon;
mod elevwarn; // warns when an elevated window makes the Space hook deaf
mod enable;
#[cfg(feature = "hdr-capture")]
mod hdr;
mod output;
mod overlay;
mod prefs;
mod spacehook; // the WH_KEYBOARD_LL "press Space to preview" hook (Quick preview, Phase 2)
mod toolbar;
mod tools;
mod upload;
mod window_shot;

pub(crate) use daemon::run_daemon;
pub(crate) use enable::{
    heal_if_wanted, is_daemon_running, is_enabled, quit, reload_hotkey, set_enabled,
};
pub(crate) use overlay::{capture_instant, run_capture, run_capture_automation, run_capture_ocr};
pub(crate) use upload::{open_hosts_config, run_upload, run_upload_keep, with_busy_pill};
pub(crate) use window_shot::{capture_hwnd_to_png, downscale_to_width, encode_gif};

/// The folder Ctrl+S auto-saves to when the "fixed save folder" option is on: the
/// user's configured folder, or the Desktop when unset — so the default follows the
/// real (known-folder) Desktop instead of a baked-in path. Used by the capture
/// overlay (autosave + the Save-As starting folder) and the Settings display.
pub(crate) fn effective_save_dir() -> String {
    let d = sagethumbs2k_core::settings::screenshot_save_dir();
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
use sagethumbs2k_core::CREATE_NO_WINDOW;

/// Spawn another instance of ourselves with `args`, fully detached (null stdio, no
/// console). Used everywhere the feature launches a sibling process (capture,
/// daemon, pin, upload, OCR) so each truly outlives its spawner.
///
/// Returns whether the child actually started. Callers that handed the child a temp file
/// to own (the capture PNG the `--upload` / `--ocr` helpers delete after reading)
/// MUST check this: if nobody started, nobody is going to clean it up, and a picture of the
/// user's screen would sit in `%TEMP%` forever.
pub(super) fn spawn_self(args: &[&str]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .is_ok()
}
