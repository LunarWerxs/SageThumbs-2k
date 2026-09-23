//! Running a dialog window and the confirmations built on it.

use super::*;

/// Register a class, create + show a dialog, run its message pump. `w`/`h` are 96-DPI design px.
#[allow(clippy::too_many_arguments)]
pub unsafe fn run_dialog(
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    w: i32,
    h: i32,
    modal: Option<HWND>,
) -> Option<HWND> {
    let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let dark = crate::dark::is_dark();
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        // A top-level dialog carries the app icon + arrow cursor; the modal popup
        // inherits its owner's icon (the original popup set neither).
        hIcon: if modal.is_none() {
            app_icon().unwrap_or_default()
        } else {
            Default::default()
        },
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        // The palette's window tone in both themes: it is what `dark_ctlcolor` hands every
        // control, so the window behind them has to be the same colour or each one reads as
        // a block.
        hbrBackground: crate::dark::dark_bg_brush(),
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent: re-register returns 0 (already registered) — fine

    // Geometry: design pixels scaled to the DPI of the monitor the window will actually
    // open on, then placed there.
    //   - modal popup: the owner's DPI, centered over the owner.
    //   - top-level:   the CURSOR monitor's DPI, centered on that monitor's work area.
    //
    // Top-level dialogs used to open at CW_USEDEFAULT, which cascades from the TOP-LEFT
    // corner of the primary monitor — so the welcome window (and every other dialog that
    // comes through here: convert, feedback, image info, doctor report, …) opened in the
    // corner of the screen rather than in front of the user. The Settings window already
    // sizes AND positions itself this way (see `main.rs`); this brings the rest in line.
    // Sizing to the cursor monitor also makes the frame DPI agree with the per-control
    // `dpi_scale()` (`GetDpiForWindow`) on mixed-DPI multi-monitor setups.
    let (ex_style, style, x, y, sw, sh, parent) = match modal {
        None => {
            let (mon_dpi, work) = cursor_monitor_metrics();
            let (sw, sh) = (dpi_scale_dpi(w, mon_dpi), dpi_scale_dpi(h, mon_dpi));
            (
                WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                work.left + ((work.right - work.left) - sw).max(0) / 2,
                work.top + ((work.bottom - work.top) - sh).max(0) / 2,
                sw,
                sh,
                None,
            )
        }
        Some(owner) => {
            let creation_dpi = GetDpiForWindow(owner) as i32;
            let (sw, sh) = (
                dpi_scale_dpi(w, creation_dpi),
                dpi_scale_dpi(h, creation_dpi),
            );
            // Center over the owner.
            let mut orc = RECT::default();
            let _ = GetWindowRect(owner, &mut orc);
            (
                WS_EX_DLGMODALFRAME,
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                orc.left + ((orc.right - orc.left) - sw) / 2,
                orc.top + ((orc.bottom - orc.top) - sh) / 2,
                sw,
                sh,
                Some(owner),
            )
        }
    };

    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        ex_style,
        class,
        PCWSTR(title_w.as_ptr()),
        style,
        x,
        y,
        sw,
        sh,
        parent,
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }

    match modal {
        None => {
            let _ = ShowWindow(hwnd, SW_SHOW);
            // These dialogs are launched by the DLL from inside Explorer's context
            // menu, i.e. by a freshly spawned process that Windows may refuse the
            // foreground to. Without this, "Convert…" can open BEHIND the Explorer
            // window that launched it and read as "the menu item did nothing".
            // Same root cause as the screenshot overlay's dead Esc key.
            force_foreground(hwnd);
            pump_until_quit(hwnd);
        }
        Some(owner) => {
            let _ = EnableWindow(owner, false);
            let _ = ShowWindow(hwnd, SW_SHOW);
            force_foreground(hwnd);
            pump_until_closed(hwnd);
            let _ = EnableWindow(owner, true);
        }
    }
    Some(hwnd)
}

/// The lifecycle tail every worker-backed dialog (Convert, Rename, Files-to-folder,
/// Tags-to-folders) shares once its own messages are handled: a DPI change re-lays out; the
/// title-bar X / Alt+F4 / taskbar close go through the dialog's own `request_close`, which
/// mirrors IDCANCEL's deferred close - a batch started on the worker thread must not be torn out
/// from under it by an unconditional `DestroyWindow` (WM_CLOSE would cascade to WM_DESTROY ->
/// `PostQuitMessage` and kill the worker mid-write); destroy quits the pump; the rest is the
/// default.
pub unsafe fn dialog_tail(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    request_close: unsafe fn(HWND),
) -> LRESULT {
    match msg {
        WM_DPICHANGED => {
            wm_dpichanged(hwnd, lparam);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_close(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// A Yes/No question under the warning icon, for an action that briefly disrupts the desktop
/// (restarting Explorer, re-registering every format); `true` when the user chose Yes.
pub unsafe fn confirm_warning(parent: HWND, title: &str, body: &str) -> bool {
    let w_body = wide(body);
    let w_title = wide(title);
    MessageBoxW(
        Some(parent),
        PCWSTR(w_body.as_ptr()),
        PCWSTR(w_title.as_ptr()),
        MB_YESNO | MB_ICONWARNING,
    ) == IDYES
}

/// Show a simple warning message box owned by the dialog.
pub unsafe fn message_box(hwnd: HWND, text: &str, caption: &str) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(
        Some(hwnd),
        PCWSTR(t.as_ptr()),
        PCWSTR(c.as_ptr()),
        MB_OK | MB_ICONWARNING,
    );
}

/// A modal two-choice prompt whose BUTTONS CARRY THE VERBS ("Renew" / "Not now"), rather
/// than a `MessageBox`'s fixed Yes/No. Returns true when the first (affirmative) button was
/// chosen; anything else - the second button, Escape, or the close box - is false, which
/// every caller must treat as "do nothing".
///
/// `TaskDialogIndirect` rather than `MessageBoxW` because a Yes/No pair makes the reader
/// reconstruct which verb "Yes" meant from the sentence above it, and a dialog offering to
/// spend money is the worst place to make anyone guess. The app already links comctl32 v6
/// (its manifest and every owner-drawn control depend on it), so this costs no new
/// dependency; on the impossible failure path it falls back to a plain `MB_YESNO` so the
/// choice is still offered.
pub unsafe fn confirm_verbs(
    parent: HWND,
    title: &str,
    body: &str,
    yes_label: &str,
    no_label: &str,
) -> bool {
    use windows::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS,
        TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW,
    };

    // Arbitrary ids; only their identity matters, and neither collides with IDOK/IDCANCEL.
    const ID_YES: i32 = 1001;
    const ID_NO: i32 = 1002;

    let w_title = wide(title);
    let w_body = wide(body);
    let w_yes = wide(yes_label);
    let w_no = wide(no_label);
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: ID_YES,
            pszButtonText: PCWSTR(w_yes.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_NO,
            pszButtonText: PCWSTR(w_no.as_ptr()),
        },
    ];

    let mut cfg = TASKDIALOGCONFIG {
        cbSize: core::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: parent,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        // No stock buttons at all: the two custom ones below carry the whole choice.
        dwCommonButtons: TASKDIALOG_COMMON_BUTTON_FLAGS(0),
        pszWindowTitle: PCWSTR(w_title.as_ptr()),
        pszContent: PCWSTR(w_body.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: ID_NO,
        ..Default::default()
    };
    // `pszMainIcon` is a union; the information icon is the same intent `MB_ICONINFORMATION`
    // carried on the MessageBox this replaced.
    cfg.Anonymous1.pszMainIcon = PCWSTR(-3isize as *const u16); // TD_INFORMATION_ICON

    let mut pressed = 0i32;
    // SAFETY: every pointer in `cfg` borrows a local that outlives this call, and the call
    // is synchronous - the dialog is gone before any of them drop.
    if TaskDialogIndirect(&cfg, Some(&mut pressed), None, None).is_ok() {
        return pressed == ID_YES;
    }

    // comctl32 refused (no v6 activation context in some embedding we do not control):
    // still ask, just with the generic buttons.
    MessageBoxW(
        Some(parent),
        PCWSTR(w_body.as_ptr()),
        PCWSTR(w_title.as_ptr()),
        MB_YESNO | MB_ICONINFORMATION,
    ) == IDYES
}
