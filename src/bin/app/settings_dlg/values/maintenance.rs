//! The dialog's maintenance buttons: cache rebuild, diagnostics log, re-registration, association repair, settings export and import.

use super::*;

/// A background thumbnail-cache-rebuild / Explorer-restart worker (see [`spawn_cache_rebuild`])
/// finished → posted back with the boxed follow-up (WM_APP + 10; distinct from the sponsor
/// (+7) / update (+8) / sync (+9) app messages).
pub(in super::super) const WM_APP_CACHE: u32 = 0x8000 + 10;

/// What to do on the UI thread once [`spawn_cache_rebuild`]'s worker finishes: re-enable the
/// window, plus an optional (text, caption) message to show — deferred to here rather than
/// shown immediately after firing the worker, since showing it before the restart has actually
/// run would claim success too early.
pub(in super::super) struct CacheRebuiltEvent {
    pub(in super::super) after: Option<(&'static str, &'static str)>,
}

/// Run `restart_explorer_clearing_cache` (up to ~33s: an unconditional 3s sleep, then up to
/// two 15s polls) on a worker thread instead of blocking the Settings window's UI thread —
/// the shape `apply_settings`, `rebuild_thumbnail_cache`, and `repair_associations` all used
/// to share. The window is disabled for the duration (so a second click/Save can't race the
/// same restart) and re-enabled by the `WM_APP_CACHE` handler once the worker posts back,
/// along with `after`'s message, if any.
pub(in super::super) unsafe fn spawn_cache_rebuild(
    hwnd: HWND,
    after: Option<(&'static str, &'static str)>,
) {
    let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(hwnd, false);
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
        let raw = Box::into_raw(Box::new(CacheRebuiltEvent { after }));
        unsafe {
            let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(target as *mut core::ffi::c_void)),
                WM_APP_CACHE,
                WPARAM(0),
                LPARAM(raw as isize),
            );
            if posted.is_err() {
                drop(Box::from_raw(raw));
            }
        }
    });
}

/// Export the saved settings to a user-chosen `.json` file (Diagnostics ▸ Export). Written
/// ATOMICALLY (`settings_io::export_settings_to_path`, 2026-09-05 audit, F13) - replacing an
/// existing backup and then hitting a write failure must leave the OLD backup intact rather
/// than truncated, since the dialog is reporting the export as FAILED either way.
pub(in super::super) unsafe fn export_settings_to_file(hwnd: HWND) {
    let Some(path) = crate::win::pick_save_settings(hwnd, "SageThumbs2K-settings.json") else {
        return;
    };
    match crate::settings_io::export_settings_to_path(std::path::Path::new(&path)) {
        Ok(()) => msg(
            hwnd,
            &format!("Settings exported to:\n{path}"),
            "Export Settings",
            MB_ICONINFORMATION,
        ),
        Err(e) => msg(
            hwnd,
            &format!("Couldn't write the file:\n\n{e}"),
            "Export Settings",
            MB_ICONERROR,
        ),
    }
}

/// Import settings from a user-chosen `.json` file: apply them to HKCU, refresh the
/// dialog, and re-register the machine-wide shell hooks if the per-format enables
/// changed (Diagnostics ▸ Import).
pub(in super::super) unsafe fn import_settings_from_file(hwnd: HWND) {
    let Some(path) = crate::win::pick_open_settings(hwnd) else {
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return msg(
                hwnd,
                &format!("Couldn't read the file:\n\n{e}"),
                "Import Settings",
                MB_ICONERROR,
            )
        }
    };
    // Snapshot the per-format enables so we only trigger the (elevated) re-register when
    // the import actually changed which formats are hooked.
    let before: Vec<bool> = formats::FORMATS
        .iter()
        .map(|&(ext, _)| settings::format_enabled(ext))
        .collect();
    match crate::settings_io::import_settings(&text) {
        Err(e) => msg(hwnd, &e, "Import Settings", MB_ICONERROR),
        Ok(n) => {
            refresh_from_settings(hwnd);
            let formats_changed = formats::FORMATS
                .iter()
                .enumerate()
                .any(|(i, &(ext, _))| settings::format_enabled(ext) != before[i]);
            if formats_changed {
                // Sync the machine-wide HKCR hooks to the imported per-format flags. A
                // declined UAC prompt or a failed regsvr32 must not be reported as success —
                // roll the imported per-format flags back to their pre-import state (the
                // same rule `apply_format_flags` follows) so HKCU never disagrees with the
                // (unchanged) HKCR hooks, and refresh the model + list to match.
                if !matches!(reregister_elevated(), Reg::Ok) {
                    for (i, &(ext, _)) in formats::FORMATS.iter().enumerate() {
                        let _ = settings::set_format_enabled(ext, before[i]);
                    }
                    revert_format_list_view(hwnd);
                    message_box(hwnd, t("msg_admin_required"), "SageThumbs 2K");
                    return;
                }
                // Same reason as `apply_format_flags`: the elevated pass cannot write this
                // user's per-format shell pieces, so bring them in line here.
                let _ = sagethumbs2k_core::register::sync_user_shell();
            }
            msg(
                hwnd,
                &format!("Imported {n} settings — applied now."),
                "Import Settings",
                MB_ICONINFORMATION,
            );
        }
    }
}

/// An OK message box with an explicit info/error icon, for the import/export feedback.
/// (`win::message_box` is warning-only.)
pub(in super::super) unsafe fn msg(hwnd: HWND, text: &str, caption: &str, icon: MESSAGEBOX_STYLE) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(
        Some(hwnd),
        PCWSTR(t.as_ptr()),
        PCWSTR(c.as_ptr()),
        MB_OK | icon,
    );
}

/// Clear Windows' thumbnail cache and restart Explorer so thumbnails rebuild. Per-user,
/// no elevation needed (the cache lives in the user's own LocalAppData). Behind a confirm
/// — it briefly blinks the taskbar. This is the fix for the classic "I changed a setting
/// but the thumbnails look the same" (Explorer keeps serving stale cached thumbnails).
pub(in super::super) unsafe fn rebuild_thumbnail_cache(hwnd: HWND) {
    if !crate::win::confirm_warning(
        hwnd,
        "Rebuild Thumbnail Cache",
        "This clears Windows' thumbnail cache and briefly restarts File Explorer (your \
         taskbar will blink). Open windows and files are not affected.\n\nContinue?",
    ) {
        return;
    }
    // Kill Explorer (releases the cache files' lock), delete thumbcache_*.db, relaunch.
    // Must go through `shellcmd::cmd_c` — `Command::args` would escape the quotes for
    // the MSVCRT convention and `cmd` would misread them (see shellcmd, issue #5).
    // Backgrounded — see `spawn_cache_rebuild`; the success message shows once it's back.
    spawn_cache_rebuild(
        hwnd,
        Some((
            "Thumbnail cache cleared and Explorer restarted. Thumbnails will rebuild as you \
             browse.",
            "Rebuild Thumbnail Cache",
        )),
    );
}

/// Open the diagnostics log in the user's default text editor (or its folder if the
/// log doesn't exist yet), so a user can find it and send it in for a bug report.
pub(in super::super) unsafe fn open_diagnostics_log() {
    let path = match sagethumbs2k_core::safety::log_file() {
        Some(p) if p.exists() => p,
        // No log yet → open its folder (the user sees there's nothing to send).
        Some(p) => p.parent().map(|d| d.to_path_buf()).unwrap_or(p),
        None => return,
    };
    let file = wide(&path.display().to_string());
    let verb = wide("open");
    ShellExecuteW(
        Some(HWND::default()),
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Why a re-registration attempt ended the way it did. The distinction matters to the
/// user: "you declined the prompt" and "your antivirus ate the DLL" need opposite
/// actions, and both used to surface as the same cheerful success message.
pub(in super::super) enum Reg {
    Ok,
    /// The DLL is not on disk — the usual cause is security software quarantining it.
    MissingDll,
    /// `ShellExecute` could not start `regsvr32` (typically a declined UAC prompt).
    NotLaunched,
    /// `regsvr32` ran and reported failure.
    Failed(u32),
    /// `regsvr32` reported success but the CLSID still is not there.
    NotRegistered,
}

/// Re-run `regsvr32` elevated against the installed DLL, and **verify it worked**.
/// `register()` reads the per-extension flags we just wrote, so this brings the HKCR
/// `shellex` keys in line with the Options format list.
///
/// The old version returned success as soon as `ShellExecute` *launched* regsvr32 —
/// which says nothing about whether registration happened. A user whose DLL had been
/// quarantined got "File associations repaired." and still had no thumbnails. So now we
/// wait for the process, check its exit code, and then read the CLSID back.
pub(in super::super) unsafe fn reregister_elevated() -> Reg {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    let dll = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sagethumbs2k.dll")))
        .unwrap_or_default();
    if !dll.exists() {
        return Reg::MissingDll;
    }

    let params = wide(&format!("/s \"{}\"", dll.display()));
    let verb = wide("runas");
    let file = wide("regsvr32.exe");

    // ShellExecuteExW (not ShellExecuteW) — it is the only variant that hands back a
    // process handle, which is what lets us wait for an answer instead of assuming one.
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if ShellExecuteExW(&mut info).is_err() || info.hProcess.is_invalid() {
        return Reg::NotLaunched;
    }

    // regsvr32 is a fast, local registry write; 60 s is far past any legitimate run and
    // still bounded, so a wedged process can't hang the Settings window forever.
    let outcome = if WaitForSingleObject(info.hProcess, 60_000) == WAIT_OBJECT_0 {
        let mut code = 0u32;
        if GetExitCodeProcess(info.hProcess, &mut code).is_ok() && code != 0 {
            Reg::Failed(code)
        } else {
            Reg::Ok
        }
    } else {
        Reg::Failed(u32::MAX) // timed out — treat as failure rather than guess success
    };
    let _ = CloseHandle(info.hProcess);

    // Even a zero exit code gets checked against reality: this is the condition that
    // actually matters, and it is cheap to confirm.
    match outcome {
        Reg::Ok if !sagethumbs2k_core::register::is_registered() => Reg::NotRegistered,
        other => other,
    }
}

/// "Repair file associations" — the fix for blank/stuck thumbnails after another program
/// stole SageThumbs' shell hooks (the classic complaint), or an update left them stale.
/// Re-runs the full elevated registration (rewrites every enabled format's thumbnail /
/// context-menu / property hooks back to us), then clears the thumbnail cache + restarts
/// Explorer so the repaired thumbnails render immediately instead of serving stale blanks.
pub(in super::super) unsafe fn repair_associations(hwnd: HWND) {
    if !crate::win::confirm_warning(
        hwnd,
        "Repair File Associations",
        "This re-registers SageThumbs 2K for all your enabled file types — the fix when \
         thumbnails go blank after another program takes over a format — then clears the \
         thumbnail cache and briefly restarts File Explorer (your taskbar will blink).\n\nContinue?",
    ) {
        return;
    }
    // Report what actually happened. Each of these needs a different action from the
    // user, so collapsing them into one message is what made this button useless as a
    // diagnostic in the first place.
    match reregister_elevated() {
        Reg::Ok => {}
        Reg::MissingDll => {
            return msg(
                hwnd,
                "sagethumbs2k.dll is missing from the install folder, so there is nothing to \
                 register.\n\nThis is almost always security software quarantining it. Allow \
                 the SageThumbs 2K folder in your antivirus, then reinstall.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::NotLaunched => {
            return msg(
                hwnd,
                "Couldn't start regsvr32 — the elevation prompt was declined or failed. \
                 Nothing was changed.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::Failed(code) => {
            return msg(
                hwnd,
                &format!(
                    "regsvr32 could not register the shell extension (error {code}). \
                     Nothing was changed.\n\nRun 'st2k doctor' from the install folder and \
                     include its output in a bug report."
                ),
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::NotRegistered => {
            return msg(
                hwnd,
                "regsvr32 reported success, but the shell extension is still not registered.\
                 \n\nSomething is undoing the registration — usually security software. Run \
                 'st2k doctor' from the install folder and include its output in a bug report.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
    }
    // Re-registering can change which ProgID owns a type, and the type-overlay suppression
    // is written per ProgID — so re-point it at whatever owns the types NOW, or the repair
    // would silently leave the icon back on top of the badge.
    sagethumbs2k_core::typeoverlay::sync(sagethumbs2k_core::settings::hide_type_overlay());
    // The folder verb records an ABSOLUTE path to the companion EXE, so a repair after the app
    // moved (or a reinstall to a different directory) has to rewrite it or the entry silently
    // launches nothing.
    sagethumbs2k_core::foldermenu::sync(sagethumbs2k_core::settings::folder_prebuild_verb());
    // Registration rewrote the hooks; drop the stale cached thumbnails + restart Explorer so
    // the repaired ones render right away. (The cmd sequence gives regsvr32 time to finish.)
    // Backgrounded — see `spawn_cache_rebuild`; the success message shows once it's back.
    spawn_cache_rebuild(
        hwnd,
        Some((
            "File associations repaired. Thumbnails will rebuild as you browse.",
            "Repair File Associations",
        )),
    );
}
