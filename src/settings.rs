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

pub use store::{ini_path, portable, INI_NAME};

/// The subkey (registry) / section (portable ini) holding per-menu-item visibility.
const MENU_ITEMS: &str = "MenuItems";

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

/// The storage backend every getter/setter in this module goes through.
///
/// Normally that's `HKCU\Software\SageThumbs2K` (see [`hkcu_root`]) and nothing here
/// changes. **Portable mode** is the exception: when a file named [`INI_NAME`] sits next
/// to the running module (the EXE for `st2k`/`SageThumbs2K`, the DLL in the shell host),
/// every read and write goes to that file instead and we touch the registry not at all.
///
/// The marker IS the config file, so a portable build ships one (empty is fine) and an
/// installed build simply never has one — meaning the installed product's behaviour is
/// bit-identical to before this module existed. There is deliberately no setting, flag or
/// env var that turns portable mode on: the file's presence next to the binary is the
/// whole switch, which is what makes "extract the zip somewhere else" work with no state.
///
/// Layout mirrors the registry tree one-for-one — root values live in `[Settings]`, each
/// registry subkey becomes its own section:
///
/// ```ini
/// [Settings]
/// EnableThumbs=1
/// Lang=fr
///
/// [MenuItems]
/// menu_convert_into=0
///
/// [.psd]
/// Enabled=0
/// ```
///
/// Everything is text on disk; [`get_u32`](store::get_u32) parses and
/// [`set_u32`](store::set_u32) writes decimal, so a DWORD round-trips exactly. Reads go
/// straight to the file every time, keeping the module-level promise that an edit takes
/// effect immediately without restarting anything (see [`load`](store::load) for why the
/// obvious cache is not merely unnecessary here but incorrect).
mod store {
    use std::collections::BTreeMap;
    use std::io;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// The file whose presence next to the running module means "portable".
    pub const INI_NAME: &str = "SageThumbs2K.ini";
    /// The section holding what would otherwise be the root key's values.
    const ROOT_SECTION: &str = "Settings";

    /// section -> (value name -> raw text). `BTreeMap` so a rewritten file has a stable,
    /// diffable order rather than whatever the hash seed produced this run.
    type Doc = BTreeMap<String, BTreeMap<String, String>>;

    /// The portable config file, or `None` when we're registry-backed.
    ///
    /// Resolved once. `ST2K_PORTABLE_INI` overrides the probe so tests can exercise the
    /// file backend without planting an ini next to the test binary (and so a developer
    /// can try portable behaviour against a normal build).
    pub fn ini_path() -> Option<&'static PathBuf> {
        static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
        PATH.get_or_init(|| {
            if let Some(p) = std::env::var_os("ST2K_PORTABLE_INI") {
                let p = PathBuf::from(p);
                return (!p.as_os_str().is_empty()).then_some(p);
            }
            // `module_path()` is the DLL inside the shell host and the EXE otherwise —
            // never `current_exe()`, which in the shell host is explorer.exe/dllhost.exe.
            let module = crate::module_path().ok()?;
            let beside = PathBuf::from(module).parent()?.join(INI_NAME);
            beside.is_file().then_some(beside)
        })
        .as_ref()
    }

    /// Whether settings are file-backed (portable) rather than registry-backed.
    pub fn portable() -> bool {
        ini_path().is_some()
    }

    /// Parse an ini. Unknown/blank lines and `;`/`#` comments are skipped; a value before
    /// any `[section]` header is treated as a root value, which makes a hand-written file
    /// that omits the `[Settings]` header still work.
    fn parse(text: &str) -> Doc {
        let mut doc = Doc::new();
        let mut section = ROOT_SECTION.to_string();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_string();
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                doc.entry(section.clone())
                    .or_default()
                    .insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        doc
    }

    fn render(doc: &Doc) -> String {
        let mut out = String::from(
            "; SageThumbs 2K portable settings.\n\
             ; Delete this file to go back to storing settings in the registry.\n",
        );
        // Root values first, then the subkey sections, so the file reads top-down.
        for section in std::iter::once(ROOT_SECTION).chain(
            doc.keys()
                .map(String::as_str)
                .filter(|s| *s != ROOT_SECTION),
        ) {
            let Some(values) = doc.get(section).filter(|v| !v.is_empty()) else {
                continue;
            };
            out.push_str(&format!("\n[{section}]\n"));
            for (k, v) in values {
                out.push_str(&format!("{k}={v}\n"));
            }
        }
        out
    }

    /// The parsed file. A missing file parses as empty (every getter then sees its default),
    /// which is what makes shipping a zero-byte marker ini a valid "factory defaults" state.
    ///
    /// DELIBERATELY UNCACHED, matching the module-level rule that settings reads aren't
    /// cached so an edit takes effect immediately. A cache keyed on `(mtime, len)` was tried
    /// and is WRONG: flipping `1` to `0`, or `512` to `256`, changes neither, so a same-length
    /// edit landing in the same filesystem clock tick as the previous write is invisible —
    /// `tests/portable_settings.rs` reproduced exactly that. Re-reading costs a warm page-cache
    /// read of a file measured in hundreds of bytes, and the two callers that would otherwise
    /// read per-item ([`super::thumb_settings`], [`super::menu_visibility`]) already take one
    /// snapshot per operation, so there is no hot path this protects.
    fn load() -> Doc {
        let Some(path) = ini_path() else {
            return Doc::new();
        };
        std::fs::read_to_string(path)
            .map(|t| parse(&t))
            .unwrap_or_default()
    }

    /// Apply `edit` to the parsed file and write it back. The cache re-`stat`s, so the
    /// next read picks the new content up without extra bookkeeping here.
    fn update(edit: impl FnOnce(&mut Doc)) -> io::Result<()> {
        let path = ini_path().ok_or_else(|| io::Error::other("not in portable mode"))?;
        let mut doc = load();
        edit(&mut doc);
        std::fs::write(path, render(&doc))
    }

    /// The section a registry subkey maps to. `None` = the root key.
    fn section(sub: Option<&str>) -> &str {
        sub.unwrap_or(ROOT_SECTION)
    }

    pub fn get_string(sub: Option<&str>, name: &str) -> Option<String> {
        load().get(section(sub))?.get(name).cloned()
    }

    pub fn get_u32(sub: Option<&str>, name: &str) -> Option<u32> {
        get_string(sub, name)?.parse().ok()
    }

    pub fn set_string(sub: Option<&str>, name: &str, value: &str) -> io::Result<()> {
        let (sec, name) = (section(sub).to_string(), name.to_string());
        update(|doc| {
            doc.entry(sec).or_default().insert(name, value.to_string());
        })
    }

    pub fn set_u32(sub: Option<&str>, name: &str, value: u32) -> io::Result<()> {
        set_string(sub, name, &value.to_string())
    }

    pub fn remove_value(sub: Option<&str>, name: &str) {
        let (sec, name) = (section(sub).to_string(), name.to_string());
        let _ = update(|doc| {
            if let Some(values) = doc.get_mut(&sec) {
                values.remove(&name);
            }
        });
    }

    /// Every value in one section, for the settings export/import round-trip.
    pub fn section_values(sub: Option<&str>) -> Vec<(String, String)> {
        load()
            .get(section(sub))
            .map(|v| v.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// The names of every non-root section (i.e. what would be registry subkeys).
    pub fn subkey_names() -> Vec<String> {
        load()
            .keys()
            .filter(|s| *s != ROOT_SECTION)
            .cloned()
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_sections_values_and_comments() {
            let doc = parse(
                "; a comment\n\
                 # another\n\
                 StrayRootValue=7\n\
                 \n\
                 [Settings]\n\
                 EnableThumbs = 1\n\
                 Lang=fr\n\
                 [MenuItems]\n\
                 menu_convert_into=0\n\
                 [.psd]\n\
                 Enabled=0\n",
            );
            // A value before any header lands in the root section, as documented.
            assert_eq!(doc[ROOT_SECTION]["StrayRootValue"], "7");
            assert_eq!(doc[ROOT_SECTION]["EnableThumbs"], "1"); // whitespace trimmed
            assert_eq!(doc[ROOT_SECTION]["Lang"], "fr");
            assert_eq!(doc["MenuItems"]["menu_convert_into"], "0");
            assert_eq!(doc[".psd"]["Enabled"], "0");
            // Rendering and re-parsing preserves every value.
            assert_eq!(parse(&render(&doc)), doc);
        }

        #[test]
        fn root_section_renders_first() {
            let mut doc = Doc::new();
            doc.entry(".psd".into())
                .or_default()
                .insert("Enabled".into(), "0".into());
            doc.entry(ROOT_SECTION.into())
                .or_default()
                .insert("Lang".into(), "de".into());
            let text = render(&doc);
            assert!(
                text.find("[Settings]") < text.find("[.psd]"),
                "root values must render before the subkey sections:\n{text}"
            );
        }

        #[test]
        fn empty_and_garbage_parse_to_nothing_rather_than_panicking() {
            assert!(parse("").is_empty());
            assert!(parse("no equals sign here\n[unclosed\n").is_empty());
        }
    }
}

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

/// Contact-sheet thumbnails for GENERIC archives (.zip/.rar/.7z): compose up to 4
/// images into one tile (also what tells a zip of photos apart from a lone photo
/// in a grid view). Off = classic single first-image cover, CBXShell-style.
/// Comics/ebooks (cbz/cb7/cbr/epub/…) always keep their single cover regardless.
pub fn archive_collage() -> bool {
    get_dword("ArchiveCollage", 1) != 0
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
    /// `FormatBadge` — stamp the file's format in the thumbnail's corner. OFF by default:
    /// it alters the picture the user asked to see, so it is opt-in decoration.
    pub format_badge: bool,
}

/// Read the per-`GetThumbnail` settings in one HKCU key open. Missing values fall
/// back to the same defaults the individual getters use, so the result is identical
/// to calling them one by one — just without the repeated opens.
pub fn thumb_settings() -> ThumbSettings {
    // ONE snapshot of whichever backing store is live, then every value is read out of it.
    // Collapsing N opens into one is the entire point of this function, so the portable
    // path takes the same shape: one section snapshot, not one file read per value.
    let ini: Option<std::collections::HashMap<String, String>> =
        store::portable().then(|| store::section_values(None).into_iter().collect());
    let key = match ini {
        Some(_) => None,
        None => CURRENT_USER.open(hkcu_root()).ok(),
    };
    let g = |name: &str, default: u32| {
        if let Some(ini) = ini.as_ref() {
            return ini
                .get(name)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default);
        }
        key.as_ref()
            .and_then(|k| k.get_u32(name).ok())
            .unwrap_or(default)
    };
    let mb = g("MaxSize", DEFAULT_MAX_FILE_MB) as u64;
    ThumbSettings {
        enabled: g("EnableThumbs", 1) != 0,
        max_file_bytes: if mb == 0 { u64::MAX } else { mb * 1024 * 1024 },
        max_thumb: clamp_thumb_size(
            g("Width", DEFAULT_THUMB_SIZE),
            g("Height", DEFAULT_THUMB_SIZE),
        ),
        use_embedded: g("UseEmbedded", 1) != 0,
        format_badge: g("FormatBadge", 0) != 0,
    }
}

/// `FormatBadge` — corner format badge on thumbnails. Default OFF.
pub fn format_badge() -> bool {
    get_dword("FormatBadge", 0) != 0
}

pub fn set_format_badge(on: bool) -> windows_registry::Result<()> {
    set_dword("FormatBadge", u32::from(on))
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
/// Convert dialog: lossy-magick (AVIF / JPEG XL) export quality, 1–100. Drives the
/// `-quality` flag passed to ImageMagick for those targets. Default 50 — a good
/// size/quality balance for AVIF (and reasonable for JXL).
pub fn cv_magick_quality() -> u32 {
    get_dword("CvMagickQuality", 50).clamp(1, 100)
}
/// Persist the Convert dialog's AVIF/JXL quality (clamped 1–100).
pub fn set_cv_magick_quality(q: u32) {
    let _ = set_dword("CvMagickQuality", q.clamp(1, 100));
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

// NOTE: the old `modern_menu_active()` (HKLM `ModernMenuActive`) was REMOVED 2026-07-21.
// It gated whether the classic menu emitted its quick-verb copies, on the false premise
// that Windows bridges the packaged (modern-compact-menu) verbs into the legacy "Show
// more options" menu. It doesn't — packaged verbs live only in the compact flyout — so the
// gate just hid the quick verbs on every classic-menu-default machine (see contextmenu.rs).
// The installer still writes the now-inert `ModernMenuActive` key; nothing reads it.

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

/// Page layout for Combine-into-PDF.
///
/// `PdfLayout`: 0 = tight (default), 1 = margin, 2 = A4 sheet, 3 = Letter sheet.
/// `PdfMarginPt` is the margin in points for modes 1-3 (default 36 = half an inch).
/// Settings exposes only the margin on/off, which is the option PDF24 actually
/// added; the two sheet modes are engine features reachable by setting
/// `PdfLayout` directly, and are documented rather than given a four-way combo
/// nobody asked for.
pub fn pdf_page() -> crate::topdf::PdfPage {
    use crate::topdf::{PdfPage, A4_PT, LETTER_PT};
    let margin = f64::from(get_dword("PdfMarginPt", 36));
    match get_dword("PdfLayout", 0) {
        1 => PdfPage::Margin(margin),
        2 => PdfPage::Sheet {
            w: A4_PT.0,
            h: A4_PT.1,
            margin,
        },
        3 => PdfPage::Sheet {
            w: LETTER_PT.0,
            h: LETTER_PT.1,
            margin,
        },
        _ => PdfPage::Tight,
    }
}

/// Carry EXIF / XMP / IPTC from the source into a converted or resized output.
///
/// **On** by default: our pipeline decodes to pixels and re-encodes, so without
/// this a Convert silently throws away the camera, lens, date and GPS — which is
/// not what someone converting their own photos expects (XnView bundles ExifTool
/// precisely to avoid it). Someone who wants the metadata GONE has an explicit
/// Strip metadata verb; losing it by accident is the worse default.
///
/// Deliberately NOT consulted by Shrink for email, which always drops metadata:
/// that path exists to hand a file to someone else, and mailing your home GPS
/// coordinates is a bigger harm than losing a camera model.
pub fn keep_metadata_on_convert() -> bool {
    get_dword("KeepMetadata", 1) != 0
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

// ---- Custom action hotkey (the user-assignable "action -> hotkey" binding) ----
// A single global hotkey bound to one of a curated set of actions (color picker,
// screenshot, convert…, rotate, files-to-folder, strip metadata, open settings). The
// action is a small opaque id (the app's `hotkey::ACTIONS` table owns the id→behavior
// map); the chord is packed exactly like the screenshot hotkeys (high byte HOTKEYF_*,
// low byte VK; `0` vk = unbound). It rides the SAME opt-in screenshot daemon, which
// registers + dispatches it — so a bound custom hotkey keeps that daemon resident even
// when the screenshot feature itself is off.

/// Default custom action id when none is stored: `1` = the colour picker (the headline ask).
pub const DEFAULT_CUSTOM_ACTION: u32 = 1;

/// The chosen custom-action id (see the app `hotkey::ACTIONS` table). Defaults to the
/// colour picker; the binding is only live once a hotkey is also assigned.
pub fn custom_action() -> u32 {
    get_dword("CustomAction", DEFAULT_CUSTOM_ACTION)
}

/// Persist the chosen custom-action id.
pub fn set_custom_action(id: u32) -> windows_registry::Result<()> {
    set_dword("CustomAction", id)
}

/// The custom action's hotkey as `(hotkeyf_mods, vk)`; `0` vk = unbound (no hotkey
/// registered). The daemon skips registration when vk is 0, so the binding stays off
/// until the user picks a chord in Settings.
pub fn custom_action_hotkey() -> (u32, u32) {
    let v = get_dword("CustomActionHotkey", 0);
    ((v >> 8) & 0xFF, v & 0xFF)
}

/// Persist the custom action hotkey (packed HOTKEYF/VK; `0` = unbound).
pub fn set_custom_action_hotkey(packed: u32) -> windows_registry::Result<()> {
    set_dword("CustomActionHotkey", packed & 0xFFFF)
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
    if store::portable() {
        return store::get_string(None, "ShotSaveDir").unwrap_or_default();
    }
    CURRENT_USER
        .open(hkcu_root())
        .and_then(|k| k.get_string("ShotSaveDir"))
        .unwrap_or_default()
}

/// Persist the chosen save folder (absolute path). Empty restores the Desktop default.
pub fn set_screenshot_save_dir(dir: &str) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, "ShotSaveDir", dir));
    }
    CURRENT_USER
        .create(hkcu_root())?
        .set_string("ShotSaveDir", dir)
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

/// Whether the app periodically checks for a newer release (throttled to once/day) and pops
/// a tray toast when one exists. ON by default. Three things honor it, none of them a
/// resident service: the per-user `SageThumbs2K_UpdateCheck` Scheduled Task (registered at
/// install; runs `--update-check` and exits), the same one-shot spawned opportunistically by
/// any ordinary app launch, and the opt-in screenshot helper's 6 h timer when it happens to
/// be running. Turning this off in Settings also removes the Scheduled Task.
pub fn update_auto_check() -> bool {
    get_dword("UpdateAutoCheck", 1) != 0
}

/// Persist the auto-update-check toggle.
pub fn set_update_auto_check(on: bool) -> windows_registry::Result<()> {
    set_dword("UpdateAutoCheck", on as u32)
}

// ---- Quick preview (QuickLook-style "press Space, see the file") --------
// The opt-in Space-to-preview popup. All EXE-side; the DLL never reads these.
// `PreviewEnabled` is the master switch and ALSO drives the resident daemon's
// residency (the app's `screenshot::enable::daemon_wanted` consults it), so a
// bound Quick preview keeps that shared tray daemon alive exactly like a bound
// custom hotkey does. The rest are viewer behavior prefs read by the viewer
// window. DWORD 0/1; getters default to the plan's §6 defaults.

/// Master switch for Quick preview. OFF by default (nothing hooks the keyboard
/// until the user opts in); also drives daemon residency.
pub fn preview_enabled() -> bool {
    get_dword("PreviewEnabled", 0) != 0
}
/// Persist the Quick preview master toggle.
pub fn set_preview_enabled(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewEnabled", on as u32)
}

/// Hold Space >= 750 ms then release closes the preview ("peek"). ON by default.
pub fn preview_hold_peek() -> bool {
    get_dword("PreviewHoldPeek", 1) != 0
}
/// Persist the hold-to-peek toggle.
pub fn set_preview_hold_peek(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewHoldPeek", on as u32)
}

/// Close the viewer when it loses focus (and isn't pinned). OFF by default.
pub fn preview_close_on_focus_loss() -> bool {
    get_dword("PreviewCloseOnFocusLoss", 0) != 0
}
/// Persist the close-on-focus-loss toggle.
pub fn set_preview_close_on_focus_loss(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewCloseOnFocusLoss", on as u32)
}

/// Bring the viewer to the front (foreground) when it opens — it still shows without
/// stealing focus and can be covered the moment you click another window. This is NOT
/// always-on-top; the toolbar pin button handles that. **ON by default.**
pub fn preview_open_front() -> bool {
    get_dword("PreviewOpenFront", 1) != 0
}
/// Persist the open-in-front toggle.
pub fn set_preview_open_front(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewOpenFront", on as u32)
}

/// Keep the Markdown outline (table-of-contents) sidebar OPEN. ON by default; the viewer's outline
/// toggle button persists the user's choice here (so it stays pinned open/closed across previews).
pub fn preview_toc_open() -> bool {
    get_dword("PreviewTocOpen", 1) != 0
}
/// Persist the Markdown outline-sidebar open/closed state.
pub fn set_preview_toc_open(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewTocOpen", on as u32)
}

// The three below are remembered VIEWER STATE, not configuration: the viewer writes them as you
// use it (exactly like the outline sidebar above), so a level or a size you set on one file is
// still there on the next one and on the next preview. They deliberately have no Settings
// control — "Reset all settings" clears them, and the caption double-click clears the size.

/// Quick preview playback volume, 0..=100 (default 100). The transport strip's slider writes here
/// when you let go of it, so the next clip starts at the level you chose instead of full blast.
pub fn preview_volume() -> u32 {
    get_dword("PreviewVolume", 100).min(100)
}

/// Persist the Quick preview playback volume (clamped to 0..=100).
pub fn set_preview_volume(v: u32) -> windows_registry::Result<()> {
    set_dword("PreviewVolume", v.min(100))
}

/// Whether Quick preview playback starts muted (default false) — the transport's speaker toggle.
pub fn preview_muted() -> bool {
    get_dword("PreviewMuted", 0) != 0
}

/// Persist the Quick preview mute state.
pub fn set_preview_muted(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMuted", on as u32)
}

/// Whether Quick preview media repeats when it reaches the end (default true, which is what the
/// viewer did unconditionally before the transport gained a loop button). Off means the clip stops
/// on its last frame, which is what you want when you are checking whether a render finished.
pub fn preview_loop() -> bool {
    get_dword("PreviewLoop", 1) != 0
}

/// Persist the Quick preview loop state.
pub fn set_preview_loop(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewLoop", on as u32)
}

/// What ←/→ do while a video or track is playing in the Quick preview. Default false = SEEK, which
/// is what those keys mean in every media player. True = move to the previous/next file in the
/// folder, for someone flipping through a folder of clips rather than watching one.
///
/// Either way the transport's own ⏮/⏭ buttons always switch files, and PgUp/PgDn always do too, so
/// neither behaviour is ever unreachable.
pub fn preview_arrow_nav() -> bool {
    get_dword("PreviewArrowNav", 0) != 0
}

/// Persist the ←/→ meaning for video playback.
pub fn set_preview_arrow_nav(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewArrowNav", on as u32)
}

/// Quick preview playback speed in PERCENT (50..=200, default 100). Percent rather than a float
/// because the settings store is DWORD-only, and the transport only ever offers fixed steps.
pub fn preview_speed() -> u32 {
    get_dword("PreviewSpeed", 100).clamp(25, 400)
}

/// Persist the Quick preview playback speed (percent, clamped to the offered range).
pub fn set_preview_speed(pct: u32) -> windows_registry::Result<()> {
    set_dword("PreviewSpeed", pct.clamp(25, 400))
}

/// The viewer size the user last dragged the window out to, as a CLIENT size in **96-dpi logical
/// px** — `None` until they resize one, which is when the viewer goes back to sizing every file to
/// its own content. Logical rather than device px so a size chosen on a 150% display reopens the
/// same apparent size on a 100% one. Two DWORDs; either being absent or 0 means "not remembered".
pub fn preview_window_size() -> Option<(i32, i32)> {
    let w = get_dword("PreviewWinW", 0).min(i32::MAX as u32) as i32;
    let h = get_dword("PreviewWinH", 0).min(i32::MAX as u32) as i32;
    (w > 0 && h > 0).then_some((w, h))
}

/// Persist (or, with `None`, forget) the remembered viewer window size. Forgetting restores the
/// per-content sizing — that is what a double-click on the viewer's caption does.
pub fn set_preview_window_size(size: Option<(i32, i32)>) -> windows_registry::Result<()> {
    let (w, h) = size.unwrap_or((0, 0));
    set_dword("PreviewWinW", w.max(0) as u32)?;
    set_dword("PreviewWinH", h.max(0) as u32)
}

/// Download web-hosted images referenced by a previewed Markdown file (badges, hotlinked art).
/// **OFF by default** — fetching an image URL from a previewed document is an outbound request
/// an attacker-authored README fully controls (classic tracking-pixel shape), so it is strictly
/// opt-in. When off, remote images render as labeled alt-text chips. HTTPS-only when on.
pub fn preview_md_remote_img() -> bool {
    get_dword("PreviewMdRemoteImg", 0) != 0
}
/// Persist the remote-markdown-images toggle.
pub fn set_preview_md_remote_img(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMdRemoteImg", on as u32)
}

/// Render local `.html`/`.htm` files as live web pages (WebView2), instead of showing their
/// source as text. **ON by default** — the viewer locks the page down (scripts off + non-`file://`
/// requests blocked), so a rendered local page can neither run scripts nor reach the network.
pub fn preview_html() -> bool {
    get_dword("PreviewHtml", 1) != 0
}
/// Persist the local-HTML-render toggle.
pub fn set_preview_html(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewHtml", on as u32)
}

/// LIVE-load the target of a `.url`/`.webloc` shortcut in an ephemeral WebView2 (no cookie/session
/// reuse) instead of showing the parsed target URL as text. **OFF by default** — pressing Space on
/// a `.url` would otherwise fire a silent outbound request to an attacker-controllable domain
/// (`.url` is a known phishing vector), so live loading is strictly opt-in.
pub fn preview_url_live() -> bool {
    get_dword("PreviewUrlLive", 0) != 0
}
/// Persist the live-`.url` toggle.
pub fn set_preview_url_live(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewUrlLive", on as u32)
}

/// Preview text/code files (Phase 3 — syntax-highlighted via the viewer's WebView2
/// host). ON by default; only consulted once Phase 3 ships.
pub fn preview_text() -> bool {
    get_dword("PreviewText", 1) != 0
}
/// Persist the text/code preview toggle.
pub fn set_preview_text(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewText", on as u32)
}

/// Render Markdown like GitHub (Phase 3 — via WebView2). ON by default; only
/// consulted once Phase 3 ships.
pub fn preview_markdown() -> bool {
    get_dword("PreviewMarkdown", 1) != 0
}
/// Persist the Markdown preview toggle.
pub fn set_preview_markdown(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewMarkdown", on as u32)
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
    if store::portable() {
        return store::get_u32(Some(ext), "Enabled")
            .map(|v| v != 0)
            .unwrap_or(true);
    }
    CURRENT_USER
        .open(format!(r"{}\{ext}", hkcu_root()))
        .and_then(|k| k.get_u32("Enabled"))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// Persist a per-extension enable flag (used by the Options dialog).
pub fn set_format_enabled(ext: &str, enabled: bool) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_u32(Some(ext), "Enabled", enabled as u32));
    }
    CURRENT_USER
        .create(format!(r"{}\{ext}", hkcu_root()))?
        .set_u32("Enabled", enabled as u32)
}

// ---- Per-menu-item visibility (the "Displayed menu items" checklist) -----

/// Whether a top-level context-menu item (by its MENU title key, e.g.
/// `menu_convert_into`) is shown. All shown by default; the Settings checklist
/// can hide ones the user never uses. Stored under `…\SageThumbs2K\MenuItems\<key>`.
pub fn menu_item_shown(key: &str) -> bool {
    if store::portable() {
        return store::get_u32(Some(MENU_ITEMS), key)
            .map(|v| v != 0)
            .unwrap_or(true);
    }
    CURRENT_USER
        .open(format!(r"{}\MenuItems", hkcu_root()))
        .and_then(|k| k.get_u32(key))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// Persist a top-level menu item's visibility (used by the Options dialog).
pub fn set_menu_item_shown(key: &str, shown: bool) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_u32(Some(MENU_ITEMS), key, shown as u32));
    }
    CURRENT_USER
        .create(format!(r"{}\MenuItems", hkcu_root()))?
        .set_u32(key, shown as u32)
}

/// The user's custom top-level menu order — a list of menu-item title keys, top to
/// bottom — or empty for the default tree order. Stored comma-joined under
/// `…\SageThumbs2K\MenuOrder` (the keys are `menu_*` identifiers, never contain a
/// comma). The classic menu builder applies it via `verbs::ordered_top_level`.
pub fn menu_order() -> Vec<String> {
    let stored = if store::portable() {
        store::get_string(None, "MenuOrder")
    } else {
        CURRENT_USER
            .open(hkcu_root())
            .and_then(|k| k.get_string("MenuOrder"))
            .ok()
    };
    stored
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

/// Persist the custom menu order (comma-joined keys); an empty slice clears it
/// (= back to the default tree order).
pub fn set_menu_order(keys: &[&str]) -> windows_registry::Result<()> {
    if store::portable() {
        return io_result(store::set_string(None, "MenuOrder", &keys.join(",")));
    }
    CURRENT_USER
        .create(hkcu_root())?
        .set_string("MenuOrder", keys.join(","))
}

/// A one-shot snapshot of the menu-item visibility subkey. Building the right-click
/// menu calls [`menu_item_shown`] once per node (~one HKCU open + `format!` alloc
/// each); on a per-right-click hot path inside explorer.exe that adds up. Open
/// `…\MenuItems` ONCE at the top of `QueryContextMenu` / `EnumSubCommands` and ask
/// [`MenuVisibility::shown`] per item instead — same semantics, ~N opens collapse
/// to one. A fresh snapshot per menu build keeps the live-toggle contract (§ module
/// docs) intact — we don't cache across builds.
pub struct MenuVisibility(MenuVisibilitySource);

/// Which backing store the snapshot came from. The portable arm holds the parsed section
/// outright — same "read once per menu build" contract, no file touched per item.
enum MenuVisibilitySource {
    Registry(Option<windows_registry::Key>),
    Portable(std::collections::HashMap<String, String>),
}

/// Open the menu-visibility subkey once for the current menu build. An absent subkey
/// (nothing ever hidden) makes every [`MenuVisibility::shown`] return true.
pub fn menu_visibility() -> MenuVisibility {
    MenuVisibility(if store::portable() {
        MenuVisibilitySource::Portable(
            store::section_values(Some(MENU_ITEMS))
                .into_iter()
                .collect(),
        )
    } else {
        MenuVisibilitySource::Registry(
            CURRENT_USER
                .open(format!(r"{}\MenuItems", hkcu_root()))
                .ok(),
        )
    })
}

impl MenuVisibility {
    /// Whether `key` (a top-level menu item title) is shown — default true unless an
    /// explicit `0` is stored. Identical to [`menu_item_shown`], reusing the snapshot.
    pub fn shown(&self, key: &str) -> bool {
        // Shown by default; hidden only when an explicit `0` is stored. (`matches!`
        // keeps this MSRV-1.80-safe — `is_none_or` would need 1.82.)
        match &self.0 {
            MenuVisibilitySource::Registry(k) => {
                !matches!(k.as_ref().and_then(|k| k.get_u32(key).ok()), Some(0))
            }
            MenuVisibilitySource::Portable(m) => {
                !matches!(m.get(key).map(String::as_str), Some("0"))
            }
        }
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
