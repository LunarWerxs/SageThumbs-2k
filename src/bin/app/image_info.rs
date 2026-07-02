//! The "Image info" window — a verbose, copyable metadata dump for the right-click
//! Tools verb. Launched standalone via `SageThumbs2K.exe --image-info <path>`: a
//! scrollable read-only edit with every file/image/EXIF field, plus a Copy button.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, gui_font, run_dialog, set_clipboard_text, wide, BUTTON, EDIT, IDCANCEL, IDOK};

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
        run_dialog(w!("SageThumbs2KImageInfo"), Some(info_wndproc), "Image info", 480, 470, None);
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Read-only, word-wrapped, vertically scrollable — the verbose dump can be long.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(hwnd, EDIT, "", edit_style, 10, 10, 460, 410, ID_EDIT, hinst);
    // Edit controls want CRLF line breaks (a lone LF renders as a box).
    let text = INFO.with(|i| i.borrow().replace('\n', "\r\n"));
    let w = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
    let f = gui_font();
    SendMessageW(edit, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));

    ctl(hwnd, BUTTON, "Copy", WS_TABSTOP, 300, 430, 80, 28, ID_COPY, hinst);
    ctl(
        hwnd,
        BUTTON,
        "Close",
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        390,
        430,
        80,
        28,
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
