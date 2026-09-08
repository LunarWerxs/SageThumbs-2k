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
//!   - FormatBadge   (0)   stamp the format (PSD/JXL/...) in the thumbnail corner
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

use std::sync::OnceLock;

use windows_registry::CURRENT_USER;

/// HKCU root for all our settings (and the per-extension subkeys).
pub const ROOT: &str = r"Software\SageThumbs2K";

pub use store::{ini_path, portable, INI_NAME, ROOT_SECTION as PORTABLE_ROOT_SECTION};

/// The subkey (registry) / section (portable ini) holding per-menu-item visibility.
const MENU_ITEMS: &str = "MenuItems";

/// The registry subkey, under the settings root, that holds the sign-in state on an
/// installed copy: the DPAPI-encrypted OAuth refresh token, the offline licence certificate
/// and the signed-in identity. The app's `cred_store` builds its key path from this constant,
/// so anything it ever stores lands under a name [`is_credential_subkey`] recognises.
pub const CREDENTIAL_SUBKEY: &str = "OAuth";

/// The portable-mode twin of [`CREDENTIAL_SUBKEY`]: on a portable copy the same values live
/// as `OAuth_*` names in the ini's ROOT section, beside every ordinary preference. The
/// `cred_store` builds every portable value name from this prefix, for the same reason.
pub const CREDENTIAL_ROOT_PREFIX: &str = "OAuth_";

/// Whether a subkey (registry) / section (ini) name is the sign-in state rather than a
/// preference. The settings export/import and the doctor's shareable bundle both leave it
/// out. Case-insensitive, as registry key names are.
///
/// Classified by the CONTAINER, never by a list of value names: the credential store
/// writes every credential through one key path and one prefix, so a value added there
/// later is scrubbed by construction, where a hand-written list of names would have to be
/// remembered (2026-09-05 audit, E01). The classification lives here rather than in the
/// app because `st2k doctor` is in this library and has to apply the same rule.
pub fn is_credential_subkey(name: &str) -> bool {
    name.eq_ignore_ascii_case(CREDENTIAL_SUBKEY)
}

/// Whether a ROOT value name is the sign-in state on a portable copy. See
/// [`is_credential_subkey`] for why this is a prefix test and not a list.
pub fn is_credential_root_value(name: &str) -> bool {
    let n = CREDENTIAL_ROOT_PREFIX.len();
    name.len() >= n
        && name.is_char_boundary(n)
        && name[..n].eq_ignore_ascii_case(CREDENTIAL_ROOT_PREFIX)
}

/// Every value in the portable ini's root section (`sub = None`) or a named subkey section.
/// Only meaningful when [`portable`] is true — a registry install walks its own key tree.
/// Exists so the Settings ▸ Diagnostics export/import round-trip works in portable mode
/// instead of silently exporting an empty document.
pub fn portable_values(sub: Option<&str>) -> Vec<(String, String)> {
    store::section_values(sub)
}

/// The names of every subkey section present in the portable ini. See [`portable_values`].
pub fn portable_subkeys() -> Vec<String> {
    store::subkey_names()
}

/// Write one value into the portable ini. See [`portable_values`].
pub fn portable_set(sub: Option<&str>, name: &str, value: &str) -> windows_registry::Result<()> {
    io_result(store::set_string(sub, name, value))
}

/// Remove one value from the portable ini. See [`portable_values`]. Used by the settings
/// import "replace, don't merge" pass to drop a stored name the imported document doesn't
/// carry (item 33/221).
pub fn portable_remove(sub: Option<&str>, name: &str) {
    store::remove_value(sub, name)
}

/// Remove a whole subkey section from the portable ini. See [`portable_subkeys`]. Used by the
/// same import "replace, don't merge" pass to drop a whole section the document doesn't
/// mention at all (item 33/221).
pub fn portable_remove_subkey(name: &str) {
    store::remove_section(name)
}

/// Rewrite the WHOLE portable ini in one load-edit-write under the ini lock, written back
/// atomically (temp file + rename), for the settings import. Its replace-not-merge pass drops
/// and writes many values, and doing that through [`portable_set`]/[`portable_remove`] one
/// value at a time was one load-edit-write per value, each a window in which a crash or a
/// concurrent process saw a half-imported file. One edit over the parsed document means the
/// file on disk is either the old configuration or the fully imported one, never a mix
/// (2026-09-05 audit, F04). `edit` sees sections by name ([`PORTABLE_ROOT_SECTION`] holds
/// what the registry keeps as root values); an unreadable existing file aborts without
/// writing, like every other portable write.
pub fn portable_edit(
    edit: impl FnOnce(
        &mut std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    ),
) -> std::io::Result<()> {
    store::update(edit)
}

/// The HKCU subkey path every settings read/write below opens — normally [`ROOT`], but
/// redirectable to a scratch subkey via the `ST2K_SETTINGS_ROOT` env var for TEST
/// ISOLATION. The in-process integration tests (`tests/explorer_command.rs`,
/// `tests/settings_gate.rs`) load the real DLL, which reads settings from the SAME
/// `HKCU\Software\SageThumbs2K` the developer's own Explorer uses — so without a redirect
/// they either observe the user's customization (menu tests fail spuriously) or have to
/// mutate the live key (the provider gate test). Pointing this at a throwaway subkey makes
/// both hermetic: a test that never writes it sees pure defaults; one that writes it does so
/// in a scratch key it can delete, never touching the user's real settings.
///
/// Resolved ONCE (an env var can't change within a process's life for our purposes) and
/// cached, so the per-`GetThumbnail` hot path — `thumb_settings` in a folder of thousands —
/// pays a single atomic load, not an `env::var` lookup per file. HKLM reads are NOT
/// redirected: they're a different hive, machine-wide, and no test writes them.
fn hkcu_root() -> &'static str {
    static ROOT_PATH: OnceLock<String> = OnceLock::new();
    ROOT_PATH.get_or_init(|| {
        std::env::var("ST2K_SETTINGS_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ROOT.to_string())
    })
}

/// Public window onto [`hkcu_root`], for the ONE other place in the codebase that opens the
/// settings key directly by name rather than through a getter/setter here:
/// `settings_io.rs`'s export/import. It used to open the literal [`ROOT`] constant instead,
/// which silently escaped the `ST2K_SETTINGS_ROOT` test-isolation redirect — the in-process
/// integration tests would export/import the developer's REAL settings even while every other
/// read/write in the same process was safely sandboxed (item 95). Anything that needs "the
/// HKCU subkey settings live under" should call this rather than hand-typing [`ROOT`].
pub fn hkcu_root_path() -> &'static str {
    hkcu_root()
}

mod store;

/// A portable write fails as an [`std::io::Error`], but every public setter here promises a
/// `windows_registry::Result`. Map the file failure onto a generic HRESULT rather than
/// widening ~40 signatures for a case callers already treat as best-effort.
fn io_result(r: std::io::Result<()>) -> windows_registry::Result<()> {
    r.map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))
}

fn get_dword(name: &str, default: u32) -> u32 {
    if store::portable() {
        return store::get_u32(None, name).unwrap_or(default);
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_u32(name))
        .unwrap_or(default)
}

/// Write a DWORD setting (creating the root key if needed). Best-effort.
pub fn set_dword(name: &str, value: u32) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_u32(None, name, value));
    }
    CURRENT_USER.create(hkcu_root())?.set_u32(name, value)
}

/// Delete a DWORD so the setting goes back to TRACKING its default. Best-effort — "absent"
/// is the goal state, so a value that was already missing is success.
///
/// Uses `create`, not `open`, on purpose: `open` hands back a READ-ONLY key and `remove_value`
/// on one fails *silently*. That exact mistake made `typeoverlay::remove_progid` a no-op in
/// production once; the comment there is the full story. `create` on an existing key just
/// opens it for writing.
pub fn remove_dword(name: &str) {
    if store::portable() {
        store::remove_value(None, name);
        return;
    }
    if let Ok(key) = CURRENT_USER.create(hkcu_root()) {
        let _ = key.remove_value(name);
    }
}

/// Persist a DWORD that HAS a default, storing it only when it genuinely differs.
///
/// **A value equal to its default is not a customization.** The nav rail already asserts
/// exactly that (`page_has_non_defaults` drives the "you changed something here" dot), but
/// persistence disagreed: the Settings dialog's `apply()` writes every setting on every OK,
/// touched or not. So the moment anyone opened Settings and clicked OK, they froze a snapshot
/// of whatever the defaults happened to be that day, and **no future default change could ever
/// reach them** — the code would keep reading their stored copy of the old number forever.
///
/// That is not a hypothetical. `MaxSize` shipped in 1.12.0 defaulting to exactly the engine's
/// buffering ceiling, which made the oversized-file rescue unreachable by construction (see
/// [`DEFAULT_MAX_FILE_MB`]). Raising the default repairs it for everyone whose value is ABSENT,
/// and keeping it absent is this function's whole job.
///
/// **Removing rather than merely skipping the write is the load-bearing half.** A user who
/// moves a setting away from the default and then back must end with no stored value, not a
/// stale one — skipping would leave the old number in place and silently ignore the change.
pub fn set_dword_tracking_default(
    name: &str,
    value: u32,
    default: u32,
) -> windows_registry::Result<()> {
    if value == default {
        remove_dword(name);
        return Ok(());
    }
    set_dword(name, value)
}

/// Read an arbitrary string value from the root key, or `None` when unset/empty. The typed
/// accessors below cover everything this module owns; this pair exists for callers that keep
/// their own value in the same root (the screenshot tool's remembered custom colours), so
/// they follow the registry/portable split without each re-implementing it.
pub fn get_string_opt(name: &str) -> Option<String> {
    if store::portable() {
        return store::get_string(None, name).filter(|s| !s.is_empty());
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_string(name))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Write an arbitrary string value into the root key. See [`get_string_opt`].
pub fn set_string(name: &str, value: &str) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, name, value));
    }
    CURRENT_USER.create(hkcu_root())?.set_string(name, value)
}

/// Read a DWORD, distinguishing "absent" (`None`) from a stored value — unlike
/// [`get_dword`], which can't tell a missing key from a key that holds the default.
/// Used by the screenshot-daemon enable migration to tell a never-set flag from an
/// explicit `0`.
pub fn get_dword_opt(name: &str) -> Option<u32> {
    if store::portable() {
        return store::get_u32(None, name);
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_u32(name))
        .ok()
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

/// True on a machine flagged as the developer's own test box (HKCU `DevMachine` DWORD = 1).
/// When set, the app appends `&dev=1` to its startup manifest request. A plain machine-local
/// opt-in flag, not an identifier, absent (the default) on every real install. Set it with
/// [`set_dev_machine`] (or `reg add HKCU\Software\SageThumbs2K /v DevMachine /t REG_DWORD /d 1`).
pub fn is_dev_machine() -> bool {
    get_dword("DevMachine", 0) != 0
}

/// Set or clear the developer-test-box flag (see [`is_dev_machine`]). Best-effort.
pub fn set_dev_machine(on: bool) -> windows_registry::Result<()> {
    set_dword("DevMachine", on as u32)
}

/// The version last installed, left as a single "tombstone" value by the uninstaller after
/// it wipes the rest of [`ROOT`]. Its presence on a fresh install means this machine had us
/// before (a reinstall, not a first-time user). A plain version string.
pub fn tombstone_version() -> Option<String> {
    if store::portable() {
        return store::get_string(None, "Tombstone")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    CURRENT_USER
        .open(hkcu_root())
        .ok()
        .and_then(|k| k.get_string("Tombstone").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Drop the reinstall tombstone once it has been reported, so a reinstall is recognized at
/// most once (the next fresh report — if any — looks like a first-time install again).
pub fn clear_tombstone() {
    if store::portable() {
        store::remove_value(None, "Tombstone");
        return;
    }
    if let Ok(k) = CURRENT_USER.open(hkcu_root()) {
        let _ = k.remove_value("Tombstone");
    }
}

/// The UI-language override (e.g. "fr", "zh-TW"), or None to follow the system
/// UI language. Set by the Options dialog's language picker.
pub fn lang_override() -> Option<String> {
    if store::portable() {
        return store::get_string(None, "Lang").filter(|s| !s.is_empty());
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_string("Lang"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Persist the language override; an empty string clears it (= follow system).
pub fn set_lang(code: &str) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, "Lang", code));
    }
    CURRENT_USER.create(hkcu_root())?.set_string("Lang", code)
}

// Parent-hub imports: `thumbs` (thumbnail/menu/container settings the DLL reads) and
// `app_prefs` (EXE-only viewer/app preference accessors) are glob-imported PRIVATELY so
// this file sees the whole settings surface as one flat namespace, exactly as it did
// when all of this lived in one file. The public surface is then re-exported by NAME
// (a `pub use child::*` would trip the "does not re-export anything public enough" lint
// on the private items each child also defines for its own internal use).
mod app_prefs;
mod thumbs;
// Only the hub's tests name a thumbs item directly (`clamp_thumb_size`); production code in
// this file reaches the children through the by-name re-exports below.
#[cfg(test)]
use thumbs::*;

pub use app_prefs::{
    app_theme, custom_action, custom_action_hotkey, cv_jpeg_quality, cv_magick_quality,
    cv_png_level, cv_webp_lossless, cv_webp_quality, eyedropper_format, eyedropper_history,
    keep_metadata_on_convert, pdf_page, preserve_file_date, preview_arrow_nav,
    preview_close_on_focus_loss, preview_enabled, preview_hold_peek, preview_html, preview_loop,
    preview_markdown, preview_md_remote_img, preview_muted, preview_open_front, preview_pdf_strip,
    preview_speed, preview_text, preview_toc_open, preview_url_live, preview_volume,
    preview_window_size, screenshot_default_tool, screenshot_delay_sec, screenshot_hide_tray,
    screenshot_hotkey, screenshot_quick_hotkey, screenshot_save_dir, screenshot_use_save_dir,
    set_app_theme, set_custom_action, set_custom_action_hotkey, set_cv_magick_quality,
    set_cv_settings, set_eyedropper_format, set_eyedropper_history, set_preview_arrow_nav,
    set_preview_close_on_focus_loss, set_preview_enabled, set_preview_hold_peek, set_preview_html,
    set_preview_loop, set_preview_markdown, set_preview_md_remote_img, set_preview_muted,
    set_preview_open_front, set_preview_speed, set_preview_text, set_preview_toc_open,
    set_preview_url_live, set_preview_volume, set_preview_window_size, set_screenshot_default_tool,
    set_screenshot_delay_sec, set_screenshot_hotkey, set_screenshot_quick_hotkey,
    set_screenshot_save_dir, set_screenshot_use_save_dir, set_update_auto_check, update_auto_check,
    verbose_logging, DEFAULT_CUSTOM_ACTION, DEFAULT_SHOT_HOTKEY, DEFAULT_SHOT_TOOL,
    SHOT_DELAY_STEPS, SHOT_TOOL_COUNT,
};

pub use thumbs::{
    archive_collage, container_prefer_cover, container_skip_scanlation, container_sort,
    corner_mark, folder_prebuild_verb, format_badge, format_badge_icon, format_enabled,
    format_enabled_snapshot, hide_type_overlay, jpeg_quality, max_file_size_bytes, max_thumb_size,
    menu_all_file_types, menu_enabled, menu_gate, menu_item_shown, menu_order, menu_preview,
    menu_quick_verbs, menu_visibility, png_level, prefer_cover_art, preview_checker,
    set_corner_mark, set_folder_prebuild_verb, set_format_badge_icon, set_format_enabled,
    set_menu_item_shown, set_menu_order, set_prefer_cover_art, set_thumb_checker,
    set_video_offset_pct, thumb_checker, thumb_settings, thumbnails_enabled, use_embedded,
    video_offset_frac, video_offset_pct, CornerMark, FormatEnabledSnapshot, MenuGate,
    MenuVisibility, ThumbSettings, DEFAULT_JPEG, DEFAULT_MAX_FILE_MB, DEFAULT_MENU_PREVIEW,
    DEFAULT_PNG, DEFAULT_THUMB_SIZE, DEFAULT_VIDEO_OFFSET_PCT, EMBEDDED_MAX_REQUEST, THUMB_MAX,
    THUMB_MIN, VIDEO_OFFSET_PCT_MAX,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the export/import and the doctor bundle both scrub by (2026-09-05 audit,
    /// E01): the sign-in state is recognised by its container, case-insensitively, and a
    /// name that merely resembles it is not swept up (a `Sub` value in the root, a section
    /// called `OAuthTokens`), since a false positive here silently drops a real preference.
    #[test]
    fn credential_state_is_classified_by_container_not_by_value_name() {
        assert!(is_credential_subkey("OAuth"));
        assert!(is_credential_subkey("oauth"));
        assert!(!is_credential_subkey("OAuthTokens"));
        assert!(!is_credential_subkey("MenuItems"));
        assert!(!is_credential_subkey(""));

        assert!(is_credential_root_value("OAuth_RefreshToken"));
        assert!(is_credential_root_value("oauth_licencecert"));
        assert!(is_credential_root_value("OAuth_Whatever_Comes_Next"));
        assert!(!is_credential_root_value("OAuth"));
        assert!(!is_credential_root_value("Sub"));
        assert!(!is_credential_root_value("Theme"));
        assert!(!is_credential_root_value("\u{e9}Auth_"));
        assert_eq!(
            format!("{CREDENTIAL_SUBKEY}_"),
            CREDENTIAL_ROOT_PREFIX,
            "the two backends must name the same container"
        );
    }

    // The clamps are tested hermetically through the PURE helpers below, with
    // explicit out-of-range inputs — no dependency on whatever happens to be in
    // the live HKCU (where a test that only reads the getter could never fail).

    /// The point of issue #26.5 was that the ceiling had to rise ABOVE the default, so a user
    /// on a 4K screen can opt into bigger tiles while everyone else keeps paying 1024. A change
    /// that quietly pinned the two back together would restore the complaint with the constants
    /// still looking configurable.
    ///
    /// The two relationships between constants are `const` assertions rather than runtime ones:
    /// they are decidable at compile time, so a bad edit should fail the BUILD rather than wait
    /// for someone to run the tests.
    const _: () = assert!(
        THUMB_MAX > DEFAULT_THUMB_SIZE,
        "THUMB_MAX must leave headroom above DEFAULT_THUMB_SIZE, or the setting cannot be raised",
    );
    /// The ceiling must stay under the decoders' own bomb guard, which is the real technical
    /// limit; past it every raised request would be refused rather than honoured.
    const _: () = assert!(THUMB_MAX < crate::decode::limits::MAX_DIM);

    #[test]
    fn the_thumbnail_ceiling_reaches_the_size_the_issue_asked_for() {
        assert_eq!(
            clamp_thumb_size(2560, 2560),
            2560,
            "2560 is the size the issue asked for; it must survive the clamp",
        );
    }

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
        assert_eq!(
            clamp_thumb_size(DEFAULT_THUMB_SIZE, DEFAULT_THUMB_SIZE),
            DEFAULT_THUMB_SIZE
        );
        // The larger edge wins, then is clamped.
        assert_eq!(clamp_thumb_size(THUMB_MIN, 200), 200);
        assert_eq!(clamp_thumb_size(40, u32::MAX), THUMB_MAX);
        // Whatever the inputs, the result is always inside the documented range.
        for (w, h) in [
            (0, 0),
            (1, 7),
            (300, 9),
            (u32::MAX, 0),
            (THUMB_MAX, THUMB_MIN),
        ] {
            let s = clamp_thumb_size(w, h);
            assert!(
                (THUMB_MIN..=THUMB_MAX).contains(&s),
                "clamp_thumb_size({w},{h}) = {s}"
            );
        }
    }

    /// Item 104: `0` used to pass through unchanged (a JPEG quality of 0 persists as a
    /// near-blank file, a UX trap rather than a crash) — this test's own expectation changed
    /// from asserting `clamp_quality(0) == 0` to asserting the floor, which is the fix.
    #[test]
    fn clamp_quality_stays_within_one_to_hundred() {
        assert_eq!(clamp_quality(0), 1, "0 must be refused, not stored as-is");
        assert_eq!(clamp_quality(1), 1);
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

#[cfg(test)]
mod tracking_default_tests {
    use super::*;

    /// A tuning number equal to its default must leave NO stored value behind, and a value
    /// changed away and back again must remove the stale one rather than skip the write.
    ///
    /// This is the mechanism that lets a default ever be reconsidered. The Settings dialog
    /// writes every setting on every OK whether or not it was touched, so before this a plain
    /// `set_dword` froze each value at the default of the day the user first pressed OK, and no
    /// later default change could reach them. `MaxSize` is the case that proved it: it shipped
    /// defaulting to exactly the engine's buffering ceiling, which made the oversized-file
    /// rescue unreachable, and the repair is a raised DEFAULT that only lands where the value
    /// is absent.
    ///
    /// Runs against a scratch HKCU subkey via `ST2K_SETTINGS_ROOT` (see `hkcu_root`), so it
    /// never touches the developer's real settings — and `hkcu_root` caches on first use, so
    /// the variable is set before anything else in this process reads it.
    #[test]
    fn a_value_equal_to_its_default_is_stored_as_absent() {
        const NAME: &str = "St2kTrackingDefaultProbe";
        const DEFAULT: u32 = 4096;

        // Skip rather than fail if another test in this binary already resolved the root: the
        // cache is process-wide and by design, so racing it would be the test's bug, not the
        // code's. In practice `--lib` runs this in its own process alongside pure-helper tests.
        if std::env::var("ST2K_SETTINGS_ROOT").is_err() {
            let scratch = format!(r"{ROOT}\TestScratch{}", std::process::id());
            unsafe { std::env::set_var("ST2K_SETTINGS_ROOT", &scratch) };
        }

        // Start clean, whatever a previous run left.
        remove_dword(NAME);
        assert_eq!(get_dword_opt(NAME), None, "precondition: nothing stored");

        // Equal to the default -> nothing is written, and reads still see the default.
        set_dword_tracking_default(NAME, DEFAULT, DEFAULT).expect("write");
        assert_eq!(
            get_dword_opt(NAME),
            None,
            "a value equal to its default must not be persisted, or the default is frozen"
        );
        assert_eq!(get_dword(NAME, DEFAULT), DEFAULT);
        // ...and it therefore TRACKS a later default change, which is the entire point.
        assert_eq!(
            get_dword(NAME, 9999),
            9999,
            "an absent value follows the default"
        );

        // Different from the default -> stored, and read back exactly.
        set_dword_tracking_default(NAME, 512, DEFAULT).expect("write");
        assert_eq!(get_dword_opt(NAME), Some(512));
        assert_eq!(
            get_dword(NAME, 9999),
            512,
            "an explicit choice outranks the default"
        );

        // Back to the default -> the stale value must be REMOVED, not merely left unwritten.
        // Skipping instead of deleting here is the subtle bug this assertion exists to catch:
        // the user's change back would be silently ignored and 512 would persist forever.
        set_dword_tracking_default(NAME, DEFAULT, DEFAULT).expect("write");
        assert_eq!(
            get_dword_opt(NAME),
            None,
            "moving a setting back to its default must clear the stored override"
        );
        assert_eq!(get_dword(NAME, 9999), 9999);

        remove_dword(NAME);
    }
}
