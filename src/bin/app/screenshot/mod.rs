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
use crate::win::window_shot;

pub(crate) use daemon::run_daemon;
pub(crate) use enable::{
    heal_if_wanted, is_daemon_running, is_enabled, quit, reload_hotkey, set_enabled,
};
pub(crate) use overlay::{capture_instant, run_capture, run_capture_automation, run_capture_ocr};
pub(crate) use upload::{open_hosts_config, run_upload, run_upload_keep, with_busy_pill};

/// Capture-hotkey presets offered in the Settings dropdown, each paired with its
/// packed HOTKEYF/VK value (high byte = HOTKEYF_* modifiers, low byte = virtual
/// key) — the same packing `settings::screenshot_hotkey` stores. Curated to safe,
/// non-conflicting chords (no bare letters that would hijack a global key, and
/// avoiding Win+Shift+S / Alt+PrtScn which the OS already claims).
pub(crate) const SHOT_PRESETS: &[(&str, u32)] = &[
    ("Ctrl + PrtScn", (0x02 << 8) | 0x2C),
    ("PrtScn", 0x2C),
    ("Ctrl + Shift + S", ((0x02 | 0x01) << 8) | 0x53),
    ("Ctrl + Shift + A", ((0x02 | 0x01) << 8) | 0x41),
    ("Ctrl + Shift + 4", ((0x02 | 0x01) << 8) | 0x34),
    ("Ctrl + Alt + S", ((0x02 | 0x04) << 8) | 0x53),
    ("F9", 0x78),
    ("Ctrl + F12", (0x02 << 8) | 0x7B),
];

/// The folder Ctrl+S auto-saves to when the "fixed save folder" option is on: the
/// user's configured folder, or the Desktop when unset — so the default follows the
/// real (known-folder) Desktop instead of a baked-in path. Used by the capture
/// overlay (autosave + the Save-As starting folder) and the Settings display.
pub(crate) fn effective_save_dir() -> String {
    let d = st2k_base::settings::screenshot_save_dir();
    if d.trim().is_empty() {
        unsafe { crate::win::desktop_dir() }
    } else {
        d
    }
}
