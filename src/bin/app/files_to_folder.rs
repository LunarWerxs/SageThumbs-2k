//! A name-prompt dialog for the DLL's "Files to folder" verb on a multi-file
//! selection (`--files-to-folder <listfile>`). Single-file selections are handled
//! in the DLL with no prompt. The actual create-folder-and-move lives in the lib
//! (`sagethumbs2k_core::files_to_folder`), shared with the DLL's single-file path.

use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    ctl, get_edit_text, read_listfile, run_dialog, t, wm_dpichanged, BUTTON, EDIT, STATIC,
    EM_SETSEL, IDCANCEL, IDOK,
};

const CID_F2F_NAME: i32 = 5001;
static F2F_FILES: OnceLock<Vec<String>> = OnceLock::new();

pub(crate) unsafe fn run_files_to_folder_dialog(_hinst: HINSTANCE, listfile: &str) {
    let files = read_listfile(listfile);
    if files.is_empty() {
        return;
    }
    let _ = F2F_FILES.set(files);

    run_dialog(
        w!("SageThumbs2KFilesToFolder"),
        Some(f2f_wndproc),
        t("f2f_title"),
        392,
        168,
        None,
    );
}

extern "system" fn f2f_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let n = F2F_FILES.get().map(|f| f.len()).unwrap_or(0);
                let lbl = WINDOW_STYLE(0);
                let prompt = t("f2f_prompt").replace("{n}", &n.to_string());
                ctl(hwnd, STATIC, &prompt, lbl, 16, 16, 344, 18, -1, hinst);
                let edit = ctl(
                    hwnd,
                    EDIT,
                    t("f2f_default_name"),
                    WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
                    16, 44, 344, 26, CID_F2F_NAME, hinst,
                );
                // Select-all + focus so the suggested name is replaced on first type.
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                let _ = SetFocus(Some(edit));
                ctl(hwnd, BUTTON, t("f2f_create"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 176, 92, 104, 30, IDOK, hinst);
                ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 286, 92, 88, 30, IDCANCEL, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    IDOK => {
                        let mut name = get_edit_text(hwnd, CID_F2F_NAME).trim().to_string();
                        if name.is_empty() {
                            name = t("f2f_default_name").to_string();
                        }
                        if let Some(files) = F2F_FILES.get() {
                            let _ = sagethumbs2k_core::files_to_folder(files, &name);
                        }
                        let _ = DestroyWindow(hwnd);
                    }
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
