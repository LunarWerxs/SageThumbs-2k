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
    ctl, edit_field, get_edit_text, label, read_listfile, run_dialog, t, wide, BUTTON, EM_SETSEL,
    IDCANCEL, IDOK,
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
            // DPI, the deferred close a running move needs, destroy, default.
            _ => crate::win::dialog_tail(hwnd, msg, wparam, lparam, request_close),
        }
    }
}

/// This executable's instance handle: what every `ctl` / `label` / `edit_field` call in a
/// dialog's `WM_CREATE` needs. Shared by this file, `rename_dlg` and `tags_to_folders`.
pub(crate) fn module_instance() -> HINSTANCE {
    // SAFETY: GetModuleHandleW(None) has no preconditions; it answers for the running module.
    unsafe { GetModuleHandleW(None) }.unwrap().into()
}

/// The default OK button (`ok_key`, `ok_w` wide) and the 88-wide Cancel 6 px to its right,
/// both 30 px high, at `(x, y)` of `hwnd` — the button row every `WM_CREATE` in this file,
/// `rename_dlg` and `tags_to_folders` ends with.
pub(crate) unsafe fn ok_cancel_buttons(
    hwnd: HWND,
    hinst: HINSTANCE,
    ok_key: &str,
    x: i32,
    y: i32,
    ok_w: i32,
    cancel_x: i32,
) {
    ctl(
        hwnd,
        BUTTON,
        t(ok_key),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        x,
        y,
        ok_w,
        30,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        cancel_x,
        y,
        88,
        30,
        IDCANCEL,
        hinst,
    );
}

/// `WM_CREATE`: the name edit plus an indeterminate progress bar (hidden until a
/// move is actually running) in the same slot, and the Create/Cancel buttons.
unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst = module_instance();
    let n = F2F_FILES.get().map(|f| f.len()).unwrap_or(0);
    let prompt = f2f_prompt(t("f2f_prompt"), n);
    label(hwnd, hinst, &prompt, 16, 16, 344, 18);
    let edit = edit_field(
        hwnd,
        hinst,
        t("f2f_default_name"),
        16,
        44,
        344,
        26,
        CID_F2F_NAME,
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

    ok_cancel_buttons(hwnd, hinst, "f2f_create", 176, 92, 104, 286);
    LRESULT(0)
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = crate::win::command_id(wparam);
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
    let name = f2f_folder_name(&get_edit_text(hwnd, CID_F2F_NAME), t("f2f_default_name"));
    let Some(files) = F2F_FILES.get().cloned() else {
        return;
    };

    start_batch(
        hwnd,
        &F2F_RUNNING,
        &[CID_F2F_NAME, IDOK],
        CID_F2F_PROGRESS,
        WM_F2F_DONE,
        move || {
            let result = sagethumbs2k_core::files_to_folder(&files, &name);
            *F2F_RESULT.lock().unwrap() = Some(result);
        },
    );
}

/// Latch `running`, disable `disable_ids`, show `progress_id` as an active marquee,
/// then spawn a worker that runs `work` and posts `done_msg` back to `hwnd` for its
/// done handler to pick the result up on the UI thread (issue #29). `work` publishes
/// whatever that handler reads (a static `*_RESULT` slot). Shared by this file and
/// `tags_to_folders`; the callers keep their own `running` re-entrancy guard.
pub(crate) unsafe fn start_batch<F>(
    hwnd: HWND,
    running: &AtomicBool,
    disable_ids: &[i32],
    progress_id: i32,
    done_msg: u32,
    work: F,
) where
    F: FnOnce() + Send + 'static,
{
    for &id in disable_ids {
        if let Ok(ctrl) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(ctrl, false);
        }
    }
    if let Ok(prog) = GetDlgItem(Some(hwnd), progress_id) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(1)), Some(LPARAM(30)));
    }
    running.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        work();
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            done_msg,
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
            let m = wide(&f2f_partial_message(t("f2f_done_partial"), moved, skipped));
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

/// Close the dialog, or defer the close if a move is still running — see
/// [`close_or_defer`] for why a running batch must not be destroyed.
unsafe fn request_close(hwnd: HWND) {
    close_or_defer(hwnd, &F2F_RUNNING);
}

/// Close `hwnd`, or — while `running` says a worker thread still owns the batch — just
/// disable Cancel and leave the teardown to that worker's own done message
/// (`on_f2f_done` / `on_rn_done` / `on_ttf_done`). None of these dialogs has a per-file
/// cancellation checkpoint inside its lib call (it is one call, not a loop the dialog
/// drives), so Cancel while running just refuses to close early: the done message's
/// `DestroyWindow` is the one that actually tears the window down, instead of destroying
/// it out from under the worker thread mid-move. Shared by this file, `rename_dlg` and
/// `tags_to_folders`.
pub(crate) unsafe fn close_or_defer(hwnd: HWND, running: &AtomicBool) {
    if running.load(Ordering::Relaxed) {
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
            let _ = EnableWindow(b, false);
        }
    } else {
        let _ = DestroyWindow(hwnd);
    }
}

/// The prompt line: `template` with its `{n}` placeholder filled by the number of
/// files the DLL passed in (`on_create`).
fn f2f_prompt(template: &str, n: usize) -> String {
    template.replace("{n}", &n.to_string())
}

/// The folder name to create: the user's edit trimmed, or `default` when they left
/// it empty (or typed only whitespace) — `start_move`.
fn f2f_folder_name(typed: &str, default: &str) -> String {
    let name = typed.trim().to_string();
    if name.is_empty() {
        default.to_string()
    } else {
        name
    }
}

/// The partial-move message (some files skipped — issue #27): `template` with its
/// `{moved}` and `{skipped}` placeholders filled in (`on_f2f_done`).
fn f2f_partial_message(template: &str, moved: usize, skipped: usize) -> String {
    template
        .replace("{moved}", &moved.to_string())
        .replace("{skipped}", &skipped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f2f_prompt_fills_the_item_count() {
        let got = f2f_prompt("Move {n} item(s) into a new folder named:", 7);
        assert_eq!(got, "Move 7 item(s) into a new folder named:");
        assert!(!got.contains("{n}"), "the placeholder must be gone: {got}");
    }

    #[test]
    fn f2f_prompt_renders_a_zero_count() {
        assert_eq!(f2f_prompt("{n} files", 0), "0 files");
    }

    #[test]
    fn f2f_folder_name_keeps_a_trimmed_name() {
        assert_eq!(
            f2f_folder_name("  holiday pics \t", "New Folder"),
            "holiday pics"
        );
    }

    #[test]
    fn f2f_folder_name_falls_back_when_nothing_was_typed() {
        assert_eq!(f2f_folder_name("   \n", "New Folder"), "New Folder");
        assert_eq!(f2f_folder_name("", "New Folder"), "New Folder");
    }

    #[test]
    fn f2f_partial_message_fills_both_placeholders() {
        let got = f2f_partial_message("Moved {moved} item(s); {skipped} couldn't be moved.", 3, 2);
        assert_eq!(got, "Moved 3 item(s); 2 couldn't be moved.");
    }

    #[test]
    fn f2f_partial_message_fills_placeholders_in_either_order() {
        assert_eq!(
            f2f_partial_message("{skipped} skipped, {moved} moved", 4, 1),
            "1 skipped, 4 moved"
        );
    }
}
