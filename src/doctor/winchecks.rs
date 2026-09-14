//! Windows-side thumbnail settings and quirk probes for `st2k doctor`: the per-user and
//! Group Policy switches that can silently turn thumbnails off machine-wide, the custom
//! "This PC" namespace entries a tweaker can add, and whether the "press Space to
//! preview" keyboard hook can reach an elevated window.

use super::*;

/// Windows-side switches that disable thumbnails for EVERY program, not just us. When
/// one of these is set the extension is registered perfectly and still shows nothing,
/// which is the most misleading failure mode there is.
pub(super) fn check_windows_switches(r: &mut Report) {
    r.head("Windows thumbnail settings");

    let advanced = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
    let icons_only = CURRENT_USER
        .open(advanced)
        .ok()
        .and_then(|k| k.get_u32("IconsOnly").ok());
    match icons_only {
        Some(1) => r.fail_with_fix(
            "IconsOnly",
            "1 — Windows is set to 'Always show icons, never thumbnails'",
            "UNCHECK it in the real dialog: File Explorer -> ... -> Options -> View tab -> \
             'Always show icons, never thumbnails'. Use the CHECKBOX, not the registry — \
             Explorer keeps its own copy of this value and writes it back over yours when it \
             exits, so a registry edit + restart silently reverts. Afterwards clear the \
             thumbnail cache (Disk Cleanup, or delete \
             %LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db with Explorer \
             closed): while the switch was on, Explorer recorded 'no thumbnail' for every \
             file it saw and keeps serving those stale answers.",
        ),
        Some(v) => r.line(S::Ok, "IconsOnly", &format!("{v} — thumbnails allowed")),
        None => r.line(S::Ok, "IconsOnly", "unset — thumbnails allowed"),
    }

    // Explorer's "Display file icon on thumbnails" (View > Options > View tab). Off, Windows
    // draws NO program icon on any thumbnail, ours or its own, and the corner-mark setting
    // "Windows' file-type icon" reads as broken while every registry line above is green. A
    // South Korean reporter (2026-09-10) toggled it by hand and could not say what Explorer
    // held afterwards; this line is so the report says.
    let overlay = CURRENT_USER
        .open(advanced)
        .ok()
        .and_then(|k| k.get_u32("ShowTypeOverlay").ok());
    match overlay {
        Some(0) => r.line(
            S::Warn,
            "ShowTypeOverlay",
            "0 — 'Display file icon on thumbnails' is OFF, so Windows draws no program icon on \
             any thumbnail (ours or its own). Tick it in File Explorer -> ... -> Options -> \
             View tab, Apply, then restart Explorer; until then the corner setting \"Windows' \
             file-type icon\" cannot show anything",
        ),
        Some(v) => r.line(
            S::Ok,
            "ShowTypeOverlay",
            &format!("{v} — 'Display file icon on thumbnails' is on"),
        ),
        None => r.line(
            S::Ok,
            "ShowTypeOverlay",
            "unset — 'Display file icon on thumbnails' is on (the default)",
        ),
    }

    // Performance Options -> "Adjust for best performance" switches OFF the "Show thumbnails
    // instead of icons" visual effect, which IS IconsOnly. Worth its own check because of how
    // it presents: the profile re-applies its own value, so IconsOnly can READ 0 while Explorer
    // keeps behaving as though it were 1, and every registry-level check says everything is
    // fine. That contradiction cost hours on a real machine (2026-08-05).
    //
    // Keyed ONLY on VisualFXSetting == 2. The per-effect `ThumbnailsOrIcon\DefaultApplied`
    // reads 1 on perfectly healthy machines (it means "this effect is at its profile default",
    // not "off"), so reporting on it would fail every install that works — the same false-alarm
    // mistake the DisableThumbnailCache note above records from issue #11.
    let visual_fx = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\VisualEffects")
        .ok()
        .and_then(|k| k.get_u32("VisualFXSetting").ok());
    if let Some(detail) = performance_profile_detail(visual_fx, icons_only) {
        r.fail_with_fix(
            "Performance profile",
            detail,
            "System Properties -> Advanced -> Performance -> Settings -> pick 'Custom' (or \
             'Adjust for best appearance') and TICK 'Show thumbnails instead of icons', then \
             Apply. Fixing IconsOnly alone will not hold while this profile is set.",
        );
    }

    check_thumbnail_policies(r);
    this_pc_namespace_section(r);
}

/// The verdict on the performance profile, split out so it can be tested against every
/// combination without a live registry (and without setting "best performance" on a real
/// machine to see what happens, which would switch that machine's own thumbnails off).
///
/// `None` means say nothing. Only `VisualFXSetting == 2` is worth reporting; see the call
/// site for why the per-effect `DefaultApplied` value must NOT be used for this.
fn performance_profile_detail(
    visual_fx: Option<u32>,
    icons_only: Option<u32>,
) -> Option<&'static str> {
    if visual_fx != Some(2) {
        return None;
    }
    Some(match icons_only {
        // The nasty shape, and the reason this check exists: the switch reads as allowed, so
        // every registry-level check passes, while the profile keeps turning it back off.
        Some(0) | None => {
            "2 — 'Adjust for best performance' is on. It owns the thumbnail switch and will \
             keep turning it back off, even though IconsOnly currently reads as allowed"
        }
        _ => "2 — 'Adjust for best performance' is on, which is what turned thumbnails off",
    })
}

/// Group Policy's thumbnail switches, split from the per-user ones above purely for length.
fn check_thumbnail_policies(r: &mut Report) {
    // Group Policy can kill thumbnails machine-wide or per-user. Only `DisableThumbnails`
    // actually does that; the two *Cache* values disable the on-disk thumbnail CACHE
    // (thumbcache_*.db) and nothing else — thumbnails still generate, they are just
    // recomputed every time. Reporting those as "thumbnails are disabled" sent a reporter
    // (issue #11) chasing four scary FAILs on an install whose thumbnails worked fine.
    let pol = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let mut any_policy = false;
    let mut cache_off = false;
    for (root, root_name) in [(CURRENT_USER, "HKCU"), (LOCAL_MACHINE, "HKLM")] {
        if let Some(1) = root
            .open(pol)
            .ok()
            .and_then(|k| k.get_u32("DisableThumbnails").ok())
        {
            any_policy = true;
            r.fail_with_fix(
                &format!("{root_name}\\...\\DisableThumbnails"),
                "1 — policy disables thumbnails",
                "Set this value to 0 or delete it (Group Policy / registry).",
            );
        }
        for value in ["NoThumbnailCache", "DisableThumbnailCache"] {
            if let Some(1) = root.open(pol).ok().and_then(|k| k.get_u32(value).ok()) {
                cache_off = true;
                r.line(
                    S::Info,
                    &format!("{root_name}\\...\\{value}"),
                    "1 — thumbnail CACHE off (thumbnails still work, just slower)",
                );
            }
        }
    }
    if cache_off {
        r.line(
            S::Info,
            "Thumbnail cache",
            "disabled by policy — every thumbnail is recomputed on each visit",
        );
    }
    // The NETWORK-only switch, which is a different value from `DisableThumbnails` and hides
    // in exactly the shape that reads as our bug: everything on C: thumbnails perfectly and
    // everything on a mapped drive shows a plain icon, so the user concludes the extension is
    // broken for "those files". A direct `IShellItemImageFactory` call is not affected by it
    // either, so the per-file probe below can hand back a real thumbnail for a path that
    // Explorer will still refuse to draw one for (issue #36). Reported wherever it is set,
    // and again per-file when the file is actually on such a drive.
    if network_thumbnails_disabled() {
        any_policy = true;
        r.line(
            S::Warn,
            r"...\DisableThumbnailsOnNetworkFolders",
            "1 — policy turns thumbnails off for NETWORK folders only; local drives are \
             unaffected, which is why this looks like a per-file fault",
        );
        r.line(
            S::Info,
            "  to turn them on",
            "Group Policy: User Configuration > Administrative Templates > Windows Components \
             > File Explorer > 'Turn off the display of thumbnails and only display icons on \
             network folders' > Disabled. Or delete that value under \
             HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer.",
        );
    }
    if !any_policy {
        r.line(S::Ok, "Thumbnail policies", "no disabling policy found");
    }
}

/// Windows' separate thumbnail switch for NETWORK folders. See its caller for why it is worth
/// its own check rather than being folded into the `DisableThumbnails` loop.
pub(super) fn network_thumbnails_disabled() -> bool {
    let pol = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    [CURRENT_USER, LOCAL_MACHINE].into_iter().any(|root| {
        matches!(
            root.open(pol)
                .ok()
                .and_then(|k| k.get_u32("DisableThumbnailsOnNetworkFolders").ok()),
            Some(1)
        )
    })
}

/// Whether `path` lives somewhere Windows considers a network location: a UNC path, or a
/// drive letter mapped to a remote share. `GetDriveTypeW` wants a root (`R:\`), not the file.
pub(super) fn is_network_path(path: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    // `DRIVE_REMOTE` lives under `System::WindowsProgramming`, not beside `GetDriveTypeW`.
    use windows::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    if path.starts_with(r"\\") && !path.starts_with(r"\\?\") {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' {
        return false;
    }
    let root: Vec<u16> = path[..2]
        .encode_utf16()
        .chain(['\\' as u16, 0])
        .collect::<Vec<u16>>();
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_REMOTE }
}

/// The custom "This PC" entries on this machine: `(display name, target folder)` for every
/// shell namespace entry under `Explorer\MyComputer\NameSpace` (both hives) that carries an
/// `Instance\InitPropertyBag\TargetFolderPath`, which is what a folder added to This PC by a
/// tweaker (Winaero's ThisPCTweaker, issue #37) looks like. Windows' own entries there are
/// known-folder CLSIDs with no such property bag, so they never match. Registry read only.
fn this_pc_namespace_entries() -> Vec<(String, String)> {
    const NAMESPACE: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\MyComputer\NameSpace";
    let mut out = Vec::new();
    for root in [CURRENT_USER, LOCAL_MACHINE] {
        let Ok(namespace) = root.open(NAMESPACE) else {
            continue;
        };
        let Ok(clsids) = namespace.keys() else {
            continue;
        };
        for clsid in clsids {
            let Ok(entry) = namespace.open(&clsid) else {
                continue;
            };
            let Ok(bag) = entry.open(r"Instance\InitPropertyBag") else {
                continue;
            };
            let Ok(target) = bag.get_string("TargetFolderPath") else {
                continue;
            };
            let target = expand_env_strings(target.trim());
            if target.is_empty() {
                continue;
            }
            let name = entry
                .get_string("")
                .ok()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| clsid.clone());
            out.push((name, target));
        }
    }
    out
}

/// `%VAR%` expansion for a REG_EXPAND_SZ read back raw; an unknown variable is left as is.
fn expand_env_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Issue #37's real cause, per file. A folder added to This PC by a tweaker is a shell
/// namespace entry, and items browsed through it carry that namespace identity: Explorer keys
/// its thumbnail cache and view on the entry, our provider is asked and answers, and the
/// picture never lands on the tile - while the same file by its real path thumbnails at once.
pub(super) fn this_pc_namespace_note(r: &mut Report, p: &Path) {
    let Ok(file) = p.canonicalize() else {
        return;
    };
    let file = file
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_lowercase();
    for (name, target) in this_pc_namespace_entries() {
        let prefix = target.trim_end_matches('\\').to_lowercase();
        if prefix.is_empty() || !file.starts_with(&prefix) {
            continue;
        }
        r.line(
            S::Warn,
            "Custom This PC entry",
            &format!(
                "this file is under '{name}' ({target}), a folder added to This PC by a \
                 tweaker. Browsed through that entry, Explorer keys the tile on the entry \
                 rather than the file and the picture may never appear; open the real folder \
                 path instead, or pin it to Home or Quick access"
            ),
        );
        return;
    }
}

/// The machine-wide half of the same finding, for the Windows section: every custom This PC
/// entry, named, whether or not the probed file sits under one.
fn this_pc_namespace_section(r: &mut Report) {
    for (name, target) in this_pc_namespace_entries() {
        r.line(
            S::Warn,
            "Custom This PC entry",
            &format!(
                "'{name}' -> {target}: files browsed through it may not show thumbnails; \
                 pin the real folder to Home or Quick access instead"
            ),
        );
    }
}

/// Build the whole report. Read-only; safe to run unelevated, and safe to paste. When
/// `file` is given, a per-file probe section is appended (`st2k doctor <path>`).
/// What to call a window class in a warning, for the classes whose keystrokes the Space preview
/// has to see. `None` for anything the feature never serves.
///
/// Shared with the daemon's live watcher (`screenshot::elevwarn`) so the report and the warning
/// can never disagree about which windows the feature covers.
pub fn served_window_kind(cls: &str) -> Option<&'static str> {
    match cls {
        "CabinetWClass" | "ExploreWClass" => Some("File Explorer"),
        _ if cls.starts_with("EVERYTHING") => Some("Everything"),
        _ => None,
    }
}

use crate::prebuild::process_is_elevated;

unsafe extern "system" fn collect_elevated(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    };
    let found = &mut *(lparam.0 as *mut Vec<String>);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 {
        return true.into();
    }
    let cls = String::from_utf16_lossy(&buf[..n as usize]);
    let Some(kind) = served_window_kind(&cls) else {
        return true.into();
    };
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != 0 && process_is_elevated(pid) {
        let entry = kind.to_string();
        if !found.contains(&entry) {
            found.push(entry);
        }
    }
    true.into()
}

/// The windows open RIGHT NOW that the Space preview would serve but cannot, because they run
/// elevated.
fn elevated_served_windows() -> Vec<String> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    let mut found: Vec<String> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect_elevated),
            LPARAM(core::ptr::addr_of_mut!(found) as isize),
        );
    }
    found
}

/// Why "press Space to preview" can be doing nothing at all.
///
/// This is the ONE failure the feature cannot report for itself. Windows withholds keystrokes
/// typed into an ELEVATED window from ordinary programs, and our keyboard hook is an ordinary
/// program, so the keypress never arrives: there is no failed attempt to notice, only silence.
/// Measured rather than assumed — a non-elevated low-level hook saw every key typed into a
/// normal window and NONE of the keys typed into an elevated one.
///
/// So the situation is detected instead of the keypress. Everything is the usual culprit,
/// because 1.4 asks for administrator the first time it indexes an NTFS drive and people
/// understandably leave it that way.
pub(super) fn check_space_preview(r: &mut Report) {
    r.head("Press Space to preview");
    if !crate::settings::preview_enabled() {
        r.line(
            S::Info,
            "Quick preview",
            "off — this feature is off by default; turn it on in Settings, Quick preview",
        );
        return;
    }
    r.line(S::Ok, "Quick preview", "on");
    let blocked = elevated_served_windows();
    if blocked.is_empty() {
        r.line(
            S::Ok,
            "Keystrokes reach us",
            "no window open right now is running as administrator",
        );
        return;
    }
    for kind in &blocked {
        r.fail_with_fix(
            "Running as administrator",
            &format!(
                "{kind} is running as administrator, so Windows never delivers the Space \
                 keypress to us and the preview cannot appear"
            ),
            &format!(
                "Restart {kind} as a standard user. In Everything: Tools, Options, General, \
                 untick 'Run as administrator', tick 'Everything Service', then exit and \
                 restart it. This cannot be fixed from SageThumbs' side."
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The performance-profile verdict, over every combination that matters.
    ///
    /// Testing this through the real registry would mean setting "Adjust for best performance"
    /// on the machine running the tests, which switches that machine's own thumbnails off —
    /// hence the pure helper.
    #[test]
    fn performance_profile_only_fires_on_best_performance() {
        // Not the performance profile: silent, whatever IconsOnly says. `None` covers the
        // common case of the value never having been written.
        for fx in [None, Some(0), Some(1), Some(3)] {
            for icons in [None, Some(0), Some(1)] {
                assert_eq!(
                    performance_profile_detail(fx, icons),
                    None,
                    "VisualFXSetting={fx:?} IconsOnly={icons:?} should not be reported"
                );
            }
        }
        // "Best performance" always reports, INCLUDING when IconsOnly reads fine — that
        // combination is the whole reason the check exists, so pin its wording.
        let looks_fine = performance_profile_detail(Some(2), Some(0))
            .expect("best-performance + IconsOnly=0 must be reported");
        assert!(
            looks_fine.contains("keep turning it back off"),
            "the IconsOnly-looks-fine case must explain the contradiction: {looks_fine}"
        );
        assert_eq!(
            performance_profile_detail(Some(2), None),
            performance_profile_detail(Some(2), Some(0)),
            "an unset IconsOnly is the same 'looks allowed' case as an explicit 0"
        );
        let already_off = performance_profile_detail(Some(2), Some(1))
            .expect("best-performance + IconsOnly=1 must be reported");
        assert!(
            already_off.contains("turned thumbnails off"),
            "{already_off}"
        );
    }
}
