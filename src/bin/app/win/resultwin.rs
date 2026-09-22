//! The shared result window: a read-only edit with Copy and Close.

use super::*;

/// Shared geometry for the three "result" dialogs — Image info, Upload links, OCR text — which
/// are all the same shape: a full-width scrollable EDIT with a Copy + Close button row at the
/// bottom right. All values are 96-DPI DESIGN px, i.e. what [`ctl`] takes.
///
/// It has to be computed from the REAL client rect: [`run_dialog`]'s `w`/`h` size the whole
/// WINDOW, so the client is narrower and shorter by the frame + caption, and laying out against
/// the design size instead clipped the edit's scrollbar and the Close button off the right edge.
/// `GetClientRect` is physical px, so divide back by the window's DPI to land in design px.
pub(crate) struct ResultLayout {
    pub(crate) cw: i32,
    pub(crate) m: i32,
    pub(crate) btn_w: i32,
    pub(crate) btn_h: i32,
    pub(crate) gap: i32,
    /// Top of the button row.
    pub(crate) btn_y: i32,
    /// Close sits rightmost, Copy immediately to its left.
    pub(crate) close_x: i32,
    pub(crate) copy_x: i32,
}

/// The window-message half those same three dialogs share: create the controls, run Copy,
/// close on OK/Cancel/X, and quit the pump on destroy. Returns `Some` when it handled the
/// message; the caller falls through to `DefWindowProcW` on `None`.
///
/// `build` lays the dialog out (it gets the module handle already resolved). `copy` returns the
/// text the Copy button should put on the clipboard — that is the ONLY behavioural difference
/// between the three: Image info copies its stored dump, Upload copies just the links (not the
/// heading above them), and OCR copies the EDIT's *current* contents so a correction the user
/// typed is honoured.
///
/// Keeping this in one place is the point: `IDOK | IDCANCEL` must both close (Esc arrives as an
/// IDCANCEL command from `IsDialogMessageW` even though no control carries that id), and
/// `WM_DESTROY` must `PostQuitMessage` when the window is a top-level dialog pumped by
/// [`run_dialog`]'s `pump_until_quit` — a copy of this that gets one of those wrong is a window
/// that won't close.
///
/// ...and must NOT when the window was opened modally over another one (Doctor or Recent uploads
/// from Settings, Recent uploads from the upload result). That pump (`pump_until_closed`) stops
/// on its own once the window is gone, so a `WM_QUIT` from it was left in the thread's queue for
/// the OWNER's loop, and closing the child closed Settings with it (found reviewing Recent
/// uploads, 2026-09-21; Doctor had carried it since it moved onto this shared procedure). Owned
/// = modal, because `run_dialog` passes the owner as the popup's parent.
pub(crate) unsafe fn result_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    build: unsafe fn(HWND, HINSTANCE),
    copy: unsafe fn(HWND) -> String,
) -> Option<LRESULT> {
    match msg {
        WM_CREATE => {
            let Ok(h) = GetModuleHandleW(None) else {
                return Some(LRESULT(-1)); // fail the create rather than build into nothing
            };
            build(hwnd, h.into());
            Some(LRESULT(0))
        }
        WM_COMMAND => {
            match super::command_id(wparam) {
                ID_RESULT_COPY => {
                    let _ = set_clipboard_text(&copy(hwnd));
                }
                IDOK | IDCANCEL => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            Some(LRESULT(0))
        }
        WM_DESTROY => {
            if !is_owned(hwnd) {
                PostQuitMessage(0); // let run_dialog's pump_until_quit exit
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Whether `hwnd` has an owner window, i.e. was opened modally by [`run_dialog`].
unsafe fn is_owned(hwnd: HWND) -> bool {
    GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid())
}

/// Control id of the Copy button in every result dialog (see [`result_wndproc`]).
pub(crate) const ID_RESULT_COPY: i32 = 101;

/// The edit every result window fills its body with: `style` says read-only or editable (the
/// scrollbar, border and tab stop are added here), it starts at `y` and stops above the button
/// row, its scrollbar is re-themed dark, and it is filled with `text` in the CRLF the control
/// wants. Returns the edit for a caller that goes on to set a font on it.
///
/// `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar; DarkMode_Explorer
/// renders it dark (the edit's own bg/text stay dark via WM_CTLCOLOREDIT in `dark_ctlcolor`). And
/// edit controls want CRLF line breaks (a lone LF renders as a box): `to_crlf` rather than a
/// one-way `\n` -> `\r\n` replace, because a line that is ALREADY CRLF (any EXIF/XMP value carrying
/// its own line breaks) would come out as `\r\r\n` and show a stray box anyway.
pub(crate) unsafe fn result_edit(
    hwnd: HWND,
    hinst: HINSTANCE,
    l: &ResultLayout,
    y: i32,
    style: WINDOW_STYLE,
    id: i32,
    text: &str,
) -> HWND {
    let edit_h = (l.btn_y - l.gap - y).max(48);
    let style = style | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        style,
        l.m,
        y,
        l.cw - 2 * l.m,
        edit_h,
        id,
        hinst,
    );
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    let w = wide(&sagethumbs2k_core::clipboard::to_crlf(text));
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
    edit
}

/// The Copy + Close pair every result window ends with, on the row [`result_layout`] computed:
/// Close rightmost and the default button, Copy immediately to its left.
pub(crate) unsafe fn result_buttons(hwnd: HWND, hinst: HINSTANCE, l: &ResultLayout) {
    ctl(
        hwnd,
        BUTTON,
        t("btn_copy"),
        WS_TABSTOP,
        l.copy_x,
        l.btn_y,
        l.btn_w,
        l.btn_h,
        ID_RESULT_COPY,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        l.close_x,
        l.btn_y,
        l.btn_w,
        l.btn_h,
        IDOK,
        hinst,
    );
}

/// A result window whose message handling is entirely the shared kind: dark control colours,
/// then [`result_wndproc`], then the default. Image info, Upload links and OCR are exactly this
/// and register [`result_window_proc`] over their type; Doctor (a font to free on destroy), the
/// Convert report and the upload result (buttons of their own) keep a hand-written procedure
/// around the same shared core.
pub(crate) trait ResultWindow {
    /// Lay the window out; runs on WM_CREATE with the module handle already resolved.
    unsafe fn build(hwnd: HWND, hinst: HINSTANCE);
    /// What the Copy button puts on the clipboard.
    unsafe fn copy_source(hwnd: HWND) -> String;
}

pub(crate) extern "system" fn result_window_proc<W: ResultWindow>(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if let Some(r) = crate::dark::dark_ctlcolor(msg, wparam) {
            return r;
        }
        if let Some(r) = result_wndproc(hwnd, msg, wparam, W::build, W::copy_source) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

pub(crate) unsafe fn result_layout(hwnd: HWND) -> ResultLayout {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let (m, btn_w, btn_h, gap) = (10, 82, 28, 8);
    let (cw, ch) = (rc.right * 96 / dpi, rc.bottom * 96 / dpi);
    let close_x = cw - m - btn_w;
    ResultLayout {
        cw,
        m,
        btn_w,
        btn_h,
        gap,
        btn_y: ch - m - btn_h,
        close_x,
        copy_x: close_x - gap - btn_w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe;

    impl ResultWindow for Probe {
        unsafe fn build(_hwnd: HWND, _hinst: HINSTANCE) {}
        unsafe fn copy_source(_hwnd: HWND) -> String {
            String::new()
        }
    }

    unsafe fn take_quit() -> bool {
        let mut msg = MSG::default();
        PeekMessageW(&mut msg, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool()
    }

    /// Closing a result window opened modally over another must leave the OWNER's message loop
    /// running; closing a top-level one must still end its own. Real windows on this test's
    /// thread, real `DestroyWindow`, and the thread's queue read back for the `WM_QUIT`.
    #[test]
    fn only_a_top_level_result_window_quits_the_pump_when_it_closes() {
        unsafe {
            let hinst: HINSTANCE = GetModuleHandleW(None).expect("module handle").into();
            let class = w!("St2kResultQuitProbe");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(result_window_proc::<Probe>),
                hInstance: hinst,
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc); // a second registration is a harmless no-op
            let make = |owner: Option<HWND>| {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    w!(""),
                    WS_POPUP,
                    0,
                    0,
                    40,
                    40,
                    owner,
                    None,
                    Some(hinst),
                    None,
                )
                .expect("create a hidden probe window")
            };
            let top = make(None);
            let child = make(Some(top));
            while take_quit() {}
            let _ = DestroyWindow(child);
            assert!(!take_quit(), "an owned result window posted WM_QUIT");
            let _ = DestroyWindow(top);
            assert!(
                take_quit(),
                "a top-level result window no longer ends its pump"
            );
        }
    }
}
