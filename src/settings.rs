//! User-configurable settings — the SageThumbs 2K "Options", a faithful port of
//! the original SageThumbs settings (HKCU\Software\SageThumbs) to our own root
//! HKCU\Software\SageThumbs2K.
//!
//! Stored as DWORDs with the SAME value names and defaults as the original
//! (see the legacy `OptionsDlg.cpp` / `SageThumbs.h`), so the behavior is
//! recognizably the same:
//!   - EnableThumbs  (1)   master on/off for the thumbnail provider
//!   - MaxSize       (100) skip files larger than this many MB
//!   - Width/Height  (1024) max generated thumbnail edge, clamped to [32, 1024]
//!   - UseEmbedded   (0)   prefer the image's embedded (EXIF) thumbnail for
//!     small requests — faster, lower quality
//!   - JPEG          (90)  "Convert to JPG" quality (0–100)
//!   - PNG           (9)   "Convert to PNG" compression (0–9)
//!   - EnableMenu    (1)   show the right-click "SageThumbs 2K" menu
//!   - per-extension: <ext>\Enabled (1) — whether that format is hooked
//!
//! Reads are intentionally NOT cached: settings are small, registry reads are
//! microseconds, and each thumbnail request gets a fresh short-lived handler
//! instance — so a change in the Options dialog takes effect immediately for
//! new requests without restarting the surrogate host.

use windows_registry::CURRENT_USER;

/// HKCU root for all our settings (and the per-extension subkeys).
pub const ROOT: &str = r"Software\SageThumbs2K";

// Defaults + bounds, matching the legacy SageThumbs.h constants.
pub const DEFAULT_MAX_FILE_MB: u32 = 100; // FILE_MAX_SIZE
// Raised from the legacy 256/512 (2026-06-22): on Hi-DPI / 4K / large ("jumbo")
// icon views the shell requests thumbnails well past 512px. Capping below the
// requested size handed back an undersized bitmap the shell could neither display
// crisply NOR durably cache — so it re-extracted on every refresh (an expensive
// 4K video-frame decode each time). We honor the request up to 1024 now; small
// views are unaffected (the provider still does `cx.min(max_thumb)`).
pub const DEFAULT_THUMB_SIZE: u32 = 1024; // THUMB_STORE_SIZE (was 256)
pub const THUMB_MIN: u32 = 32; // THUMB_MIN_SIZE
pub const THUMB_MAX: u32 = 1024; // THUMB_MAX_SIZE (was 512)
pub const EMBEDDED_MAX_REQUEST: u32 = 96; // THUMB_EMBEDDED_MIN_SIZE
pub const DEFAULT_JPEG: u32 = 90; // JPEG_DEFAULT
pub const DEFAULT_PNG: u32 = 9; // PNG_DEFAULT
/// Default classic-menu preview placement: `1` = at the top of the SageThumbs
/// submenu (how the original SageThumbs showed its preview). The SINGLE source of
/// truth for both the first-run getter default ([`menu_preview`]) and the Options
/// dialog's "Defaults" button, so the two can't disagree (they used to: the getter
/// defaulted to 1 while "Defaults" selected 2).
pub const DEFAULT_MENU_PREVIEW: u32 = 1;

fn get_dword(name: &str, default: u32) -> u32 {
    CURRENT_USER
        .open(ROOT)
        .and_then(|k| k.get_u32(name))
        .unwrap_or(default)
}

/// Write a DWORD setting (creating the root key if needed). Best-effort.
pub fn set_dword(name: &str, value: u32) -> windows_registry::Result<()> {
    CURRENT_USER.create(ROOT)?.set_u32(name, value)
}

/// One-time flag: `false` until the app has reported a fresh install once, then `true`
/// forever. A plain boolean — NOT a per-machine identifier.
pub fn install_reported() -> bool {
    get_dword("InstallReported", 0) != 0
}

/// Mark the fresh-install report as sent (see [`install_reported`]). Best-effort.
pub fn set_install_reported() {
    let _ = set_dword("InstallReported", 1);
}

/// The version last installed, left as a single "tombstone" value by the uninstaller after
/// it wipes the rest of [`ROOT`]. Its presence on a fresh install means this machine had us
/// before — a reinstall, not a first-time user. A plain version string, NOT an identifier.
pub fn tombstone_version() -> Option<String> {
    CURRENT_USER
        .open(ROOT)
        .ok()
        .and_then(|k| k.get_string("Tombstone").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Drop the reinstall tombstone once it has been reported, so a reinstall is recognized at
/// most once (the next fresh report — if any — looks like a first-time install again).
pub fn clear_tombstone() {
    if let Ok(k) = CURRENT_USER.open(ROOT) {
        let _ = k.remove_value("Tombstone");
    }
}

/// The UI-language override (e.g. "fr", "zh-TW"), or None to follow the system
/// UI language. Set by the Options dialog's language picker.
pub fn lang_override() -> Option<String> {
    CURRENT_USER
        .open(ROOT)
        .and_then(|k| k.get_string("Lang"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Persist the language override; an empty string clears it (= follow system).
pub fn set_lang(code: &str) -> windows_registry::Result<()> {
    CURRENT_USER.create(ROOT)?.set_string("Lang", code)
}

// ---- Ebook/comic archive cover-selection (CBZ/CB7/CBR) -------------------
// Ports DarkThumbs' CBXManager toggles. Defaults: natural-sort ON, prefer a
// "cover"-named image ON, skip scanlation filler (credits/logos) OFF.

/// Pick archive pages in natural sort order (else first in archive order).
pub fn container_sort() -> bool {
    get_dword("ContainerSort", 1) != 0
}
/// Prefer an image whose name contains "cover".
pub fn container_prefer_cover() -> bool {
    get_dword("ContainerPreferCover", 1) != 0
}
/// Skip scanlation filler pages (credits/logo/recruit/invite).
pub fn container_skip_scanlation() -> bool {
    get_dword("ContainerSkipScanlation", 0) != 0
}

// ---- Thumbnail-generation settings (read by the provider/decoder) -------

/// Master switch for the thumbnail provider.
pub fn thumbnails_enabled() -> bool {
    get_dword("EnableThumbs", 1) != 0
}

/// Files larger than this are not thumbnailed. `0` removes the user limit, but
/// the provider still caps the in-memory read at a hard ceiling
/// (`decode::limits::MAX_INPUT_BYTES`, currently 256 MiB), so "unlimited"
/// effectively means "up to that ceiling".
pub fn max_file_size_bytes() -> u64 {
    let mb = get_dword("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    if mb == 0 {
        u64::MAX
    } else {
        mb * 1024 * 1024
    }
}

/// Reduce a stored Width/Height pair to a single thumbnail edge in the legacy
/// [THUMB_MIN, THUMB_MAX] range: take the larger of the two so either knob
/// raises the ceiling, then clamp. Pure so it can be tested without HKCU.
pub(crate) fn clamp_thumb_size(w: u32, h: u32) -> u32 {
    w.max(h).clamp(THUMB_MIN, THUMB_MAX)
}

/// The max thumbnail edge to generate, clamped to the [32, 1024] range.
/// The original stored Width/Height separately; we cap the square request box
/// at the larger of the two so either knob raises the ceiling.
pub fn max_thumb_size() -> u32 {
    let w = get_dword("Width", DEFAULT_THUMB_SIZE);
    let h = get_dword("Height", DEFAULT_THUMB_SIZE);
    clamp_thumb_size(w, h)
}

/// Prefer the image's embedded (EXIF) thumbnail when the request is small (<= 96px).
/// ON by default: for a small tile of a 12-50 MP photo, grabbing the camera-baked ~160px
/// thumbnail is sub-millisecond vs a full multi-megapixel decode + downscale, and at that
/// size it's visually identical. Falls back to a full decode when no embedded thumb exists.
/// Users who want byte-exact small tiles can turn it off in Settings.
pub fn use_embedded() -> bool {
    get_dword("UseEmbedded", 1) != 0
}

/// A snapshot of the four settings every `GetThumbnail` consults, read with a
/// SINGLE HKCU key open instead of one open per getter. The provider used to call
/// [`thumbnails_enabled`], [`max_file_size_bytes`], [`max_thumb_size`] (which opens
/// twice) and [`use_embedded`] separately — ~5 `RegOpenKeyEx`es on the hot path,
/// per file, in a folder of thousands of thumbnails. Pulling them all from one open
/// key collapses that to one open. Semantics are UNCHANGED: it's still a fresh read
/// per `GetThumbnail` (a fresh provider instance per request — see the module docs),
/// so a Settings change still takes effect immediately for the next thumbnail; we
/// only stop re-opening the same key five times within a single request.
pub struct ThumbSettings {
    /// `EnableThumbs` — master on/off for the provider.
    pub enabled: bool,
    /// `MaxSize` resolved to bytes (`u64::MAX` when the user limit is 0/unlimited).
    pub max_file_bytes: u64,
    /// `Width`/`Height` reduced + clamped to the [32, 1024] edge.
    pub max_thumb: u32,
    /// `UseEmbedded` — prefer the embedded thumbnail for small requests.
    pub use_embedded: bool,
}

/// Read the per-`GetThumbnail` settings in one HKCU key open. Missing values fall
/// back to the same defaults the individual getters use, so the result is identical
/// to calling them one by one — just without the repeated opens.
pub fn thumb_settings() -> ThumbSettings {
    let key = CURRENT_USER.open(ROOT).ok();
    let g = |name: &str, default: u32| {
        key.as_ref().and_then(|k| k.get_u32(name).ok()).unwrap_or(default)
    };
    let mb = g("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    ThumbSettings {
        enabled: g("EnableThumbs", 1) != 0,
        max_file_bytes: if mb == 0 { u64::MAX } else { mb * 1024 * 1024 },
        max_thumb: clamp_thumb_size(g("Width", DEFAULT_THUMB_SIZE), g("Height", DEFAULT_THUMB_SIZE)),
        use_embedded: g("UseEmbedded", 1) != 0,
    }
}

// ---- Convert-verb quality settings --------------------------------------

/// Clamp a stored JPEG quality DWORD into the 0..=100 byte range. Pure so it
/// can be tested without HKCU.
pub(crate) fn clamp_quality(q: u32) -> u8 {
    q.min(100) as u8
}

/// Clamp a stored PNG compression DWORD into the legacy 0..=9 zlib range. Pure
/// so it can be tested without HKCU.
pub(crate) fn clamp_png(l: u32) -> u32 {
    l.min(9)
}

/// "Convert to JPG" quality, 0–100.
pub fn jpeg_quality() -> u8 {
    clamp_quality(get_dword("JPEG", DEFAULT_JPEG))
}

/// "Convert to PNG" compression level, 0–9 (legacy zlib scale).
pub fn png_level() -> u32 {
    clamp_png(get_dword("PNG", DEFAULT_PNG))
}

// ---- Convert… dialog per-format export settings (persisted) --------------
// The Convert dialog's per-format Settings popup (JPEG quality / PNG level /
// WebP quality+lossless) used to live in process-only statics that reset every
// launch. These persist them under their own HKCU keys (separate from the global
// thumbnail JPEG/PNG above) so a user's chosen export quality survives restarts.
// Defaults match the dialog's historical static defaults (JPEG 90 / WebP 80,
// lossy / PNG 6).

/// Convert dialog: JPEG export quality, 1–100.
pub fn cv_jpeg_quality() -> u32 {
    get_dword("CvJpegQuality", 90).clamp(1, 100)
}
/// Convert dialog: lossy-WebP export quality, 1–100.
pub fn cv_webp_quality() -> u32 {
    get_dword("CvWebpQuality", 80).clamp(1, 100)
}
/// Convert dialog: encode WebP losslessly (else lossy at [`cv_webp_quality`]).
pub fn cv_webp_lossless() -> bool {
    get_dword("CvWebpLossless", 0) != 0
}
/// Convert dialog: PNG compression level, 0–9.
pub fn cv_png_level() -> u32 {
    get_dword("CvPngLevel", 6).clamp(0, 9)
}

/// Persist the Convert dialog's per-format settings (best-effort; clamped).
pub fn set_cv_settings(jpeg_quality: u32, webp_quality: u32, webp_lossless: bool, png_level: u32) {
    let _ = set_dword("CvJpegQuality", jpeg_quality.clamp(1, 100));
    let _ = set_dword("CvWebpQuality", webp_quality.clamp(1, 100));
    let _ = set_dword("CvWebpLossless", webp_lossless as u32);
    let _ = set_dword("CvPngLevel", png_level.clamp(0, 9));
}

// ---- Menu setting -------------------------------------------------------

/// Show the right-click "SageThumbs 2K" menu.
pub fn menu_enabled() -> bool {
    get_dword("EnableMenu", 1) != 0
}

/// Show the menu on ANY file (not just supported images/audio). When on, an UNSUPPORTED
/// selection still gets a CONDENSED menu — only the file-agnostic utilities (Files to
/// folder · Sort into folders · Rename · Pick color) + Settings (see
/// [`crate::verbs::condensed_top_level`]). OFF by default — the menu stays on supported
/// formats only unless the user wants it everywhere.
pub fn menu_all_file_types() -> bool {
    get_dword("MenuAllFileTypes", 0) != 0
}

/// Thumbnail preview inside the classic right-click menu (single image
/// selection): 0 = off, 1 = at the top of the SageThumbs submenu,
/// 2 = directly on the main context menu.
///
/// Default: 1 (at the top of the SageThumbs submenu) — this is how the original
/// SageThumbs showed its preview, so long-time users get the familiar behavior and
/// we don't crowd the main right-click menu out of the box. It's owner-drawn (the
/// only way to make a menu row tall enough for the image) but the menu still renders
/// in the system theme (dark stays dark); see [`crate::contextmenu`]. Users who want
/// it directly on the main menu (2) or off (0) can change it in Settings.
pub fn menu_preview() -> u32 {
    get_dword("MenuPreview", DEFAULT_MENU_PREVIEW).min(2)
}

/// Surface the most-used verbs (Convert into / Resize / Rotate) directly on the
/// MAIN right-click menu (above the SageThumbs submenu), so they're one click
/// instead of two. OFF by default — the original SageThumbs kept everything inside
/// its submenu, so we don't crowd the main menu unless the user opts in.
pub fn menu_quick_verbs() -> bool {
    get_dword("MenuQuickVerbs", 0) != 0
}

/// Draw a subtle checkerboard behind the menu preview's transparent areas, so a
/// transparent (or white-on-transparent) image doesn't vanish into the flat menu
/// background. On by default.
pub fn preview_checker() -> bool {
    get_dword("PreviewChecker", 1) != 0
}

/// Preserve the source file's date/time on saved outputs (Convert / Resize /
/// Rotate). Off by default — saved files get the current time, like most tools.
pub fn preserve_file_date() -> bool {
    get_dword("PreserveFileDate", 0) != 0
}

// ---- Screenshot capture hotkey ------------------------------------------
// The opt-in screenshot daemon's global hotkey, stored in the native Win32
// "hotkey control" packing: high byte = HOTKEYF_* modifiers (SHIFT 0x01,
// CONTROL 0x02, ALT 0x04), low byte = virtual-key code. The daemon converts
// these to RegisterHotKey's MOD_* flags. Default: Ctrl + PrtScn — matching the
// behavior before the hotkey became configurable.

/// Default capture hotkey: Ctrl + PrtScn, in packed HOTKEYF/VK form.
pub const DEFAULT_SHOT_HOTKEY: u32 = (0x02 << 8) | 0x2C; // HOTKEYF_CONTROL | VK_SNAPSHOT

/// The screenshot capture hotkey as `(hotkeyf_mods, vk)`.
pub fn screenshot_hotkey() -> (u32, u32) {
    let v = get_dword("ScreenshotHotkey", DEFAULT_SHOT_HOTKEY);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the capture hotkey (packed HOTKEYF/VK; only the low 16 bits are kept).
pub fn set_screenshot_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("ScreenshotHotkey", packed & 0xFFFF)
}

/// The OPTIONAL "quick-save" capture hotkey as `(hotkeyf_mods, vk)` — a second,
/// editor-less hotkey that grabs the whole screen straight to the clipboard + a
/// PNG. Default `0` (vk == 0) means **disabled** (no second hotkey registered);
/// the daemon skips registration when vk is 0, so it stays off until the user
/// picks a chord in Settings.
pub fn screenshot_quick_hotkey() -> (u32, u32) {
    let v = get_dword("ScreenshotQuickHotkey", 0);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the quick-save hotkey (packed HOTKEYF/VK; `0` = disabled).
pub fn set_screenshot_quick_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("ScreenshotQuickHotkey", packed & 0xFFFF)
}

/// Hide the screenshot daemon's notification-area (tray) icon. Off by default —
/// the icon makes the feature discoverable and offers Capture / Settings / Quit.
/// When hidden the hotkey still fires; manage the service from the Settings app.
pub fn screenshot_hide_tray() -> bool {
    get_dword("ScreenshotHideTray", 0) != 0
}

// ---- Screenshot save destination (Ctrl+S in the capture overlay) --------

/// When ON, Ctrl+S (and the Save button) in the capture overlay auto-saves the PNG to
/// [`screenshot_save_dir`] (default: the Desktop). When OFF, Ctrl+S prompts for a
/// location each time. OFF by default — the capture asks where to save unless the user
/// opts into a fixed folder.
pub fn screenshot_use_save_dir() -> bool {
    get_dword("ShotUseSaveDir", 0) != 0
}

/// Persist the "use a fixed save folder" toggle.
pub fn set_screenshot_use_save_dir(on: bool) -> windows_registry::Result<()> {
    set_dword("ShotUseSaveDir", on as u32)
}

/// The folder Ctrl+S auto-saves to when [`screenshot_use_save_dir`] is on. An empty
/// string means "unset" — the app resolves that to the Desktop known folder at use
/// time (so we never bake an absolute path here, and it follows the user's real
/// Desktop). See `crate`'s app `screenshot::effective_save_dir`.
pub fn screenshot_save_dir() -> String {
    CURRENT_USER
        .open(ROOT)
        .and_then(|k| k.get_string("ShotSaveDir"))
        .unwrap_or_default()
}

/// Persist the chosen save folder (absolute path). Empty restores the Desktop default.
pub fn set_screenshot_save_dir(dir: &str) -> windows_registry::Result<()> {
    CURRENT_USER.create(ROOT)?.set_string("ShotSaveDir", dir)
}

// ---- Diagnostics --------------------------------------------------------

/// Verbose ("Debug") logging — when on, `safety::log_debug` traces are written to the
/// diagnostics log alongside the always-on errors/crashes. Off by default; the same
/// `Debug` DWORD `dev-register.ps1 -Debug` sets, now also toggleable in the Options
/// dialog so a user can capture detail for a bug report and turn it back off.
pub fn verbose_logging() -> bool {
    get_dword("Debug", 0) != 0
}

// ---- Updates ------------------------------------------------------------

/// Whether the resident screenshot helper periodically checks for a newer release
/// (throttled to once/day) and pops a tray toast when one exists. ON by default, but
/// only has any effect while the screenshot helper is actually running — that already-
/// resident process does the check, so there is NO separate scheduled task or service.
pub fn update_auto_check() -> bool {
    get_dword("UpdateAutoCheck", 1) != 0
}

/// Persist the auto-update-check toggle.
pub fn set_update_auto_check(on: bool) -> windows_registry::Result<()> {
    set_dword("UpdateAutoCheck", on as u32)
}

// ---- Per-extension enable (read by registration) ------------------------

/// Whether a given extension (no dot, lowercase) is hooked. Enabled unless an
/// explicit `0` is stored under `…\SageThumbs2K\<ext>\Enabled`.
///
/// SEMANTICS NOTE: although this flag lives in HKCU, it is read at (elevated)
/// (re-)registration time to drive MACHINE-WIDE HKCR registration, so toggling
/// a format here enables/disables that format's thumbnails for ALL users — it
/// is an "all users" switch, not a per-user one (there is no per-user gate).
pub fn format_enabled(ext: &str) -> bool {
    CURRENT_USER
        .open(format!(r"{ROOT}\{ext}"))
        .and_then(|k| k.get_u32("Enabled"))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// Persist a per-extension enable flag (used by the Options dialog).
pub fn set_format_enabled(ext: &str, enabled: bool) -> windows_registry::Result<()> {
    CURRENT_USER
        .create(format!(r"{ROOT}\{ext}"))?
        .set_u32("Enabled", enabled as u32)
}

// ---- Per-menu-item visibility (the "Displayed menu items" checklist) -----

/// Whether a top-level context-menu item (by its MENU title key, e.g.
/// `menu_convert_into`) is shown. All shown by default; the Settings checklist
/// can hide ones the user never uses. Stored under `…\SageThumbs2K\MenuItems\<key>`.
pub fn menu_item_shown(key: &str) -> bool {
    CURRENT_USER
        .open(format!(r"{ROOT}\MenuItems"))
        .and_then(|k| k.get_u32(key))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// Persist a top-level menu item's visibility (used by the Options dialog).
pub fn set_menu_item_shown(key: &str, shown: bool) -> windows_registry::Result<()> {
    CURRENT_USER.create(format!(r"{ROOT}\MenuItems"))?.set_u32(key, shown as u32)
}

/// The user's custom top-level menu order — a list of menu-item title keys, top to
/// bottom — or empty for the default tree order. Stored comma-joined under
/// `…\SageThumbs2K\MenuOrder` (the keys are `menu_*` identifiers, never contain a
/// comma). The classic menu builder applies it via `verbs::ordered_top_level`.
pub fn menu_order() -> Vec<String> {
    CURRENT_USER
        .open(ROOT)
        .and_then(|k| k.get_string("MenuOrder"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

/// Persist the custom menu order (comma-joined keys); an empty slice clears it
/// (= back to the default tree order).
pub fn set_menu_order(keys: &[&str]) -> windows_registry::Result<()> {
    CURRENT_USER.create(ROOT)?.set_string("MenuOrder", keys.join(","))
}

/// A one-shot snapshot of the menu-item visibility subkey. Building the right-click
/// menu calls [`menu_item_shown`] once per node (~one HKCU open + `format!` alloc
/// each); on a per-right-click hot path inside explorer.exe that adds up. Open
/// `…\MenuItems` ONCE at the top of `QueryContextMenu` / `EnumSubCommands` and ask
/// [`MenuVisibility::shown`] per item instead — same semantics, ~N opens collapse
/// to one. A fresh snapshot per menu build keeps the live-toggle contract (§ module
/// docs) intact — we don't cache across builds.
pub struct MenuVisibility(Option<windows_registry::Key>);

/// Open the menu-visibility subkey once for the current menu build. `None` (subkey
/// absent — nothing ever hidden) makes every [`MenuVisibility::shown`] return true.
pub fn menu_visibility() -> MenuVisibility {
    MenuVisibility(CURRENT_USER.open(format!(r"{ROOT}\MenuItems")).ok())
}

impl MenuVisibility {
    /// Whether `key` (a top-level menu item title) is shown — default true unless an
    /// explicit `0` is stored. Identical to [`menu_item_shown`], reusing the open key.
    pub fn shown(&self, key: &str) -> bool {
        // Shown by default; hidden only when an explicit `0` is stored. (`matches!`
        // keeps this MSRV-1.80-safe — `is_none_or` would need 1.82.)
        !matches!(self.0.as_ref().and_then(|k| k.get_u32(key).ok()), Some(0))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // The clamps are tested hermetically through the PURE helpers below, with
    // explicit out-of-range inputs — no dependency on whatever happens to be in
    // the live HKCU (where a test that only reads the getter could never fail).

    #[test]
    fn clamp_thumb_size_enforces_legacy_range() {
        // Below the floor (incl. the disabled/zero value) snaps up to THUMB_MIN.
        assert_eq!(clamp_thumb_size(0, 0), THUMB_MIN);
        assert_eq!(clamp_thumb_size(1, 1), THUMB_MIN);
        assert_eq!(clamp_thumb_size(THUMB_MIN - 1, 0), THUMB_MIN);
        // Above the ceiling (incl. an absurd u32::MAX) snaps down to THUMB_MAX.
        assert_eq!(clamp_thumb_size(THUMB_MAX + 1, 0), THUMB_MAX);
        assert_eq!(clamp_thumb_size(u32::MAX, u32::MAX), THUMB_MAX);
        // The endpoints survive unchanged.
        assert_eq!(clamp_thumb_size(THUMB_MIN, THUMB_MIN), THUMB_MIN);
        assert_eq!(clamp_thumb_size(THUMB_MAX, THUMB_MAX), THUMB_MAX);
        // A mid-range value passes through.
        assert_eq!(clamp_thumb_size(DEFAULT_THUMB_SIZE, DEFAULT_THUMB_SIZE), DEFAULT_THUMB_SIZE);
        // The larger edge wins, then is clamped.
        assert_eq!(clamp_thumb_size(THUMB_MIN, 200), 200);
        assert_eq!(clamp_thumb_size(40, u32::MAX), THUMB_MAX);
        // Whatever the inputs, the result is always inside the documented range.
        for (w, h) in [(0, 0), (1, 7), (300, 9), (u32::MAX, 0), (THUMB_MAX, THUMB_MIN)] {
            let s = clamp_thumb_size(w, h);
            assert!((THUMB_MIN..=THUMB_MAX).contains(&s), "clamp_thumb_size({w},{h}) = {s}");
        }
    }

    #[test]
    fn clamp_quality_caps_at_100() {
        assert_eq!(clamp_quality(0), 0);
        assert_eq!(clamp_quality(DEFAULT_JPEG), DEFAULT_JPEG as u8);
        assert_eq!(clamp_quality(100), 100);
        // Over 100 is pinned to 100 (and must not wrap when cast to u8).
        assert_eq!(clamp_quality(101), 100);
        assert_eq!(clamp_quality(256), 100); // would be 0 if it wrapped at the cast
        assert_eq!(clamp_quality(u32::MAX), 100);
    }

    #[test]
    fn clamp_png_caps_at_9() {
        assert_eq!(clamp_png(0), 0);
        assert_eq!(clamp_png(DEFAULT_PNG), DEFAULT_PNG);
        assert_eq!(clamp_png(9), 9);
        // Over 9 is pinned to 9.
        assert_eq!(clamp_png(10), 9);
        assert_eq!(clamp_png(u32::MAX), 9);
    }

    // The public getters delegate to the pure clamps, so their output is bounded
    // for whatever is (or isn't) in the live HKCU; this just confirms the wiring
    // holds and never panics.
    #[test]
    fn public_getters_stay_within_bounds() {
        let s = max_thumb_size();
        assert!((THUMB_MIN..=THUMB_MAX).contains(&s), "max_thumb_size = {s}");
        assert!(jpeg_quality() <= 100);
        assert!(png_level() <= 9);
    }

    #[test]
    fn unknown_format_defaults_enabled() {
        // A made-up extension nobody configured is enabled by default.
        assert!(format_enabled("zzz_definitely_not_configured"));
    }
}
