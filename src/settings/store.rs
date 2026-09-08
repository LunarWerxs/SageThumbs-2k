//! The storage backend every getter/setter in this module goes through.
//!
//! Normally that's `HKCU\Software\SageThumbs2K` (see [`hkcu_root`]) and nothing here
//! changes. **Portable mode** is the exception: when a file named [`INI_NAME`] sits next
//! to the running module (the EXE for `st2k`/`SageThumbs2K`, the DLL in the shell host),
//! every read and write goes to that file instead and we touch the registry not at all.
//!
//! The marker IS the config file, so a portable build ships one (empty is fine) and an
//! installed build simply never has one — meaning the installed product's behaviour is
//! bit-identical to before this module existed. There is deliberately no setting, flag or
//! env var that turns portable mode on: the file's presence next to the binary is the
//! whole switch, which is what makes "extract the zip somewhere else" work with no state.
//!
//! Layout mirrors the registry tree one-for-one — root values live in `[Settings]`, each
//! registry subkey becomes its own section:
//!
//! ```ini
//! [Settings]
//! EnableThumbs=1
//! Lang=fr
//!
//! [MenuItems]
//! menu_convert_into=0
//!
//! [.psd]
//! Enabled=0
//! ```
//!
//! Everything is text on disk; [`get_u32`] parses and
//! [`set_u32`] writes decimal, so a DWORD round-trips exactly. Reads go
//! straight to the file every time, keeping the module-level promise that an edit takes
//! effect immediately without restarting anything (see [`load`] for why the
//! obvious cache is not merely unnecessary here but incorrect).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The file whose presence next to the running module means "portable".
pub const INI_NAME: &str = "SageThumbs2K.ini";
/// The section holding what would otherwise be the root key's values.
pub const ROOT_SECTION: &str = "Settings";

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
        let h = unsafe { CreateMutexW(None, false, w!("Local\\SageThumbs2K.PortableIni")) }.ok()?;
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

/// Write `content` to `path` via a uniquely-named staging file + rename, so a crash
/// mid-write (this runs under `panic = "abort"`, and several of our own processes can
/// hit it) leaves either the OLD file intact or the fully-written NEW one, never a
/// truncated mix of both. Delegates to [`crate::fsutil::write_atomically`] - the same
/// helper the app EXE's settings-export path (`settings_io::export_settings_to_path`,
/// 2026-09-05 audit, F13) uses, so this codebase has one atomic-write implementation
/// rather than two that could quietly drift apart.
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    crate::fsutil::write_atomically(path, content.as_bytes())
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
pub(super) fn update(edit: impl FnOnce(&mut Doc)) -> io::Result<()> {
    let path = ini_path().ok_or_else(|| io::Error::other("not in portable mode"))?;
    let _lock = IniLock::acquire();
    let mut doc = match load() {
        Ok(d) => d,
        Err(e) => {
            crate::safety::log_debugf!(
                "portable ini: aborting write, could not read the existing file at {}: {e}",
                path.display()
            );
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

    /// No entry in `dir` may end in `.tmp` - `write_atomic`'s staging files (named
    /// `.{name}.{pid}.{n}.tmp` by `fsutil::staging_path`, not the destination's own name
    /// with its extension swapped for `tmp`) must never survive a write. A prior version
    /// of this assertion checked `path.with_extension("tmp")`, a filename the current
    /// staging scheme never produces, so that half of the test was vacuous - it would
    /// have passed even if staging files were leaking, as long as none happened to be
    /// named exactly `probe.tmp`.
    fn assert_no_leftover_tmp_files(dir: &std::path::Path) {
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .expect("read scratch dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn write_atomic_writes_full_content_and_leaves_no_tmp_behind() {
        let dir =
            std::env::temp_dir().join(format!("st2k_write_atomic_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("probe.ini");

        write_atomic(&path, "[Settings]\nA=1\n").expect("write_atomic");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[Settings]\nA=1\n");
        assert_no_leftover_tmp_files(&dir);

        // A second write REPLACES the file rather than appending to or corrupting it.
        write_atomic(&path, "[Settings]\nA=2\n").expect("second write_atomic");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[Settings]\nA=2\n");
        assert_no_leftover_tmp_files(&dir);

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
