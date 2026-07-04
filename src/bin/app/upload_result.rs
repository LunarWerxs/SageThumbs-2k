//! The upload-result window — shows the uploaded link(s) in a selectable, read-only
//! edit with a **Copy** button (copies every link to the clipboard) and Close. Used by
//! the right-click "Upload" verb (`--upload-keep`, one line per image) and the
//! screenshot Upload button (`--upload`, a single link). The links are already on the
//! clipboard when this opens; Copy re-copies them (handy if the clipboard changed since,
//! or to grab them again after picking one out of the list). Modeled on `image_info.rs`.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    ctl, gui_font, run_dialog, set_clipboard_text, t, wide, BUTTON, EDIT, IDCANCEL, IDOK,
};

const ID_EDIT: i32 = 100;
const ID_COPY: i32 = 101;

thread_local! {
    /// (heading line, links joined by CRLF) — set before `run_dialog`, read in WM_CREATE.
    /// The edit shows the heading + the links; the Copy button copies ONLY the links.
    static RESULT: RefCell<(String, String)> =
        const { RefCell::new((String::new(), String::new())) };
}

/// Show the uploaded `links` (CRLF-separated — one per image) under `heading`, with a
/// Copy button that (re-)copies just the links to the clipboard.
pub fn show_upload_result(heading: &str, links: &str) {
    RESULT.with(|r| *r.borrow_mut() = (heading.to_string(), links.to_string()));
    unsafe {
        // `run_dialog`'s w/h are the TOTAL window size (no client adjustment), so the
        // client is ~30 design-px shorter than `h`. Size generously and keep the buttons
        // well inside the client — a too-short window clips the Copy/Close row.
        run_dialog(
            w!("SageThumbs2KUploadResult"),
            Some(result_wndproc),
            t("up_caption_file"),
            460,
            300,
            None,
        );
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area (in design px). `run_dialog`'s w/h size the whole
    // WINDOW, so the client is narrower/shorter by the frame + caption; hardcoding the design
    // width clipped the edit's scrollbar AND the Close button off the right. GetClientRect is
    // physical px → divide back to 96-DPI design px, because `ctl` re-scales design px to DPI.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let cw = rc.right * 96 / dpi; // client width, design px
    let ch = rc.bottom * 96 / dpi; // client height, design px
    let m = 10;
    let (btn_w, btn_h, gap) = (82, 28, 8);
    let btn_y = ch - m - btn_h;
    let edit_h = (btn_y - gap - m).max(48);

    // Read-only, selectable, vertically scrollable — a multi-image upload can list many links.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(hwnd, EDIT, "", edit_style, m, m, cw - 2 * m, edit_h, ID_EDIT, hinst);
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar. Re-theme
    // the edit to DarkMode_Explorer so its scrollbar renders dark (the edit bg/text stay dark
    // via WM_CTLCOLOREDIT in `dark_ctlcolor`).
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    let text = RESULT.with(|r| {
        let (h, l) = &*r.borrow();
        // Edit controls want CRLF; the links are already CRLF-joined, the heading may not be.
        format!("{}\r\n\r\n{}", h.replace('\n', "\r\n"), l)
    });
    let wtext = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(wtext.as_ptr()));
    let f = gui_font();
    SendMessageW(edit, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));

    // Buttons bottom-right, inside the client (Close rightmost, Copy to its left).
    let close_x = cw - m - btn_w;
    let copy_x = close_x - gap - btn_w;
    ctl(hwnd, BUTTON, t("btn_copy"), WS_TABSTOP, copy_x, btn_y, btn_w, btn_h, ID_COPY, hinst);
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        close_x,
        btn_y,
        btn_w,
        btn_h,
        IDOK,
        hinst,
    );
}

extern "system" fn result_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    ID_COPY => RESULT.with(|r| {
                        let _ = set_clipboard_text(&r.borrow().1);
                    }),
                    IDOK | IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0); // let run_dialog's pump_until_quit exit
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
