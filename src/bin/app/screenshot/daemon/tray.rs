//! The tray icon: its data, menu and the balloons it raises.

use super::*;

/// Build a NOTIFYICONDATAW for our tray entry (hWnd + uID identify it for ADD/DELETE).
pub(super) unsafe fn tray_data(hwnd: HWND, with_payload: bool) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    if with_payload {
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = app_icon().unwrap_or_default();
        let mut tip_text = format!("SageThumbs 2K — Screenshot ({})", hotkey_label());
        // The autostart write can fail silently (AV, a locked hive, …) while THIS
        // session's daemon keeps running fine, so the tray tip is the one place that gap
        // is visible without opening the diagnostics log — a status line saying "Running"
        // with no other sign is exactly what preceded "PrtScn and Space do nothing" after
        // the next reboot.
        if super::super::enable::autostart_missing_while_wanted() {
            tip_text.push(' ');
            tip_text.push_str(crate::win::t("shot_tray_no_autostart"));
        }
        let tip = wide(&tip_text);
        // `szTip` is a fixed 128-`u16` buffer with no NUL guard of its own — `.take(127)`
        // leaves the last slot at its zeroed default so the string stays NUL-terminated even
        // if `tip` somehow reached the full 128 (dormant today: both halves above are short
        // and fixed-table).
        for (d, s) in nid.szTip.iter_mut().zip(tip.iter().take(127)) {
            *d = *s;
        }
    }
    nid
}

/// Human label for the currently-configured capture hotkey (e.g. "Ctrl + PrtScn"),
/// for the tray tooltip — so a remapped hotkey is shown correctly instead of the
/// hardcoded default. The stored value always comes from the Settings dropdown, so
/// it matches one of the presets; an unknown value falls back to the default label.
pub(super) fn hotkey_label() -> &'static str {
    let (m, v) = st2k_base::settings::screenshot_hotkey();
    let packed = (m << 8) | v;
    crate::screenshot::SHOT_PRESETS
        .iter()
        .find(|&&(_, p)| p == packed)
        .map_or("Ctrl + PrtScn", |&(label, _)| label)
}

/// Add the tray icon, retrying on a short timer until the shell accepts it. `NIM_ADD`
/// fails when the taskbar doesn't exist yet (autostart at logon racing Explorer) — one
/// silent attempt left the icon permanently missing. If the add fails because the icon
/// is ALREADY there (a redundant call), `NIM_MODIFY` succeeds and settles it — so the
/// retry timer only survives genuine "no taskbar yet" failures.
pub(super) unsafe fn ensure_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd, true);
    if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
    } else {
        let _ = SetTimer(Some(hwnd), TRAY_RETRY_TIMER_ID, TRAY_RETRY_MS, None);
    }
}

pub(super) unsafe fn remove_tray_icon(hwnd: HWND) {
    // Cancel any pending add-retry too, so a hide can't be undone by a late retry tick.
    let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
    let nid = tray_data(hwnd, false);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

pub(super) unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    // Translated, so `wide` buffers must outlive the AppendMenuW calls that point at them.
    let (cap, ocr, ups, set, hide, quit) = (
        wide(crate::win::t("tray_capture")),
        wide(crate::win::t("tray_ocr")),
        wide(crate::win::t("tray_uploads")),
        wide(crate::win::t("tray_settings")),
        wide(crate::win::t("tray_hide")),
        wide(crate::win::t("tray_quit")),
    );
    let _ = AppendMenuW(menu, MF_STRING, IDM_CAPTURE, PCWSTR(cap.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_OCR, PCWSTR(ocr.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_UPLOADS, PCWSTR(ups.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, PCWSTR(set.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_HIDE, PCWSTR(hide.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, PCWSTR(quit.as_ptr()));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when the user clicks elsewhere.
    crate::win::force_foreground(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

/// Copy a balloon string into one of `NOTIFYICONDATAW`'s fixed buffers, always leaving
/// the terminating NUL in place: a string longer than the buffer (a translated licence
/// notice can be) is cut, never left unterminated for the shell to read past.
pub(super) fn set_balloon_text(dst: &mut [u16], text: &str) {
    let src = wide(text);
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}

/// Pop a tray "update available" balloon (clickable → the releases page). A no-op if the
/// tray icon is hidden, in which case the next Settings open still surfaces the update.
pub(super) unsafe fn show_update_toast(hwnd: HWND, tag: &str) {
    LAST_BALLOON.store(BALLOON_UPDATE, Ordering::Relaxed);
    let mut nid = tray_data(hwnd, false);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    set_balloon_text(&mut nid.szInfoTitle, "SageThumbs 2K update available");
    set_balloon_text(
        &mut nid.szInfo,
        &format!("Version {tag} is ready — click to download."),
    );
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Pop a tray balloon explaining that Space cannot work over the window now in front.
///
/// The title names the program (Everything / File Explorer) so it is obvious WHICH window is the
/// problem when several are open. A no-op if the tray icon is hidden, which is the accepted floor
/// here: `st2k doctor` still reports it, and there is no other channel that does not steal focus
/// from the very window the user is working in.
pub(super) unsafe fn show_elevated_warning(hwnd: HWND, kind: &str) {
    // Always logged, not just under verbose: this is the answer to "I pressed Space and nothing
    // happened", and a support reply should not depend on the user having had logging on before
    // the thing they are reporting happened.
    st2k_base::safety::log(&format!(
        "quick preview: {kind} is running elevated — its keystrokes never reach us, warning shown"
    ));
    LAST_BALLOON.store(BALLOON_ELEVATED, Ordering::Relaxed);
    let mut nid = tray_data(hwnd, false);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_WARNING;
    set_balloon_text(
        &mut nid.szInfoTitle,
        &format!("{kind}: {}", crate::win::t("admin_warn_title")),
    );
    set_balloon_text(&mut nid.szInfo, crate::win::t("admin_warn_body"));
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Open the GitHub releases page in the default browser (the update toast's click target).
pub(super) unsafe fn open_releases() {
    let url = wide(crate::update::RELEASES_URL);
    ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(url.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}
