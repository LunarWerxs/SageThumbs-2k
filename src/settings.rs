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
    use std::path::{Path, PathBuf};
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

    /// Strip a trailing `; comment` / `# comment` from a value, so a hand-edited file like
    /// `MaxSize=100 ; big files` stores `"100"`, not the literal `"100 ; big files"` (which
    /// then fails `u32::parse` in `get_u32` and silently falls back to the default). Only a
    /// `;`/`#` preceded by whitespace counts, so a value that legitimately contains one (a
    /// path, a URL fragment) passes through untouched.
    fn strip_inline_comment(v: &str) -> &str {
        let bytes = v.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if (b == b';' || b == b'#') && i > 0 && bytes[i - 1].is_ascii_whitespace() {
                return v[..i].trim_end();
            }
        }
        v
    }

    /// Parse an ini. Unknown/blank lines and full-line `;`/`#` comments are skipped; a
    /// value before any `[section]` header is treated as a root value, which makes a
    /// hand-written file that omits the `[Settings]` header still work. A trailing
    /// `; comment` after a value on the same line is stripped too (see
    /// [`strip_inline_comment`]).
    ///
    /// A leading UTF-8 BOM (PowerShell's default `Set-Content`/`Out-File -Encoding UTF8`, and
    /// several editors) is stripped first. Left in place, it lands on the first line and
    /// neither the comment check (`starts_with(';')`) nor the section-header check
    /// (`strip_prefix('[')`) recognizes it, so that whole line — often the first
    /// `[section]` header in a hand-edited file — is silently dropped and everything after
    /// it misfiles into the root section instead (item 24/P24).
    fn parse(text: &str) -> Doc {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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
                doc.entry(section.clone()).or_default().insert(
                    k.trim().to_string(),
                    strip_inline_comment(v.trim()).to_string(),
                );
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

    /// The parsed file. A MISSING file (`NotFound`) parses as empty (every getter then sees
    /// its default), which is what makes shipping a zero-byte marker ini a valid "factory
    /// defaults" state.
    ///
    /// Any OTHER read error — non-UTF-8 bytes from a Notepad "ANSI" save, a sharing violation
    /// from AV/backup, an I/O hiccup — is returned as `Err` rather than collapsed to the same
    /// empty `Doc`. That distinction is the whole point: [`update`] must not treat "I could
    /// not read the real file" as "the file is empty" and then write that emptiness back over
    /// it, which used to make one unreadable read followed by any setting write silently
    /// destroy the user's whole portable configuration (item 9/204/P9).
    ///
    /// DELIBERATELY UNCACHED on success, matching the module-level rule that settings reads
    /// aren't cached so an edit takes effect immediately. A cache keyed on `(mtime, len)` was
    /// tried and is WRONG: flipping `1` to `0`, or `512` to `256`, changes neither, so a
    /// same-length edit landing in the same filesystem clock tick as the previous write is
    /// invisible — `tests/portable_settings.rs` reproduced exactly that. Re-reading costs a
    /// warm page-cache read of a file measured in hundreds of bytes, and the two callers that
    /// would otherwise read per-item ([`super::thumb_settings`], [`super::menu_visibility`])
    /// already take one snapshot per operation, so there is no hot path this protects.
    fn load() -> io::Result<Doc> {
        let Some(path) = ini_path() else {
            return Ok(Doc::new());
        };
        match std::fs::read_to_string(path) {
            Ok(t) => Ok(parse(&t)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Doc::new()),
            Err(e) => Err(e),
        }
    }

    /// Short-lived cross-process lock guarding one `update()` call, so two writers to the
    /// SAME portable ini (the Settings EXE, `st2k`, the screenshot daemon, or two `st2k`
    /// invocations, all in portable mode) cannot race a load-edit-write and silently drop
    /// one edit. A NAMED mutex so every process shares the one kernel object; `Local\`
    /// scopes it to this logon session, matching `decode::magick_gate`'s semaphore.
    struct IniLock(windows::Win32::Foundation::HANDLE);

    impl IniLock {
        /// Best-effort: a lock that could not be created, or two waits that both timed out (a
        /// leaked/wedged mutex must never hang a settings write forever — the same
        /// reasoning as `decode::magick_gate`'s finite wait), returns `None` and the
        /// caller proceeds unlocked rather than blocking a shell/host thread forever.
        ///
        /// Two waits, not one: the first (2000 ms) is the original budget; a SHORT retry
        /// (500 ms) after it catches the common case of a holder that was mid-edit and about
        /// to finish, instead of falling through unlocked on the first miss and silently
        /// risking a lost concurrent write (item 93). Still bounded — a genuinely wedged or
        /// leaked mutex gives up after ~2.5 s total, and the fallthrough is logged so a
        /// degraded run leaves a trace instead of degrading silently.
        fn acquire() -> Option<Self> {
            use windows::core::w;
            use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
            use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
            let h =
                unsafe { CreateMutexW(None, false, w!("Local\\SageThumbs2K.PortableIni")) }.ok()?;
            for timeout_ms in [2_000u32, 500] {
                match unsafe { WaitForSingleObject(h, timeout_ms) } {
                    // WAIT_ABANDONED means a previous holder died mid-edit without releasing;
                    // we still got ownership, and the file itself is never left half-written
                    // because `write_atomic` only replaces it via a completed rename.
                    WAIT_OBJECT_0 | WAIT_ABANDONED => return Some(IniLock(h)),
                    _ => {}
                }
            }
            crate::safety::log_debug(
                "portable ini: IniLock wait timed out twice; proceeding unlocked (a concurrent \
                 write may be lost)",
            );
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
            None
        }
    }

    impl Drop for IniLock {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }

    /// Write `content` to `path` via a sibling `.tmp` file + rename, so a crash mid-write
    /// (this runs under `panic = "abort"`, and several of our own processes can hit it)
    /// leaves either the OLD file intact or the fully-written NEW one, never a truncated
    /// mix of both. `rename_retrying` (not a bare `fs::rename`) absorbs a transient AV /
    /// indexer lock on the destination, same as every other write path in this codebase.
    fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        if let Err(e) = crate::fsutil::rename_retrying(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// Apply `edit` to the parsed file and write it back. Held under [`IniLock`] for the
    /// whole load-edit-write so two writers can't race and silently drop one edit, and
    /// written via [`write_atomic`] so a crash mid-write can't leave a truncated ini
    /// behind.
    ///
    /// **Aborts without writing when [`load`] fails with a real error** (anything but a
    /// missing file) rather than treating the unreadable file as empty — the old
    /// `unwrap_or_default` behaviour rendered that empty `Doc` straight back over the real
    /// file, so one unreadable read followed by any setting write destroyed the user's whole
    /// portable configuration with no error anywhere (item 9/204/P9). Logged via
    /// `log_debug` so the failure leaves a trace even though every public setter here is
    /// best-effort.
    fn update(edit: impl FnOnce(&mut Doc)) -> io::Result<()> {
        let path = ini_path().ok_or_else(|| io::Error::other("not in portable mode"))?;
        let _lock = IniLock::acquire();
        let mut doc = match load() {
            Ok(d) => d,
            Err(e) => {
                crate::safety::log_debug(&format!(
                    "portable ini: aborting write, could not read the existing file at {}: {e}",
                    path.display()
                ));
                return Err(e);
            }
        };
        edit(&mut doc);
        write_atomic(path, &render(&doc))
    }

    /// The section a registry subkey maps to. `None` = the root key.
    fn section(sub: Option<&str>) -> &str {
        sub.unwrap_or(ROOT_SECTION)
    }

    pub fn get_string(sub: Option<&str>, name: &str) -> Option<String> {
        // An unreadable file reads as "value absent" here (same outcome as a missing file),
        // matching the pre-existing read-side contract; only `update`'s WRITE path treats the
        // two differently (see `load`'s and `update`'s doc comments).
        load().ok()?.get(section(sub))?.get(name).cloned()
    }

    pub fn get_u32(sub: Option<&str>, name: &str) -> Option<u32> {
        get_string(sub, name)?.parse().ok()
    }

    /// Whether `value` is safe to store as `name=value` in the ini. It must not contain a
    /// newline — `render` writes one `key=value` line per entry, so an embedded `\r`/`\n`
    /// would inject a literal extra line that `parse` then reads back as a bogus new key, or
    /// (if it starts with `[`) a spoofed `[section]` header, on the very next load. It must
    /// also not itself START WITH `[`, `;` or `#`, the same three lead characters `parse`
    /// treats as syntax rather than a value. Mirrors the `ini_safe()` guard
    /// `settings_io.rs`'s import already applies to its own writes — this generic setter did
    /// not share it (item 112).
    fn value_is_ini_safe(value: &str) -> bool {
        !value.contains(['\r', '\n']) && !value.starts_with(['[', ';', '#'])
    }

    pub fn set_string(sub: Option<&str>, name: &str, value: &str) -> io::Result<()> {
        if !value_is_ini_safe(value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "value contains a newline or starts with an ini syntax character ([, ;, #) \
                 and cannot be safely stored",
            ));
        }
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

    /// Remove a whole subkey section (everything under it), for the settings import
    /// "replace, don't merge" pass — a section the imported document doesn't carry at all is
    /// dropped in one call rather than one `remove_value` per stored name (item 33/221).
    pub fn remove_section(name: &str) {
        let sec = name.to_string();
        let _ = update(|doc| {
            doc.remove(&sec);
        });
    }

    /// Every value in one section, for the settings export/import round-trip. An unreadable
    /// file reads as empty here, same as [`get_string`].
    pub fn section_values(sub: Option<&str>) -> Vec<(String, String)> {
        load()
            .unwrap_or_default()
            .get(section(sub))
            .map(|v| v.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// The names of every non-root section (i.e. what would be registry subkeys). An
    /// unreadable file reads as empty here, same as [`get_string`].
    pub fn subkey_names() -> Vec<String> {
        load()
            .unwrap_or_default()
            .keys()
            .filter(|s| *s != ROOT_SECTION)
            .cloned()
            .collect()
    }

    /// The WHOLE parsed file, section by section — one `load()` (one file read/parse) for a
    /// caller that's about to look up many different sections (e.g. [`super::format_enabled_snapshot`]
    /// sweeping every registered extension). Every other getter above calls `load()` itself per
    /// lookup, which is fine for a handful of reads but reparses the file from scratch on each
    /// one — see [`load`]'s own doc comment for why that isn't cached at THIS layer. An
    /// unreadable file reads as empty here, same as [`get_string`].
    pub fn full_doc() -> BTreeMap<String, BTreeMap<String, String>> {
        load().unwrap_or_default()
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

        /// A005-style value drift: a hand-edited `MaxSize=100 ; big files` used to store the
        /// literal comment text as part of the value, which then silently failed `u32::parse`
        /// in `get_u32` and fell back to the default with no indication anything was wrong.
        #[test]
        fn trailing_inline_comment_is_stripped_from_the_value() {
            let doc = parse("MaxSize=100 ; big files\nLabel=x#not-a-comment\n");
            assert_eq!(doc[ROOT_SECTION]["MaxSize"], "100");
            // No leading whitespace before the `#` -> not a comment, kept literally: the
            // value legitimately contains the character (e.g. a URL fragment).
            assert_eq!(doc[ROOT_SECTION]["Label"], "x#not-a-comment");
        }

        /// PowerShell's default `Set-Content`/`Out-File -Encoding UTF8` (and several editors)
        /// writes a UTF-8 BOM. Without stripping it, the byte sits on the first content line
        /// and neither the comment check nor the `[section]` check recognizes it, so a
        /// hand-edited file's leading `[section]` header used to be silently dropped and
        /// everything after it misfiled into the root section (item 24/P24).
        #[test]
        fn parse_strips_a_leading_utf8_bom() {
            let doc = parse("\u{feff}[MenuItems]\nmenu_convert_into=0\n");
            assert_eq!(doc["MenuItems"]["menu_convert_into"], "0");
            assert!(
                !doc.contains_key(ROOT_SECTION),
                "the BOM'd [MenuItems] header must be recognized, not dropped into the root \
                 section: {doc:?}"
            );

            // A BOM'd value-only file (no header at all) still lands in the root section, same
            // as an un-BOM'd one — the strip must not eat a real leading character of content.
            let doc2 = parse("\u{feff}EnableThumbs=0\n");
            assert_eq!(doc2[ROOT_SECTION]["EnableThumbs"], "0");
        }

        /// A value containing a newline, or starting with `[`/`;`/`#`, would corrupt the ini on
        /// the next parse: an embedded `\n` injects a literal extra line (a bogus key, or a
        /// spoofed `[section]` header), and a leading `[`/`;`/`#` makes the WHOLE value read
        /// back as syntax instead of data. `set_string` must refuse these rather than writing
        /// them verbatim (item 112).
        #[test]
        fn set_string_rejects_values_that_would_corrupt_the_ini_on_reparse() {
            for bad in [
                "a\nEnableThumbs=0",
                "a\r\nb",
                "[Settings]",
                ";a comment",
                "#a comment",
            ] {
                assert!(
                    !value_is_ini_safe(bad),
                    "{bad:?} must be rejected as unsafe to store"
                );
            }
            for good in [
                "ordinary value",
                r"C:\Users\me\Desktop",
                "x#not-a-comment",
                "",
            ] {
                assert!(value_is_ini_safe(good), "{good:?} must be accepted");
            }
        }

        #[test]
        fn write_atomic_writes_full_content_and_leaves_no_tmp_behind() {
            let dir =
                std::env::temp_dir().join(format!("st2k_write_atomic_test_{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            let path = dir.join("probe.ini");

            write_atomic(&path, "[Settings]\nA=1\n").expect("write_atomic");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[Settings]\nA=1\n");
            assert!(
                !path.with_extension("tmp").exists(),
                "write_atomic must not leave its .tmp sibling behind"
            );

            // A second write REPLACES the file rather than appending to or corrupting it.
            write_atomic(&path, "[Settings]\nA=2\n").expect("second write_atomic");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[Settings]\nA=2\n");

            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A004/A271: `update()` used to load-edit-write with no lock at all, so two
        /// concurrent writers could race and one edit would silently vanish. This proves
        /// `IniLock` actually gives mutual exclusion, using a DELIBERATELY unsynchronized
        /// shared counter — correctness here depends entirely on the lock, not on any other
        /// primitive. Without it, many threads racing this non-atomic read-sleep-write lose
        /// updates and the final count comes out under N; that is the same failure MODE
        /// (not the same process boundary) as the cross-process lost-write the finding
        /// described.
        #[test]
        fn ini_lock_serializes_concurrent_holders() {
            struct Racy(std::cell::UnsafeCell<u32>);
            unsafe impl Sync for Racy {}
            static COUNTER: Racy = Racy(std::cell::UnsafeCell::new(0));

            const N: usize = 24;
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    std::thread::spawn(|| {
                        let _lock = IniLock::acquire();
                        let p = COUNTER.0.get();
                        unsafe {
                            let cur = p.read();
                            std::thread::yield_now(); // widen the race window
                            p.write(cur + 1);
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(
                unsafe { *COUNTER.0.get() },
                N as u32,
                "IniLock must serialize its holders, or concurrent writers lose updates"
            );
        }
    }
}

// Defaults + bounds, matching the legacy SageThumbs.h constants.
/// Skip files bigger than this (MB). Legacy 100 -> 256 (2026-08-11) -> 4096 (2026-08-15), and
/// both raises fixed the same class of fault: the user-facing PREFERENCE, not the safety
/// limit, was what refused a file nothing else objected to. At 100, `effective_input_cap`
/// takes `min(MaxSize, 256 MiB)`, so a 150 MB TIFF was refused by the knob — and since we own
/// that extension's thumbnail slot, it then got no thumbnail from anyone.
///
/// **256 was still wrong, more subtly: it EQUALLED the hard ceiling, which silently closed a
/// rescue window.** `streamsrc::stream_source` refuses a file when `size > min(MaxSize, hard
/// cap)` and then offers the oversized WIC rescue when `size <= MaxSize` — the two conditions
/// describing "our buffering ceiling refused it" and "the user did not ask us to skip it".
/// Make MaxSize equal the hard cap and that pair reads `size > 256 MiB && size <= 256 MiB`,
/// which is false for every file that has ever existed. The rescue was unreachable at the
/// default setting, and its own tests missed it by driving the cascade with MaxSize unlimited.
/// Sitting the default clear of the ceiling is what re-opens it; the gate's logic was right.
///
/// This LOOSENS no buffering: 256 MiB remains the real wall for anything we read into memory
/// (`effective_input_cap` still clamps). What it opens is the path that never buffers — WIC
/// reading the file itself and scaling during decode, measured at 2.1 s and no measurable
/// memory for a 340 MP PNG. 4096 keeps a meaningful "don't even try" for the genuinely absurd
/// while covering every real image file; the honest cost ceiling is pixels, not bytes, and
/// that one is `decode::limits::MAX_SCALED_SOURCE_PIXELS`.
///
/// It also matters far less than it looks: the cap gates only the "buffer the whole file"
/// tail of `streamsrc::stream_source`. Video, audio cover art, OpenEXR, archives, RAW and the
/// baked-preview containers (PSD/PSB/.blend/DWG) are all served by targeted or streaming
/// reads that never consult it — a 4 GB movie thumbnails regardless of this number.
pub const DEFAULT_MAX_FILE_MB: u32 = 4096; // FILE_MAX_SIZE
                                           // Raised from the legacy 256/512 (2026-06-22): on Hi-DPI / 4K / large ("jumbo")
                                           // icon views the shell requests thumbnails well past 512px. Capping below the
                                           // requested size handed back an undersized bitmap the shell could neither display
                                           // crisply NOR durably cache — so it re-extracted on every refresh (an expensive
                                           // 4K video-frame decode each time). We honor the request up to 1024 now; small
                                           // views are unaffected (the provider still does `cx.min(max_thumb)`).
pub const DEFAULT_THUMB_SIZE: u32 = 1024; // THUMB_STORE_SIZE (was 256)
pub const THUMB_MIN: u32 = 32; // THUMB_MIN_SIZE
/// Ceiling the user may raise the thumbnail edge to. Raised 1024 -> 2560 (2026-08-14, issue
/// #26.5). The old 1024 was historical, not technical: Windows itself keeps thumbnail cache
/// buckets above it (`thumbcache_1280/1920/2560.db`), and the decoders' own guard is
/// `decode::limits::MAX_DIM` at 16384, so nothing in the pipeline needed 1024 specifically.
/// The request came from cover-art libraries viewed on a 4K/85" screen, where a 1024 px tile
/// cannot resolve the text printed on a movie poster.
///
/// The DEFAULT deliberately stays 1024. Above it, every raised edge costs memory, decode time,
/// cache-file growth and Explorer responsiveness on exactly the large collections that want it
/// — so this is a ceiling a user opts into, not one everybody pays for. `max_thumb_size()` is
/// also only ever an upper bound on what the shell asks for (`cx.min(max_thumb)`), so raising
/// it changes nothing until Explorer genuinely requests a bigger tile.
pub const THUMB_MAX: u32 = 2560; // THUMB_MAX_SIZE (was 512, then 1024)
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

/// The max thumbnail edge to generate, clamped to the [`THUMB_MIN`, `THUMB_MAX`] range.
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
    /// `FormatBadgeStyle` — how that badge is drawn once it IS on. Only read when
    /// `format_badge` is true.
    pub badge_style: crate::badge::BadgeStyle,
    /// `ThumbChecker` — burn a transparency checkerboard into the thumbnail behind
    /// see-through pixels. OFF by default; correct alpha is the better default, this is for
    /// people who want the original SageThumbs' look back.
    pub thumb_checker: bool,
    /// `VideoCoverArt` — for a video that carries embedded poster art, show the poster
    /// instead of a frame from the film. OFF by default: a real frame is the more useful
    /// tile for the videos most people have (phone clips, screen recordings, camera
    /// footage), and a poster there is often a generic stand-in. For a ripped-film library
    /// the reverse is true, which is what this is for. Cover art is used as a FALLBACK
    /// whatever this says, since a file whose codec Windows lacks has no frame to show.
    pub prefer_cover_art: bool,
    /// `VideoOffset` resolved to the [0.0, 0.95] fraction every seek site wants — see
    /// [`video_offset_frac`].
    pub video_offset_frac: f64,
    /// `ArchiveCollage` — see [`archive_collage`]. The raw stored DWORD rather than a bool: the
    /// consuming container code (P06b) reads it as a count/strength knob, not a pure toggle.
    pub archive_collage: u32,
    /// `ContainerPreferCover` — see [`container_prefer_cover`].
    pub container_prefer_cover: bool,
    /// `ContainerSort` — see [`container_sort`].
    pub container_sort: bool,
    /// `ContainerSkipScanlation` — see [`container_skip_scanlation`].
    pub container_skip_scanlation: bool,
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
    // `gopt` is the primitive; `g` is it with a default applied. Both are needed because
    // `CornerMark` has to tell ABSENT from 0 to know whether to fall back to the legacy pair,
    // and doing that with a sentinel default would make 0 (the real "system icon" value)
    // indistinguishable from "never set".
    let gopt = |name: &str| -> Option<u32> {
        if let Some(ini) = ini.as_ref() {
            return ini.get(name).and_then(|v| v.parse().ok());
        }
        key.as_ref().and_then(|k| k.get_u32(name).ok())
    };
    let g = |name: &str, default: u32| gopt(name).unwrap_or(default);
    // Same derivation as `corner_mark()`, off this one snapshot rather than re-opening the key.
    // It has to agree with that function exactly, which is what
    // `thumb_settings_agrees_with_the_individual_accessors` asserts.
    let mark = match gopt("CornerMark") {
        Some(v) => crate::settings::CornerMark::from_dword(v),
        None => crate::settings::CornerMark::from_legacy(
            g("FormatBadge", 0) != 0,
            g("HideTypeOverlay", 0) != 0,
        ),
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
        format_badge: mark == crate::settings::CornerMark::Badge,
        badge_style: crate::badge::BadgeStyle::from_dword(g(
            "FormatBadgeStyle",
            DEFAULT_BADGE_STYLE,
        )),
        thumb_checker: g("ThumbChecker", 0) != 0,
        prefer_cover_art: g("VideoCoverArt", 0) != 0,
        video_offset_frac: f64::from(clamp_video_offset_pct(g(
            "VideoOffset",
            DEFAULT_VIDEO_OFFSET_PCT,
        ))) / 100.0,
        archive_collage: g("ArchiveCollage", 1),
        container_prefer_cover: g("ContainerPreferCover", 1) != 0,
        container_sort: g("ContainerSort", 1) != 0,
        container_skip_scanlation: g("ContainerSkipScanlation", 0) != 0,
    }
}

/// `FormatBadgeStyle` default: the category-coloured icon. The badge itself is opt-in, so
/// anyone who turns it on has asked to be able to tell formats apart at a glance — and a
/// colour does that faster than three letters. `0` selects the older plain text chip.
const DEFAULT_BADGE_STYLE: u32 = 1;

/// What ends up in the BOTTOM-RIGHT CORNER of a thumbnail we produced — the one place where
/// two different things want to draw, and only one of them can win.
///
/// # Why this is one setting and not two checkboxes
///
/// It used to be two independent booleans, `FormatBadge` (draw our mark) and `HideTypeOverlay`
/// (stop Explorer drawing its own file-type icon), and they address the SAME 20 px of tile.
/// Ticking only the first produced the combination nobody wants: Explorer stamps the associated
/// program's icon straight on top of our badge, in that exact corner (see [`crate::badge`] and
/// [`crate::typeoverlay`], whose doc comments each name the other). The user had to find a
/// second, differently-worded checkbox on the same page to get a clean result, and the pairing
/// was never stated anywhere. One three-way choice cannot express the broken combination at all.
///
/// Which mark you get, not whether a decoration is "on": every value here is a real answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CornerMark {
    /// Leave the corner to Windows: Explorer draws the file's own type icon there, exactly as
    /// it does for a file we never touched. The default, and byte-for-byte what an install did
    /// before this setting existed.
    #[default]
    SystemIcon,
    /// Our own format mark — [`crate::badge::BadgeStyle`] (`FormatBadgeStyle`) then picks the
    /// plain text chip or the category-coloured page. Explorer's overlay is suppressed so it
    /// cannot paint over it.
    Badge,
    /// Nothing in the corner at all: no mark of ours, and Explorer's own icon suppressed too.
    /// The bare picture.
    None,
}

impl CornerMark {
    /// `CornerMark`: 0 = system icon, 1 = our badge, 2 = nothing. An unknown value falls to the
    /// default rather than to a blank corner — a value we cannot read is not consent to hide
    /// what Windows would otherwise show.
    pub const fn from_dword(v: u32) -> Self {
        match v {
            1 => Self::Badge,
            2 => Self::None,
            _ => Self::SystemIcon,
        }
    }

    /// `const` so a table can name a variant's stored value directly — see the settings
    /// dialog's `DEPENDENT_ON_COMBO`, where the enum IS the combo's option order.
    pub const fn as_dword(self) -> u32 {
        match self {
            Self::SystemIcon => 0,
            Self::Badge => 1,
            Self::None => 2,
        }
    }

    /// The value an install that predates `CornerMark` should read as, from the two booleans it
    /// does have. Used ONLY when `CornerMark` is absent, so an upgrade keeps whatever the user
    /// had rather than silently reverting to the default.
    ///
    /// `badge` wins over `overlay_hidden` because a user who asked for our mark asked for a
    /// mark; the fact that the old two-checkbox UI let Explorer scribble on it was the bug, not
    /// the request.
    pub fn from_legacy(badge: bool, overlay_hidden: bool) -> Self {
        match (badge, overlay_hidden) {
            (true, _) => Self::Badge,
            (false, true) => Self::None,
            (false, false) => Self::SystemIcon,
        }
    }
}

/// The install wizard's `CornerMark` choice, written by the elevated installer under
/// `HKLM\Software\SageThumbs2K\CornerMark` — the same key path as [`ROOT`], but the machine
/// hive, the same way `bin/app/license.rs`'s `LicenseMode` is written. `None` on a portable
/// build (no installer, no HKLM write) or when the value has never been written.
fn hklm_corner_mark() -> Option<u32> {
    windows_registry::LOCAL_MACHINE
        .open(ROOT)
        .and_then(|k| k.get_u32("CornerMark"))
        .ok()
}

/// `CornerMark` — see [`CornerMark`]. Resolution order: the user's own HKCU choice; failing
/// that, the installer's wizard choice recorded in HKLM (item C3); failing that, the pre-2.5
/// legacy pair, so an upgrading install keeps the corner it already had.
pub fn corner_mark() -> CornerMark {
    match get_dword_opt("CornerMark") {
        Some(v) => CornerMark::from_dword(v),
        None => match hklm_corner_mark() {
            Some(v) => CornerMark::from_dword(v),
            // The legacy pair is READ here and never written again. Leaving the old values in
            // place rather than deleting them costs nothing (this branch stops being reached
            // the moment `CornerMark` exists somewhere) and keeps a downgrade working.
            None => CornerMark::from_legacy(
                get_dword("FormatBadge", 0) != 0,
                get_dword("HideTypeOverlay", 0) != 0,
            ),
        },
    }
}

pub fn set_corner_mark(m: CornerMark) -> windows_registry::Result<()> {
    set_dword("CornerMark", m.as_dword())
}

/// Whether to stamp our own format badge — true for exactly one [`CornerMark`] value.
pub fn format_badge() -> bool {
    corner_mark() == CornerMark::Badge
}

/// `FormatBadgeStyle` — icon (default) or plain text for that badge.
pub fn format_badge_icon() -> bool {
    get_dword("FormatBadgeStyle", DEFAULT_BADGE_STYLE) != 0
}

pub fn set_format_badge_icon(on: bool) -> windows_registry::Result<()> {
    set_dword("FormatBadgeStyle", u32::from(on))
}

/// `ThumbChecker` — burn the transparency checkerboard into Explorer thumbnails. Default OFF
/// (the shell composites real alpha over the folder background, which is normally what you
/// want); see [`crate::checkerpx`] for why this is a separate switch from `PreviewChecker`.
pub fn thumb_checker() -> bool {
    get_dword("ThumbChecker", 0) != 0
}

pub fn set_thumb_checker(on: bool) -> windows_registry::Result<()> {
    set_dword("ThumbChecker", u32::from(on))
}

/// `VideoCoverArt` — prefer a video's embedded poster over a frame from the film itself.
/// Default OFF: see [`ThumbSettings::prefer_cover_art`] for why a frame is the better
/// default and a poster the better option.
pub fn prefer_cover_art() -> bool {
    get_dword("VideoCoverArt", 0) != 0
}

pub fn set_prefer_cover_art(on: bool) -> windows_registry::Result<()> {
    set_dword("VideoCoverArt", u32::from(on))
}

/// `VideoOffset` — how far INTO a video the thumbnail frame is taken from, as a percentage of
/// its running time. 30 % has always been the hard-coded mark; this makes it a setting.
///
/// The default is unchanged, because 30 % is a good answer for the videos most people have.
/// It is a bad answer for a specific and common library: films and TV rips that open on a
/// black distributor card, a fade-in, or a title sequence over black. Those thumbnail as a
/// black rectangle, which is indistinguishable from "SageThumbs failed" (issue #26.4).
pub const DEFAULT_VIDEO_OFFSET_PCT: u32 = 30;
/// Upper bound. Not 100: seeking to the very end lands on credits, a fade-out, or past the
/// last keyframe, so the tile would be black for the opposite reason.
pub const VIDEO_OFFSET_PCT_MAX: u32 = 95;

/// Clamp a stored percentage into the usable range. Pure, so the range is testable without
/// touching HKCU. 0 is allowed and means "the first frame".
pub(crate) fn clamp_video_offset_pct(pct: u32) -> u32 {
    pct.min(VIDEO_OFFSET_PCT_MAX)
}

pub fn video_offset_pct() -> u32 {
    clamp_video_offset_pct(get_dword("VideoOffset", DEFAULT_VIDEO_OFFSET_PCT))
}

pub fn set_video_offset_pct(pct: u32) -> windows_registry::Result<()> {
    set_dword_tracking_default(
        "VideoOffset",
        clamp_video_offset_pct(pct),
        DEFAULT_VIDEO_OFFSET_PCT,
    )
}

/// The same value as the fraction every seek site actually wants.
///
/// One conversion in one place: the seek fraction is threaded through four separate call
/// paths (`video::frame_from_path`, `frame_from_bytes_repr`, `mp4::keyframe_mini_mp4`,
/// `mkv::keyframe_mini_mkv` and `video::frame_from_block_stream`), and they must agree or the
/// same file thumbnails differently in Explorer, the preview pane and the CLI.
pub fn video_offset_frac() -> f64 {
    f64::from(video_offset_pct()) / 100.0
}

/// Whether to suppress Explorer's own file-type icon on the thumbnails of the formats we hook
/// — derived from [`CornerMark`], because it IS half of that one decision.
///
/// True for both non-default values: our badge needs the corner to itself, and "nothing" means
/// nothing. It stays FALSE by default, which matters beyond tidiness: applying it writes into
/// other programs' ProgID keys (see [`crate::typeoverlay`]), and that should never happen
/// without being asked for.
pub fn hide_type_overlay() -> bool {
    corner_mark() != CornerMark::SystemIcon
}

/// `FolderPrebuildVerb` — the folder right-click entry that pre-builds thumbnails. Default ON,
/// unlike [`hide_type_overlay`]: it only creates keys of our own under `HKCU`, changes nothing
/// that already exists, and adds no code to Explorer's process (see [`crate::foldermenu`]).
/// The product already puts a right-click menu on files, so a folder entry is in character.
pub fn folder_prebuild_verb() -> bool {
    get_dword("FolderPrebuildVerb", 1) != 0
}

pub fn set_folder_prebuild_verb(on: bool) -> windows_registry::Result<()> {
    set_dword("FolderPrebuildVerb", u32::from(on))
}

// ---- Convert-verb quality settings --------------------------------------

/// Clamp a stored JPEG quality DWORD into the 1..=100 byte range. Pure so it
/// can be tested without HKCU. `0` is refused rather than passed through — a saved quality of
/// 0 is not "as small as possible", it silently produces a degenerate/near-blank JPEG (item
/// 104), so the floor matches the lower bound every other quality knob in this module already
/// uses (`cv_jpeg_quality`, `cv_webp_quality`, `cv_magick_quality` are all `.clamp(1, 100)`).
pub(crate) fn clamp_quality(q: u32) -> u8 {
    q.clamp(1, 100) as u8
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

/// Which annotation tool the capture editor opens with, as an INDEX into
/// `screenshot::tools::Tool::DEFAULTABLE`. That array owns the ordering and the fallback;
/// this is only the stored number, so the two cannot disagree about what index 0 means.
pub const DEFAULT_SHOT_TOOL: u32 = 0; // Arrow

/// How many tools the Settings dropdown offers, i.e. the length of
/// `screenshot::tools::Tool::DEFAULTABLE`. It lives HERE because the Settings dialog cannot
/// see that array (it is `pub(super)` inside the screenshot module) and still has to clamp
/// `CB_SETCURSEL`: an out-of-range index selects nothing and the combo renders BLANK, which
/// is what a hand-edited registry value used to produce. `tools.rs` holds a compile-time
/// assertion that the two agree, so adding a tool without updating this fails the build
/// rather than silently truncating the list.
pub const SHOT_TOOL_COUNT: u32 = 10;

/// The starting tool for the capture editor (index into `Tool::DEFAULTABLE`).
///
/// Defaults to ARROW rather than the rectangle it used to hardcode: pointing at the thing
/// you just captured is the common case, and a rectangle is the one people undo. Unclamped
/// on purpose — `Tool::from_default_index` owns the range check, so a hand-edited registry
/// value degrades in exactly one place.
pub fn screenshot_default_tool() -> u32 {
    get_dword("ShotDefaultTool", DEFAULT_SHOT_TOOL)
}

/// Persist the capture editor's starting tool. See [`screenshot_default_tool`].
pub fn set_screenshot_default_tool(index: u32) -> windows_registry::Result<()> {
    set_dword("ShotDefaultTool", index)
}

/// Surface the most-used verbs (Convert into / Resize / Rotate) directly on the
/// MAIN right-click menu (above the SageThumbs submenu), so they're one click
/// instead of two. OFF by default — the original SageThumbs kept everything inside
/// its submenu, so we don't crowd the main menu unless the user opts in.
pub fn menu_quick_verbs() -> bool {
    get_dword("MenuQuickVerbs", 0) != 0
}

/// A snapshot of the three menu-gate settings ([`menu_enabled`], [`menu_all_file_types`],
/// [`menu_quick_verbs`]), read with a SINGLE HKCU key open instead of one open per getter —
/// the same collapsing [`ThumbSettings`]/[`thumb_settings`] already does for the per-thumbnail
/// settings. `explorer.exe`'s modern-menu `GetState`/`EnumSubCommands` calls all three
/// separately today, once per top-level menu item per right-click (item 132).
#[derive(Clone, Copy, Debug)]
pub struct MenuGate {
    /// `EnableMenu` — master on/off for the right-click menu.
    pub enabled: bool,
    /// `MenuAllFileTypes` — show a condensed menu on unsupported selections too.
    pub all_file_types: bool,
    /// `MenuQuickVerbs` — surface the top verbs directly on the main right-click menu.
    pub quick_verbs: bool,
}

/// Read the menu-gate settings in one HKCU key open. Missing values fall back to the same
/// defaults the individual getters use, so the result is identical to calling
/// [`menu_enabled`]/[`menu_all_file_types`]/[`menu_quick_verbs`] separately — just without the
/// repeated opens.
pub fn menu_gate() -> MenuGate {
    let ini: Option<std::collections::HashMap<String, String>> =
        store::portable().then(|| store::section_values(None).into_iter().collect());
    let key = match ini {
        Some(_) => None,
        None => CURRENT_USER.open(hkcu_root()).ok(),
    };
    let gopt = |name: &str| -> Option<u32> {
        if let Some(ini) = ini.as_ref() {
            return ini.get(name).and_then(|v| v.parse().ok());
        }
        key.as_ref().and_then(|k| k.get_u32(name).ok())
    };
    let g = |name: &str, default: u32| gopt(name).unwrap_or(default);
    MenuGate {
        enabled: g("EnableMenu", 1) != 0,
        all_file_types: g("MenuAllFileTypes", 0) != 0,
        quick_verbs: g("MenuQuickVerbs", 0) != 0,
    }
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

/// The eyedropper's clipboard format: 0 hex `#RRGGBB` (the default and the historical
/// behaviour), 1 `rgb(r, g, b)`, 2 `hsl(...)`, 3 `hsv(...)`. Cycled with Tab inside the
/// overlay and remembered here, so a designer who lives in HSL sets it once. No Settings
/// row on purpose — the choice belongs in the tool, at the moment you can see the value.
pub fn eyedropper_format() -> u32 {
    get_dword("EyeFormat", 0).min(3)
}
/// Persist the eyedropper's clipboard format.
pub fn set_eyedropper_format(f: u32) -> windows_registry::Result<()> {
    set_dword("EyeFormat", f.min(3))
}

/// The eyedropper's pick history: up to 10 colours as comma-joined `RRGGBB` hex, most
/// recent first. Shown as a swatch row in the loupe on the NEXT session and recallable
/// with the 1–9 keys — re-grabbing yesterday's brand colour without hunting for a pixel
/// that still shows it.
pub fn eyedropper_history() -> Vec<(u8, u8, u8)> {
    let Some(s) = get_string_opt("EyeHistory") else {
        return Vec::new();
    };
    s.split(',')
        .filter_map(|t| {
            let t = t.trim();
            if t.len() != 6 {
                return None;
            }
            let v = u32::from_str_radix(t, 16).ok()?;
            Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
        })
        .take(10)
        .collect()
}
/// Persist the eyedropper pick history (most recent first; the cap is applied here so no
/// caller can grow the value without bound).
pub fn set_eyedropper_history(h: &[(u8, u8, u8)]) -> windows_registry::Result<()> {
    let joined: Vec<String> = h
        .iter()
        .take(10)
        .map(|&(r, g, b)| format!("{r:02X}{g:02X}{b:02X}"))
        .collect();
    set_string("EyeHistory", &joined.join(","))
}

/// Seconds to wait before a capture freezes the screen (0 = immediately, the default and
/// the historical behaviour). The wait is what lets a hover-only menu, a tooltip, or a
/// dropdown be summoned INTO the capture — the moment the overlay appears, focus moves and
/// those dismiss themselves, so without a delay they are uncapturable.
pub fn screenshot_delay_sec() -> u32 {
    get_dword("ShotDelaySec", 0).min(10)
}
/// Persist the capture delay. See [`screenshot_delay_sec`].
pub fn set_screenshot_delay_sec(s: u32) -> windows_registry::Result<()> {
    set_dword("ShotDelaySec", s.min(10))
}
/// The Settings combo's wire format: option index -> stored seconds. The array IS the
/// mapping (same discipline as `Tool::DEFAULTABLE`), so the dropdown and the stored value
/// cannot drift apart.
pub const SHOT_DELAY_STEPS: [u32; 5] = [0, 1, 2, 3, 5];

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

/// Show the page-thumbnail strip beside a multi-page PDF in the Quick preview. Default ON.
///
/// A switch rather than always-on because the strip costs a sixth of the window, and someone
/// reading a two-page letter wants the page, not a contact sheet of it. It hides itself on a
/// narrow window regardless (see `pdfview::strip_width`).
pub fn preview_pdf_strip() -> bool {
    get_dword("PreviewPdfStrip", 1) != 0
}

/// Which skin SageThumbs' OWN windows use, independent of the Windows app-colour setting.
///
/// `0` follow Windows (the default and the behaviour every version before this had), `1` light,
/// `2` dark. Requested by a user who wanted the Quick preview dark while the rest of Windows
/// stayed light, which previously meant flipping the whole OS.
///
/// This governs the app's windows only: Quick preview, Settings, Convert, the screenshot
/// editor. The Explorer context menu and the preview PANE live inside Explorer's process and
/// keep following Windows, because they are drawn into someone else's UI and disagreeing with
/// the surrounding shell would look broken rather than themed.
pub fn app_theme() -> u32 {
    get_dword("AppTheme", 0).min(2)
}

/// Persist the app skin. Out-of-range values are refused rather than clamped: the only writer
/// is a three-item combo, so anything else means a caller bug or a hand-edited registry, and
/// silently storing 7 as "dark" would hide it.
pub fn set_app_theme(mode: u32) -> windows_registry::Result<()> {
    if mode > 2 {
        return Ok(());
    }
    set_dword_tracking_default("AppTheme", mode, 0)
}

/// Persist the ←/→ meaning for video playback.
pub fn set_preview_arrow_nav(on: bool) -> windows_registry::Result<()> {
    set_dword("PreviewArrowNav", on as u32)
}

/// Quick preview playback speed in PERCENT (25..=400, default 100). Percent rather than a float
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

/// A one-shot snapshot of every per-extension `Enabled` flag, for a caller that's about to
/// call the equivalent of [`format_enabled`] once per format in a sweep over `FORMATS`
/// (`register.rs`'s HKCR (re)registration, `typeoverlay.rs`, `doctor.rs`'s per-format audit —
/// each on the order of ~330 lookups). In portable mode, [`format_enabled`] goes through
/// `store::get_u32`, which — per `store::load`'s own doc comment — re-reads and re-parses the
/// WHOLE ini file from disk on every single call; unlike [`menu_visibility`]'s "read the tree
/// once per menu build" snapshot, nothing collapsed that for the per-extension flags. This
/// does: one [`store::full_doc`] parse up front, then every [`FormatEnabledSnapshot::enabled`]
/// lookup is an in-memory map hit. The registry arm doesn't get the same win (each extension is
/// its own HKCU subkey, so there's no single tree to snapshot) but stays correct by falling
/// back to [`format_enabled`] per lookup — the whole benefit here is portable-only.
pub struct FormatEnabledSnapshot(FormatEnabledSource);

enum FormatEnabledSource {
    Registry,
    Portable(std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>),
}

/// Take the snapshot. Call this once before a sweep and reuse it for every extension, instead
/// of calling [`format_enabled`] per extension.
pub fn format_enabled_snapshot() -> FormatEnabledSnapshot {
    FormatEnabledSnapshot(if store::portable() {
        FormatEnabledSource::Portable(store::full_doc())
    } else {
        FormatEnabledSource::Registry
    })
}

impl FormatEnabledSnapshot {
    /// Same semantics as [`format_enabled`] (default true unless an explicit `0` is stored),
    /// reusing the one-shot parse in portable mode.
    pub fn enabled(&self, ext: &str) -> bool {
        match &self.0 {
            FormatEnabledSource::Registry => format_enabled(ext),
            FormatEnabledSource::Portable(doc) => doc
                .get(ext)
                .and_then(|v| v.get("Enabled"))
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| v != 0)
                .unwrap_or(true),
        }
    }
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
                // Numeric, like the registry arm above and like `menu_item_shown` (this
                // function's own doc claims "identical" to it) — a literal string match
                // against "0" disagreed on a non-canonical stored value like "00", which
                // `menu_item_shown`'s `get_u32` parses as 0 (hidden) but this used to keep
                // as "shown" since "00" != "0".
                !matches!(m.get(key).and_then(|v| v.parse::<u32>().ok()), Some(0))
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

    /// A188: [`FormatEnabledSnapshot`]'s portable arm must agree with [`format_enabled`] for
    /// every case that matters — an explicit `0` (disabled), any other stored value (enabled),
    /// and an extension nobody configured at all (enabled by default) — since it exists purely
    /// to replace repeated `format_enabled` calls with one parse, not to change the answer.
    #[test]
    fn format_enabled_snapshot_portable_arm_matches_format_enabled_semantics() {
        let mut doc = std::collections::BTreeMap::new();
        let mut psd = std::collections::BTreeMap::new();
        psd.insert("Enabled".to_string(), "0".to_string());
        doc.insert(".psd".to_string(), psd);
        let mut heic = std::collections::BTreeMap::new();
        heic.insert("Enabled".to_string(), "1".to_string());
        doc.insert(".heic".to_string(), heic);

        let snap = FormatEnabledSnapshot(FormatEnabledSource::Portable(doc));
        assert!(!snap.enabled(".psd"), "explicit 0 must read as disabled");
        assert!(snap.enabled(".heic"), "explicit 1 must read as enabled");
        assert!(
            snap.enabled(".never_configured"),
            "an extension with no stored value defaults enabled, matching format_enabled"
        );
    }

    /// A187: the portable arm of `MenuVisibility::shown` used to literal-string-match
    /// `"0"`, disagreeing with `menu_item_shown`'s numeric `get_u32` parse (which reads
    /// "00" as 0) despite `shown`'s own doc comment calling the two "identical".
    #[test]
    fn menu_visibility_portable_arm_parses_stored_value_numerically() {
        let mut m = std::collections::HashMap::new();
        m.insert("menu_convert_into".to_string(), "00".to_string());
        let mv = MenuVisibility(MenuVisibilitySource::Portable(m));
        assert!(
            !mv.shown("menu_convert_into"),
            "a non-canonical \"00\" must be treated as 0 (hidden), matching menu_item_shown"
        );
        // Absent / non-numeric stored values stay shown (the documented default).
        assert!(mv.shown("menu_never_configured"));
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
