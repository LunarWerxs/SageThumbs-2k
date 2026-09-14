//! COM registration and `LoadLibrary` probes for `st2k doctor`: is each coclass
//! registered, does its DLL exist and actually load, is it in the Approved list, and
//! does the per-format `shellex`/ProgID walk point at us rather than another program.

use super::*;
use crate::guids::{
    CLSID_CONTEXT_MENU_STR, CLSID_PREVIEW_HANDLER_STR, CLSID_PROPERTY_STORE_STR,
    CLSID_THUMBNAIL_PROVIDER_STR, THUMB_HANDLER_CATEGORY,
};

/// C9: was a hand-typed local copy of the exact same GUID `register.rs` also hand-typed
/// (`{E357FCCD-A995-4576-B01F-234630154E96}`, the shell's `IThumbnailProvider` category) —
/// now the one shared constant both files read.
const THUMB_HANDLER: &str = THUMB_HANDLER_CATEGORY;
const APPROVED: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";
/// {BB2E617C-0920-11D1-9A0B-00C04FC2D6C1} — the legacy `IExtractImage` shellex slot some
/// side-by-side thumbnailers (MysticThumbs, XnShellEx, the original SageThumbs) bind under
/// instead of, or alongside, the modern `IThumbnailProvider` one `THUMB_HANDLER` names.
const EXTRACT_IMAGE_HANDLER: &str = "{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}";

/// Read a registry default (`""`) value as a string, from any of the three roots we use.
fn hkcr_default(path: &str) -> Option<String> {
    CLASSES_ROOT
        .open(path)
        .ok()
        .and_then(|k| k.get_string("").ok())
}

/// The DLL path Windows would actually load for a CLSID, straight from the registry —
/// NOT the path we think we installed to. A stale entry pointing at a deleted build is
/// exactly the kind of thing that produces silent nothing.
fn inproc_path(clsid: &str) -> Option<String> {
    hkcr_default(&format!("CLSID\\{clsid}\\InprocServer32"))
}

/// Try to genuinely load the DLL. This is the check that catches a missing runtime
/// dependency: the registry can be perfect and the loader still refuses, in which case
/// the shell silently falls back to a plain icon with nothing logged anywhere.
fn can_load(path: &Path) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    unsafe {
        match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(h) => {
                let _ = FreeLibrary(h);
                Ok(())
            }
            Err(e) => Err(format!("{} (0x{:08X})", e.message(), e.code().0)),
        }
    }
}
use std::os::windows::ffi::OsStrExt as _;

/// Where the shell extension is installed, per the registry. Falls back to the folder
/// this executable sits in (a portable/dev layout).
pub(super) fn installed_dll() -> Option<PathBuf> {
    if let Some(p) = inproc_path(CLSID_THUMBNAIL_PROVIDER_STR) {
        return Some(PathBuf::from(p));
    }
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.join("sagethumbs2k.dll"))
}

/// The decade-old original SageThumbs, if it is still on disk.
///
/// Inert by itself — nothing in the registry points at it once we are installed — so this is
/// NOT reported as a failure. It is reported because of a specific footgun: running that
/// install's `unins000.exe` unregisters the shell-extension entries by CLSID and file
/// association, and the overlap with ours is enough to leave a working SageThumbs 2K with no
/// thumbnails and no obvious reason why. Someone tidying up their Program Files would have no
/// way to know that, which is exactly when they would run it.
fn check_legacy_install(r: &mut Report) {
    let legacy: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|base| Path::new(&base).join("SageThumbs"))
        .filter(|p| p.is_dir())
        .collect();
    for dir in legacy {
        let uninstaller = dir.join("unins000.exe");
        if uninstaller.is_file() {
            r.line(
                S::Warn,
                "Old SageThumbs install",
                &format!(
                    "{} — harmless where it sits, but do NOT run its unins000.exe",
                    dir.display()
                ),
            );
            r.line(
                S::Info,
                "  why",
                "that uninstaller strips shell-extension registrations by CLSID and file type, \
                 and would take SageThumbs 2K's with it. Delete the FOLDER instead if you want \
                 it gone.",
            );
        } else {
            r.line(
                S::Info,
                "Old SageThumbs install",
                &format!("{} — leftover files, no uninstaller, inert", dir.display()),
            );
        }
    }
}

/// Check one COM handler's registration/load status, writing to the report as it goes.
/// Returns `false` only when this handler is BOTH critical and broken (unregistered, missing
/// DLL, or fails to load); a non-critical handler always returns `true` regardless of state.
fn check_one_handler(r: &mut Report, name: &str, clsid: &str, critical: bool) -> bool {
    match inproc_path(clsid) {
        None => {
            if critical {
                r.fail_with_fix(
                    name,
                    "NOT REGISTERED (no InprocServer32)",
                    "Reinstall, or run an elevated: \
                     regsvr32 \"C:\\Program Files\\SageThumbs2K\\sagethumbs2k.dll\"",
                );
                false
            } else {
                r.line(S::Warn, name, "not registered");
                true
            }
        }
        Some(p) => {
            let path = PathBuf::from(&p);
            if !path.exists() {
                r.fail_with_fix(
                    name,
                    &format!("registered -> {p} (FILE MISSING)"),
                    "The registration points at a DLL that is not there — reinstall.",
                );
                !critical
            } else if let Err(e) = can_load(&path) {
                r.fail_with_fix(
                    name,
                    &format!("DLL WILL NOT LOAD: {e}"),
                    "Windows cannot load the extension, so the shell silently shows \
                     plain icons. Usually a missing Microsoft Visual C++ Redistributable \
                     (x64) — install it and retry.",
                );
                !critical
            } else {
                r.line(S::Ok, name, &format!("registered, loads OK -> {p}"));
                true
            }
        }
    }
}

/// The Approved list is mandatory on locked-down / policy-managed machines and is silently
/// enforced: an unapproved extension is simply never loaded.
fn check_approved_list(r: &mut Report) {
    let approved = LOCAL_MACHINE.open(APPROVED).ok();
    for (name, clsid) in [
        ("Approved: thumbnail", CLSID_THUMBNAIL_PROVIDER_STR),
        ("Approved: context menu", CLSID_CONTEXT_MENU_STR),
        ("Approved: preview handler", CLSID_PREVIEW_HANDLER_STR),
        ("Approved: property handler", CLSID_PROPERTY_STORE_STR),
    ] {
        let listed = approved
            .as_ref()
            .and_then(|k| k.get_string(clsid).ok())
            .is_some();
        if listed {
            r.line(S::Ok, name, "listed");
        } else {
            r.line(S::Warn, name, "not in the Approved Shell Extensions list");
        }
    }
}

/// The COM half: is each coclass registered, does its DLL exist, and will it load.
pub(super) fn check_registration(r: &mut Report) -> bool {
    r.head("COM registration");
    check_legacy_install(r);

    let mut thumb_ok = true;
    let handlers = [
        ("Thumbnail provider", CLSID_THUMBNAIL_PROVIDER_STR, true),
        ("Context menu (classic)", CLSID_CONTEXT_MENU_STR, false),
        ("Preview handler", CLSID_PREVIEW_HANDLER_STR, false),
        ("Property handler", CLSID_PROPERTY_STORE_STR, false),
    ];

    for (name, clsid, critical) in handlers {
        if !check_one_handler(r, name, clsid, critical) {
            thumb_ok = false;
        }
    }

    check_approved_list(r);

    thumb_ok
}

/// The formats whose thumbnail slot WE took from another program — the mirror image of the
/// "owned by another program" line below, and the case a confused user actually hits: their
/// thumbnails changed right after installing us because we replaced someone else's handler,
/// not because anything is broken. Until this existed the report could only see theft in one
/// direction, so the single most common "it worked before SageThumbs" complaint was invisible
/// to the very tool we hand people. Every entry is restored automatically on uninstall.
pub(super) fn check_displaced(r: &mut Report) {
    let displaced = crate::register::displaced_handlers();
    if displaced.is_empty() {
        return;
    }

    // Two key paths are recorded per extension (the bare `.ext` and its
    // `SystemFileAssociations` twin) — collapse them so each format is listed once.
    let mut by_ext: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (path, clsid) in &displaced {
        if let Some(ext) = crate::register::displaced_key_ext(path) {
            by_ext.insert(ext.to_string(), clsid.clone());
        }
    }
    if by_ext.is_empty() {
        return;
    }

    r.head("Handlers SageThumbs 2K replaced");
    r.line(
        S::Info,
        "Formats taken from another program",
        &format!("{} — each is put back if you uninstall", by_ext.len()),
    );
    const SHOWN: usize = 20;
    for (ext, clsid) in by_ext.iter().take(SHOWN) {
        // Resolve the CLSID to its friendly name so the report reads "Icaros Thumbnail
        // Provider" rather than a bare GUID nobody can identify.
        let name = hkcr_default(&format!("CLSID\\{clsid}")).unwrap_or_else(|| clsid.clone());
        r.line(S::Info, ext, &name);
    }
    if by_ext.len() > SHOWN {
        r.line(S::Info, "  ...", &format!("{} more", by_ext.len() - SHOWN));
    }
}

/// Resolve the effective thumbnail handler for one extension: the SystemFileAssociations
/// key first (Windows consults it before the bare-extension key — see register.rs's module
/// doc), falling back to the bare key. `lookup` reads one key's default value; taken as a
/// parameter (rather than calling `hkcr_default` directly) so the precedence itself is
/// testable without touching the real registry.
fn effective_thumb_handler(
    sfa_key: &str,
    bare_key: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    lookup(sfa_key).or_else(|| lookup(bare_key))
}

/// The per-extension half: for each format we claim, does `.ext\shellex` actually point
/// at us? Reports hijacks separately from plain absences — "another program took it" is
/// a completely different fix from "registration never ran".
///
/// `snap`: G134 — this loop is exactly the ~330-lookup sweep `FormatEnabledSnapshot`'s own
/// doc comment names (`register.rs`, `typeoverlay.rs`, and this file's per-format audit); in
/// portable mode `format_enabled` re-reads and re-parses the WHOLE ini file from disk on
/// EVERY call, so a per-extension `crate::settings::format_enabled(ext)` call here meant one
/// full ini parse per format. Take the snapshot once in [`report`] and reuse it.
pub(super) fn check_extensions(r: &mut Report, snap: &crate::settings::FormatEnabledSnapshot) {
    r.head("Per-format file associations");

    let (mut ours, mut missing, mut stolen, mut disabled) = (0usize, 0usize, 0usize, 0usize);
    let mut stolen_examples: Vec<String> = Vec::new();
    let mut missing_examples: Vec<String> = Vec::new();

    for &(ext, _) in FORMATS.iter() {
        if !snap.enabled(ext) {
            disabled += 1;
            continue;
        }
        // register.rs writes BOTH the SystemFileAssociations twin and the bare-extension
        // key, and Windows consults the former first (see register.rs's module doc). Check
        // it first here too, falling back to the bare key: reading only the bare key would
        // report a wrong verdict on a machine where just one of the pair was overwritten.
        let sfa_key = format!(r"SystemFileAssociations\.{ext}\shellex\{THUMB_HANDLER}");
        let bare_key = format!(".{ext}\\shellex\\{THUMB_HANDLER}");
        let effective = effective_thumb_handler(&sfa_key, &bare_key, hkcr_default);
        match effective.as_deref() {
            Some(c) if c.eq_ignore_ascii_case(CLSID_THUMBNAIL_PROVIDER_STR) => ours += 1,
            Some(other) => {
                stolen += 1;
                if stolen_examples.len() < 6 {
                    stolen_examples.push(format!(".{ext} -> {other}"));
                }
            }
            None => {
                missing += 1;
                if missing_examples.len() < 6 {
                    missing_examples.push(format!(".{ext}"));
                }
            }
        }
    }

    let enabled = ours + missing + stolen;
    r.line(
        S::Info,
        "Formats enabled in settings",
        &format!("{enabled} (of {})", FORMATS.len()),
    );
    if disabled > 0 {
        r.line(S::Info, "Formats turned off by you", &format!("{disabled}"));
    }

    if enabled == 0 {
        r.fail_with_fix(
            "Enabled formats",
            "0 — every format is switched off",
            "Settings -> File types -> enable the formats you want.",
        );
        return;
    }

    if ours == enabled {
        r.line(
            S::Ok,
            "Hooked by SageThumbs 2K",
            &format!("{ours}/{enabled}"),
        );
    } else if ours == 0 {
        r.fail_with_fix(
            "Hooked by SageThumbs 2K",
            &format!("0/{enabled} — no format is hooked"),
            "Registration never landed. Settings -> Advanced -> 'Repair file associations', \
             or reinstall.",
        );
    } else {
        r.line(
            S::Warn,
            "Hooked by SageThumbs 2K",
            &format!("{ours}/{enabled}"),
        );
    }

    if missing > 0 {
        r.line(
            S::Warn,
            "  not hooked",
            &format!("{missing}  e.g. {}", missing_examples.join(", ")),
        );
    }
    if stolen > 0 {
        r.line(
            S::Warn,
            "  owned by another program",
            &format!("{stolen}  e.g. {}", stolen_examples.join(", ")),
        );
    }
}

/// Every ProgID that could resolve `.ext`'s thumbnail before Explorer ever reaches the
/// SystemFileAssociations/bare-extension keys [`check_extensions`] audits: the per-user
/// `UserChoice` the shell honours first, then the class default under `.ext`. Mirrors
/// `typeoverlay.rs`'s private `progids_for` (same two sources, same rules) — duplicated
/// here rather than called because that function is not `pub(crate)` and this module stays
/// read-only registry access by design (see the module doc's "nothing is written").
fn progid_candidates(ext: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<String>| {
        if let Some(s) = s {
            let s = s.trim().to_string();
            if !s.is_empty() && !s.contains('\\') && !out.contains(&s) {
                out.push(s);
            }
        }
    };
    let user_choice =
        format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\UserChoice");
    push(
        CURRENT_USER
            .open(&user_choice)
            .ok()
            .and_then(|k| k.get_string("ProgId").ok()),
    );
    push(hkcr_default(&format!(".{ext}")));
    out
}

/// The ProgID-level half `check_extensions` cannot see: Windows resolves a thumbnail
/// handler at the **ProgID** level BEFORE it ever reaches the SystemFileAssociations or
/// bare-extension keys (`register.rs`'s module doc names the exact precedence: per-user
/// UserChoice ProgID, then the extension's default ProgID's shellex, THEN
/// SystemFileAssociations, THEN the bare-extension key). A side-by-side thumbnailer
/// (MysticThumbs, XnShellEx, the original SageThumbs) bound at that level wins for every
/// file of that type — invisibly to `check_extensions` AND to the user, who installed us
/// for exactly those formats and sees `check_extensions` report "Hooked by SageThumbs 2K"
/// while nothing changes on screen.
/// A handler whose DLL lives under the Windows directory is the OS's own (the Photos
/// thumbnailer that `.jpg`/`.png`'s default ProgID carries, say): `register.rs` documents
/// that one winning at the ProgID level as accepted, so it is not a competitor to report.
fn is_windows_own_handler(clsid: &str) -> bool {
    let Some(p) = inproc_path(clsid) else {
        return false;
    };
    let lower = p.trim_start_matches('"').to_ascii_lowercase();
    if lower.starts_with("%systemroot%") || lower.starts_with("%windir%") {
        return true;
    }
    std::env::var("SystemRoot")
        .map(|root| lower.starts_with(&root.to_ascii_lowercase()))
        .unwrap_or(false)
}

pub(super) fn check_progid_handlers(r: &mut Report, snap: &crate::settings::FormatEnabledSnapshot) {
    r.head("ProgID-level thumbnail handlers (checked before our own keys)");

    let mut total = 0usize;
    let mut examples: Vec<String> = Vec::new();

    for &(ext, _) in FORMATS.iter() {
        if !snap.enabled(ext) {
            continue;
        }
        for progid in progid_candidates(ext) {
            let thumb = hkcr_default(&format!(r"{progid}\shellex\{THUMB_HANDLER}"));
            let extract = hkcr_default(&format!(r"{progid}\shellex\{EXTRACT_IMAGE_HANDLER}"));
            for (kind, clsid) in [("IThumbnailProvider", thumb), ("IExtractImage", extract)] {
                let Some(clsid) = clsid else { continue };
                if clsid.eq_ignore_ascii_case(CLSID_THUMBNAIL_PROVIDER_STR)
                    || is_windows_own_handler(&clsid)
                {
                    continue;
                }
                total += 1;
                if examples.len() < 8 {
                    let dll = inproc_path(&clsid)
                        .and_then(|p| {
                            Path::new(&p)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| "(no InprocServer32)".to_string());
                    examples.push(format!(".{ext} -> {progid} [{kind} {clsid}] {dll}"));
                }
            }
        }
    }

    if total == 0 {
        r.line(S::Ok, "Foreign ProgID handlers", "none found");
    } else {
        r.fail_with_fix(
            "Foreign ProgID handlers",
            &format!(
                "{total} found — these win for their format no matter how healthy our own \
                 registration is:\n         {}",
                examples.join("\n         ")
            ),
            "another program's ProgID-level handler is checked before ours; uninstall or \
             reconfigure it, or reassociate the file type to remove its ProgID-level hook.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: reading only the bare-extension key mislabels an extension as
    /// "stolen" (or "missing") when the higher-priority SystemFileAssociations key still
    /// holds ours — and mislabels it "ours" the other way around when the SFA key was
    /// overwritten but the bare key happens to still be intact.
    #[test]
    fn effective_thumb_handler_prefers_system_file_associations_over_bare_key() {
        use std::collections::HashMap;
        const SFA: &str = "SystemFileAssociations\\.xyz\\shellex\\{THUMB}";
        const BARE: &str = ".xyz\\shellex\\{THUMB}";

        // SFA holds ours, bare was overwritten by another program — must read as OURS, not
        // stolen. This is exactly the case a bare-key-only read got wrong.
        let mut regs = HashMap::new();
        regs.insert(SFA, CLSID_THUMBNAIL_PROVIDER_STR.to_string());
        regs.insert(BARE, "{some-foreign-clsid}".to_string());
        let lookup = |k: &str| regs.get(k).cloned();
        assert_eq!(
            effective_thumb_handler(SFA, BARE, lookup),
            Some(CLSID_THUMBNAIL_PROVIDER_STR.to_string())
        );

        // SFA absent (never written on an old install) but bare set: falls back correctly.
        let mut regs2 = HashMap::new();
        regs2.insert(BARE, CLSID_THUMBNAIL_PROVIDER_STR.to_string());
        let lookup2 = |k: &str| regs2.get(k).cloned();
        assert_eq!(
            effective_thumb_handler(SFA, BARE, lookup2),
            Some(CLSID_THUMBNAIL_PROVIDER_STR.to_string())
        );

        // Neither set: None, same as before.
        let empty: HashMap<&str, String> = HashMap::new();
        let lookup3 = |k: &str| empty.get(k).cloned();
        assert_eq!(effective_thumb_handler(SFA, BARE, lookup3), None);
    }
}
