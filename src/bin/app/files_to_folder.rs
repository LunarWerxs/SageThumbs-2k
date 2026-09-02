//! A name-prompt dialog for the DLL's "Files to folder" verb on a multi-file
//! selection (`--files-to-folder <listfile>`). Single-file selections are handled
//! in the DLL with no prompt. The actual create-folder-and-move lives in the lib
//! (`sagethumbs2k_core::files_to_folder`), shared with the DLL's single-file path.

use core::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{PBM_SETMARQUEE, PBS_MARQUEE};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    ctl, get_edit_text, read_listfile, run_dialog, t, wide, wm_dpichanged, BUTTON, EDIT, EM_SETSEL,
    IDCANCEL, IDOK, STATIC,
};

const CID_F2F_NAME: i32 = 5001;
const CID_F2F_PROGRESS: i32 = 5002;
/// Posted by the worker thread when the create-and-move finishes.
const WM_F2F_DONE: u32 = 0x8000 + 40; // WM_APP + 40

static F2F_FILES: OnceLock<Vec<String>> = OnceLock::new();
/// Set while the worker thread owns the move, so `request_close` can defer
/// destroying the window until it posts `WM_F2F_DONE` (issue #29 — the pump used
/// to freeze for the whole batch, with Windows flagging the window Not
/// Responding; the create-and-move now runs off the UI thread).
static F2F_RUNNING: AtomicBool = AtomicBool::new(false);
/// Set by the worker thread just before it posts `WM_F2F_DONE`; read once, on the
/// UI thread, by `on_f2f_done`.
static F2F_RESULT: Mutex<Option<windows::core::Result<(PathBuf, usize, usize)>>> = Mutex::new(None);

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
            WM_CREATE => on_create(hwnd),
            WM_COMMAND => on_command(hwnd, wparam),
            WM_F2F_DONE => on_f2f_done(hwnd),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            // Mirror IDCANCEL's deferred close: a move started on the worker thread
            // must not be torn out from under it by an unconditional DestroyWindow.
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
}

/// `WM_CREATE`: the name edit plus an indeterminate progress bar (hidden until a
/// move is actually running) in the same slot, and the Create/Cancel buttons.
unsafe fn on_create(hwnd: HWND) -> LRESULT {
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
        16,
        44,
        344,
        26,
        CID_F2F_NAME,
        hinst,
    );
    // Select-all + focus so the suggested name is replaced on first type.
    SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    let _ = SetFocus(Some(edit));

    let prog = ctl(
        hwnd,
        w!("msctls_progress32"),
        "",
        WINDOW_STYLE(PBS_MARQUEE),
        16,
        76,
        344,
        8,
        CID_F2F_PROGRESS,
        hinst,
    );
    let _ = ShowWindow(prog, SW_HIDE);

    ctl(
        hwnd,
        BUTTON,
        t("f2f_create"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        176,
        92,
        104,
        30,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        286,
        92,
        88,
        30,
        IDCANCEL,
        hinst,
    );
    LRESULT(0)
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    match id {
        IDOK => start_move(hwnd),
        IDCANCEL => request_close(hwnd),
        _ => {}
    }
    LRESULT(0)
}

/// `IDOK`: read the folder name and run the create-and-move on a worker thread
/// (issue #29) so the UI pump stays responsive instead of freezing for the whole
/// batch. `on_f2f_done` picks the result back up on the UI thread.
unsafe fn start_move(hwnd: HWND) {
    if F2F_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let mut name = get_edit_text(hwnd, CID_F2F_NAME).trim().to_string();
    if name.is_empty() {
        name = t("f2f_default_name").to_string();
    }
    let Some(files) = F2F_FILES.get().cloned() else {
        return;
    };

    if let Ok(edit) = GetDlgItem(Some(hwnd), CID_F2F_NAME) {
        let _ = EnableWindow(edit, false);
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }
    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_F2F_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(1)), Some(LPARAM(30)));
    }
    F2F_RUNNING.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let result = sagethumbs2k_core::files_to_folder(&files, &name);
        *F2F_RESULT.lock().unwrap() = Some(result);
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_F2F_DONE,
            WPARAM(0),
            LPARAM(0),
        );
    });
}

/// `WM_F2F_DONE`: pick up the worker's result. A clean move closes the dialog; a
/// partial move (some files skipped — issue #27) says so instead of closing as if
/// everything moved; a hard failure (the folder itself couldn't be created)
/// re-enables the fields and keeps the dialog open, same as before this fix.
unsafe fn on_f2f_done(hwnd: HWND) -> LRESULT {
    F2F_RUNNING.store(false, Ordering::Relaxed);
    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_F2F_PROGRESS) {
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(0)), Some(LPARAM(0)));
        let _ = ShowWindow(prog, SW_HIDE);
    }
    let result = F2F_RESULT.lock().unwrap().take();
    let cap = wide("SageThumbs 2K");
    match result {
        Some(Ok((_dir, moved, skipped))) if skipped > 0 => {
            let m = wide(
                &t("f2f_done_partial")
                    .replace("{moved}", &moved.to_string())
                    .replace("{skipped}", &skipped.to_string()),
            );
            MessageBoxW(
                Some(hwnd),
                PCWSTR(m.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
            let _ = DestroyWindow(hwnd);
        }
        Some(Ok(_)) => {
            let _ = DestroyWindow(hwnd);
        }
        Some(Err(_)) | None => {
            // Keep the dialog open on failure (with a message) instead of
            // silently closing as if it worked — the create/move can fail on
            // permissions, a read-only/locked file, or a cross-volume move.
            let m = wide(t("f2f_failed"));
            MessageBoxW(
                Some(hwnd),
                PCWSTR(m.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
            if let Ok(edit) = GetDlgItem(Some(hwnd), CID_F2F_NAME) {
                let _ = EnableWindow(edit, true);
            }
            if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
                let _ = EnableWindow(btn, true);
            }
        }
    }
    LRESULT(0)
}

/// Close the dialog, or defer the close if a move is still running. There is no
/// per-file cancellation checkpoint inside `files_to_folder` (it is one lib call,
/// not a loop this dialog drives) — Cancel while running just refuses to close
/// early, so `on_f2f_done`'s `DestroyWindow` is the one that actually tears the
/// window down, instead of destroying it out from under the worker thread mid-move.
unsafe fn request_close(hwnd: HWND) {
    if F2F_RUNNING.load(Ordering::Relaxed) {
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
            let _ = EnableWindow(b, false);
        }
    } else {
        let _ = DestroyWindow(hwnd);
    }
}
