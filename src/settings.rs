//! User-configurable settings — the SageThumbs 2K "Options", a faithful port of
//! the original SageThumbs settings (HKCU\Software\SageThumbs) to our own root
//! HKCU\Software\SageThumbs2K.
//!
//! Stored as DWORDs with the SAME value names and defaults as the original
//! (see the legacy `OptionsDlg.cpp` / `SageThumbs.h`), so the behavior is
//! recognizably the same:
//!   - EnableThumbs  (1)   master on/off for the thumbnail provider
//!   - MaxSize       (100) skip files larger than this many MB
//!   - Width/Height  (256) generated thumbnail size, clamped to [32, 512]
//!   - UseEmbedded   (0)   prefer the image's embedded (EXIF) thumbnail for
//!                         small requests — faster, lower quality
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
pub const DEFAULT_THUMB_SIZE: u32 = 256; // THUMB_STORE_SIZE
pub const THUMB_MIN: u32 = 32; // THUMB_MIN_SIZE
pub const THUMB_MAX: u32 = 512; // THUMB_MAX_SIZE
pub const EMBEDDED_MAX_REQUEST: u32 = 96; // THUMB_EMBEDDED_MIN_SIZE
pub const DEFAULT_JPEG: u32 = 90; // JPEG_DEFAULT
pub const DEFAULT_PNG: u32 = 9; // PNG_DEFAULT

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
/// the provider still caps the in-memory read at a hard ceiling (256 MB), so
/// "unlimited" effectively means "up to that ceiling".
pub fn max_file_size_bytes() -> u64 {
    let mb = get_dword("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    if mb == 0 {
        u64::MAX
    } else {
        mb * 1024 * 1024
    }
}

/// The max thumbnail edge to generate, clamped to the legacy [32, 512] range.
/// The original stored Width/Height separately; we cap the square request box
/// at the larger of the two so either knob raises the ceiling.
pub fn max_thumb_size() -> u32 {
    let w = get_dword("Width", DEFAULT_THUMB_SIZE);
    let h = get_dword("Height", DEFAULT_THUMB_SIZE);
    w.max(h).clamp(THUMB_MIN, THUMB_MAX)
}

/// Prefer the image's embedded (EXIF) thumbnail when the request is small.
pub fn use_embedded() -> bool {
    get_dword("UseEmbedded", 0) != 0
}

// ---- Convert-verb quality settings --------------------------------------

/// "Convert to JPG" quality, 0–100.
pub fn jpeg_quality() -> u8 {
    get_dword("JPEG", DEFAULT_JPEG).min(100) as u8
}

/// "Convert to PNG" compression level, 0–9 (legacy zlib scale).
pub fn png_level() -> u32 {
    get_dword("PNG", DEFAULT_PNG).min(9)
}

// ---- Menu setting -------------------------------------------------------

/// Show the right-click "SageThumbs 2K" menu.
pub fn menu_enabled() -> bool {
    get_dword("EnableMenu", 1) != 0
}

/// Thumbnail preview inside the classic right-click menu (single image
/// selection): 0 = off, 1 = at the top of the SageThumbs submenu,
/// 2 = directly on the main context menu. Default: submenu (1).
pub fn menu_preview() -> u32 {
    get_dword("MenuPreview", 1).min(2)
}

/// Surface the most-used verbs (Convert into / Resize / Rotate) directly on the
/// MAIN right-click menu (above the SageThumbs submenu), so they're one click
/// instead of two. Off by default.
pub fn menu_quick_verbs() -> bool {
    get_dword("MenuQuickVerbs", 0) != 0
}

// ---- Per-extension enable (read by registration) ------------------------

/// Whether a given extension (no dot, lowercase) is hooked. Enabled unless an
/// explicit `0` is stored under `…\SageThumbs2K\<ext>\Enabled`.
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

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the read/clamp logic against whatever is (or isn't) in the
    // live HKCU — they must not panic and must honor the documented bounds.
    #[test]
    fn max_thumb_size_is_always_in_range() {
        let s = max_thumb_size();
        assert!((THUMB_MIN..=THUMB_MAX).contains(&s), "got {s}");
    }

    #[test]
    fn quality_values_are_bounded() {
        assert!(jpeg_quality() <= 100);
        assert!(png_level() <= 9);
    }

    #[test]
    fn unknown_format_defaults_enabled() {
        // A made-up extension nobody configured is enabled by default.
        assert!(format_enabled("zzz_definitely_not_configured"));
    }
}
