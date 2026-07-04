//! The "Image info" window — a verbose, copyable metadata dump for the right-click
//! Tools verb. Launched standalone via `SageThumbs2K.exe --image-info <path>`: a
//! scrollable read-only edit with every file/image/EXIF field, plus a Copy button.

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
    /// The metadata text to show — set just before `run_dialog`, read in WM_CREATE.
    static INFO: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Gather verbose metadata for `path` and show it in a scrollable, copyable window.
pub fn run_image_info(path: &str) {
    let text = sagethumbs2k_core::read_info_verbose(path);
    INFO.with(|i| *i.borrow_mut() = text);
    unsafe {
        // Title reuses the context-menu verb's key — same phrase, already translated
        // in every shipped locale.
        run_dialog(
            w!("SageThumbs2KImageInfo"),
            Some(info_wndproc),
            t("menu_image_info"),
            480,
            470,
            None,
        );
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area (in design px). `run_dialog`'s w/h size the whole
    // WINDOW, so the client is narrower/shorter by the frame + caption; hardcoding the design
    // coords clipped the Copy/Close row below the client bottom. GetClientRect is physical
    // px → divide back to 96-DPI design px, because `ctl` re-scales design px to DPI.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let cw = rc.right * 96 / dpi; // client width, design px
    let ch = rc.bottom * 96 / dpi; // client height, design px
    let m = 10;
    let (btn_w, btn_h, gap) = (82, 28, 8);
    let btn_y = ch - m - btn_h;
    let edit_h = (btn_y - gap - m).max(48);

    // Read-only, word-wrapped, vertically scrollable — the verbose dump can be long.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(hwnd, EDIT, "", edit_style, m, m, cw - 2 * m, edit_h, ID_EDIT, hinst);
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar. Re-theme
    // the edit to DarkMode_Explorer so its scrollbar renders dark (the edit bg/text stay dark
    // via WM_CTLCOLOREDIT in `dark_ctlcolor`).
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    // Edit controls want CRLF line breaks (a lone LF renders as a box).
    let text = INFO.with(|i| i.borrow().replace('\n', "\r\n"));
    let w = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
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

extern "system" fn info_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
                    ID_COPY => INFO.with(|i| {
                        let _ = set_clipboard_text(&i.borrow());
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
