//! The DLL's "Sort into folders ▸ By audio tag" verb on an audio selection
//! (`--tags-to-folders <listfile>`). Dialog: destination, a `$artist - $album`
//! folder-name template, and copy-vs-move. The sort engine is in the lib
//! (`sagethumbs2k_core::tags_to_folders`).

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{PBM_SETMARQUEE, PBS_MARQUEE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    checked, ctl, get_edit_text, pick_folder, read_listfile, run_dialog, set_edit_text, t, wide,
    wm_dpichanged, BM_SETCHECK_MSG, BUTTON, EDIT, IDCANCEL, IDOK, STATIC,
};

const CID_TTF_DEST: i32 = 5101;
const CID_TTF_BROWSE: i32 = 5102;
const CID_TTF_TEMPLATE: i32 = 5103;
const CID_TTF_MISSING: i32 = 5104;
const CID_TTF_MOVE: i32 = 5105;
const CID_TTF_COPY: i32 = 5106;
const CID_TTF_PROGRESS: i32 = 5107;
/// Posted by the worker thread when the sort finishes.
const WM_TTF_DONE: u32 = 0x8000 + 41; // WM_APP + 41

static TTF_FILES: OnceLock<Vec<String>> = OnceLock::new();
/// Set while the worker thread owns the sort, so `request_close` can defer
/// destroying the window until it posts `WM_TTF_DONE` (issue #29 — the pump used
/// to freeze for the whole batch, with Windows flagging the window Not
/// Responding; the sort now runs off the UI thread).
static TTF_RUNNING: AtomicBool = AtomicBool::new(false);
/// (done, skipped, move_files), set by the worker thread just before it posts
/// `WM_TTF_DONE`; read once, on the UI thread, by `on_ttf_done`.
static TTF_RESULT: Mutex<Option<(usize, usize, bool)>> = Mutex::new(None);

pub(crate) unsafe fn run_tags_to_folders_dialog(_hinst: HINSTANCE, listfile: &str) {
    let files = read_listfile(listfile);
    if files.is_empty() {
        return;
    }
    let _ = TTF_FILES.set(files);

    run_dialog(
        w!("SageThumbs2KTagsToFolders"),
        Some(ttf_wndproc),
        t("ttf_title"),
        452,
        270,
        None,
    );
}

extern "system" fn ttf_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_create(hwnd),
            WM_COMMAND => on_command(hwnd, wparam),
            WM_TTF_DONE => on_ttf_done(hwnd),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            // Mirror IDCANCEL's deferred close: a sort started on the worker
            // thread must not be torn out from under it by an unconditional
            // DestroyWindow.
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

/// `WM_CREATE`: lay out the destination/template/missing-token edits, the move/copy radio
/// pair, and the sort/cancel button row anchored to the real client bottom.
unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let lbl = WINDOW_STYLE(0);
    // Default destination = the first file's folder.
    let default_dest = TTF_FILES
        .get()
        .and_then(|f| f.first())
        .and_then(|p| std::path::Path::new(p).parent())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    ctl(
        hwnd,
        STATIC,
        t("ttf_destination"),
        lbl,
        16,
        18,
        90,
        18,
        -1,
        hinst,
    );
    let dest = ctl(
        hwnd,
        EDIT,
        &default_dest,
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        110,
        16,
        268,
        24,
        CID_TTF_DEST,
        hinst,
    );
    let _ = dest;
    ctl(
        hwnd,
        BUTTON,
        "\u{2026}",
        WS_TABSTOP,
        384,
        15,
        44,
        26,
        CID_TTF_BROWSE,
        hinst,
    );

    ctl(
        hwnd,
        STATIC,
        t("ttf_template"),
        lbl,
        16,
        56,
        90,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        t("ttf_template_default"),
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        110,
        54,
        318,
        24,
        CID_TTF_TEMPLATE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("ttf_tokens"),
        lbl,
        110,
        82,
        318,
        16,
        -1,
        hinst,
    );

    ctl(
        hwnd,
        STATIC,
        t("ttf_missing"),
        lbl,
        16,
        112,
        90,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        t("ttf_missing_default"),
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        110,
        110,
        160,
        24,
        CID_TTF_MISSING,
        hinst,
    );

    let mv = ctl(
        hwnd,
        BUTTON,
        t("ttf_move"),
        WINDOW_STYLE(BS_AUTORADIOBUTTON as u32) | WS_GROUP | WS_TABSTOP,
        110,
        146,
        110,
        22,
        CID_TTF_MOVE,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("ttf_copy"),
        WINDOW_STYLE(BS_AUTORADIOBUTTON as u32) | WS_TABSTOP,
        230,
        146,
        110,
        22,
        CID_TTF_COPY,
        hinst,
    );
    SendMessageW(mv, BM_SETCHECK_MSG, Some(WPARAM(1)), Some(LPARAM(0))); // default: Move

    // Indeterminate progress bar, hidden until a sort is actually running (issue
    // #29): the sort runs on a worker thread so this pump keeps pumping instead
    // of Windows flagging the window Not Responding.
    let prog = ctl(
        hwnd,
        w!("msctls_progress32"),
        "",
        WINDOW_STYLE(PBS_MARQUEE),
        16,
        180,
        414,
        8,
        CID_TTF_PROGRESS,
        hinst,
    );
    let _ = ShowWindow(prog, SW_HIDE);

    // Anchor the button row to the REAL client bottom — `run_dialog`'s h is
    // the TOTAL window height, so a hardcoded y put the row's bottom below
    // the client edge (clipped). GetClientRect is physical px → back to
    // 96-DPI design px, because `ctl` re-scales design px to DPI.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let by = rc.bottom * 96 / dpi - 12 - 30;
    ctl(
        hwnd,
        BUTTON,
        t("ttf_sort"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        244,
        by,
        92,
        30,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        342,
        by,
        88,
        30,
        IDCANCEL,
        hinst,
    );
    LRESULT(0)
}

/// `WM_COMMAND`: dispatch by control/menu id.
unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    match id {
        CID_TTF_BROWSE => {
            if let Some(dir) = pick_folder(hwnd) {
                set_edit_text(hwnd, CID_TTF_DEST, &dir);
            }
        }
        IDOK => on_command_ok(hwnd),
        IDCANCEL => request_close(hwnd),
        _ => {}
    }
    LRESULT(0)
}

/// `IDOK`: read the destination/template/missing-token fields (falling back to
/// their defaults when blank), then run the sort on a worker thread (issue #29)
/// so the UI pump stays responsive instead of freezing for the whole batch.
/// `on_ttf_done` picks the done/skipped counts back up on the UI thread.
unsafe fn on_command_ok(hwnd: HWND) {
    if TTF_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let mut dest = get_edit_text(hwnd, CID_TTF_DEST).trim().to_string();
    if dest.is_empty() {
        dest = TTF_FILES
            .get()
            .and_then(|f| f.first())
            .and_then(|p| std::path::Path::new(p).parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
    }
    let mut template = get_edit_text(hwnd, CID_TTF_TEMPLATE).trim().to_string();
    if template.is_empty() {
        template = t("ttf_template_default").to_string();
    }
    let mut missing = get_edit_text(hwnd, CID_TTF_MISSING).trim().to_string();
    if missing.is_empty() {
        missing = t("ttf_missing_default").to_string();
    }
    let move_files = checked(hwnd, CID_TTF_MOVE);
    let Some(files) = TTF_FILES.get().cloned() else {
        return;
    };

    for id in [
        CID_TTF_DEST,
        CID_TTF_BROWSE,
        CID_TTF_TEMPLATE,
        CID_TTF_MISSING,
        CID_TTF_MOVE,
        CID_TTF_COPY,
        IDOK,
    ] {
        if let Ok(ctrl) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(ctrl, false);
        }
    }
    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_TTF_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(1)), Some(LPARAM(30)));
    }
    TTF_RUNNING.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let (done, skipped) = sagethumbs2k_core::tags_to_folders(
            &files,
            std::path::Path::new(&dest),
            &template,
            &missing,
            move_files,
        );
        *TTF_RESULT.lock().unwrap() = Some((done, skipped, move_files));
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_TTF_DONE,
            WPARAM(0),
            LPARAM(0),
        );
    });
}

/// `WM_TTF_DONE`: report the done/skipped counts the worker thread produced, then close.
unsafe fn on_ttf_done(hwnd: HWND) -> LRESULT {
    TTF_RUNNING.store(false, Ordering::Relaxed);
    if let Some((done, skipped, move_files)) = TTF_RESULT.lock().unwrap().take() {
        let key = if move_files {
            "ttf_done_moved"
        } else {
            "ttf_done_copied"
        };
        let m = wide(
            &t(key)
                .replace("{done}", &done.to_string())
                .replace("{skipped}", &skipped.to_string()),
        );
        let cap = wide("SageThumbs 2K");
        MessageBoxW(
            Some(hwnd),
            PCWSTR(m.as_ptr()),
            PCWSTR(cap.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
    let _ = DestroyWindow(hwnd);
    LRESULT(0)
}

/// Close the dialog, or defer the close if a sort is still running. There is no
/// per-file cancellation checkpoint inside `tags_to_folders` (it is one lib call,
/// not a loop this dialog drives) — Cancel while running just refuses to close
/// early, so `on_ttf_done`'s `DestroyWindow` is the one that actually tears the
/// window down, instead of destroying it out from under the worker thread mid-sort.
unsafe fn request_close(hwnd: HWND) {
    if TTF_RUNNING.load(Ordering::Relaxed) {
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
            let _ = EnableWindow(b, false);
        }
    } else {
        let _ = DestroyWindow(hwnd);
    }
}
