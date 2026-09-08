//! The DLL's "Rename with pattern…" verb (`--rename-with-pattern <listfile>`): a
//! free-form template + find/replace over the selected files, with a live "old →
//! new" preview that recomputes on every keystroke. Mirrors `files_to_folder.rs`'s
//! structure exactly (statics, worker-thread apply, deferred close) — see that
//! file's doc comment for the shape this one repeats. The engine itself
//! (`expand_pattern` / `rename_by_pattern` / `rename_pattern_preview`) lives in the
//! lib (`sagethumbs2k_core`, `verbs/actions/rename.rs`), shared between this live
//! preview and the actual rename so the two can never disagree.

use core::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{SetBkColor, SetBkMode, SetTextColor, HDC, TRANSPARENT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{PBM_SETMARQUEE, PBS_MARQUEE};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::settings;

use crate::dark::{dark_bg_brush, dark_ctlcolor, DARK_BG};
use crate::win::{
    ctl, get_edit_text, read_listfile, run_dialog, set_edit_text, t, wide, wm_dpichanged, BUTTON,
    EDIT, EM_SETSEL, IDCANCEL, IDOK, STATIC,
};

const CID_RN_PATTERN: i32 = 5201;
const CID_RN_FIND: i32 = 5202;
const CID_RN_REPLACE: i32 = 5203;
const CID_RN_PREVIEW: i32 = 5204;
/// Error text, drawn in the SAME rect as the preview listbox — only one of the two
/// is ever visible (see `rebuild_preview`), so this doesn't cost extra layout.
const CID_RN_ERROR: i32 = 5205;
const CID_RN_PROGRESS: i32 = 5206;
const CID_RN_HINT: i32 = 5207;

const DLG_W: i32 = 460;
const DLG_H: i32 = 404;

/// Posted by the worker thread when the rename pass finishes.
const WM_RN_DONE: u32 = 0x8000 + 42; // WM_APP + 42

/// The HKCU value that persists the last pattern the user typed, restored the next
/// time the dialog opens. Find/replace are deliberately NOT persisted (they're
/// usually a one-off for this batch, unlike the pattern).
const SETTING_LAST_PATTERN: &str = "RnLastPattern";

static RN_FILES: OnceLock<Vec<String>> = OnceLock::new();
/// Set while the worker thread owns the rename pass, so `request_close` can defer
/// destroying the window until it posts `WM_RN_DONE` — same reasoning as
/// `files_to_folder.rs`'s `F2F_RUNNING` (the pump must not freeze for the whole
/// batch, and the window must not be torn down out from under the worker thread).
static RN_RUNNING: AtomicBool = AtomicBool::new(false);
/// (attempted, done, note), set by the worker thread just before it posts
/// `WM_RN_DONE`; read once, on the UI thread, by `on_rn_done`. Mirrors `ActionReport`'s
/// three fields rather than storing the type itself — the type isn't re-exported past
/// the lib's own `verbs` facade, only the function that returns it is.
static RN_RESULT: Mutex<Option<(usize, usize, Option<String>)>> = Mutex::new(None);

pub(crate) unsafe fn run_rename_with_pattern_dialog(_hinst: HINSTANCE, listfile: &str) {
    let files = read_listfile(listfile);
    if files.is_empty() {
        return;
    }
    let n = files.len();
    let _ = RN_FILES.set(files);

    let title = t("rn_title").replace("{n}", &n.to_string());
    run_dialog(
        w!("SageThumbs2KRenamePattern"),
        Some(rn_wndproc),
        &title,
        DLG_W,
        DLG_H,
        None,
    );
}

extern "system" fn rn_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // The error label draws in red — decided before the generic dark-mode
        // static/edit theming below gets a chance to paint it the ordinary text
        // colour, same technique `settings_dlg::mod.rs` uses for its status lines.
        if msg == WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), CID_RN_ERROR).is_ok_and(|s| s.0 as isize == lparam.0)
        {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(0x004D_48E5)); // red — same tone the Settings status lines use
            SetBkColor(hdc, DARK_BG());
            SetBkMode(hdc, TRANSPARENT);
            return LRESULT(dark_bg_brush().0 as isize);
        }
        if msg == WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), CID_RN_HINT).is_ok_and(|s| s.0 as isize == lparam.0)
        {
            return crate::dark::dark_ctlcolor_dim(wparam);
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_create(hwnd),
            WM_COMMAND => on_command(hwnd, wparam),
            WM_RN_DONE => on_rn_done(hwnd),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            // Same deferred-close shape as `files_to_folder.rs`: a rename started on
            // the worker thread must not be torn out from under it.
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

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let lbl = WINDOW_STYLE(0);

    let last_pattern = settings::get_string_opt(SETTING_LAST_PATTERN)
        .unwrap_or_else(|| t("rn_pattern_default").to_string());

    ctl(
        hwnd,
        STATIC,
        t("rn_pattern_label"),
        lbl,
        16,
        16,
        300,
        18,
        -1,
        hinst,
    );
    let pattern_edit = ctl(
        hwnd,
        EDIT,
        &last_pattern,
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        16,
        36,
        428,
        24,
        CID_RN_PATTERN,
        hinst,
    );
    SendMessageW(pattern_edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    let _ = SetFocus(Some(pattern_edit));

    ctl(
        hwnd,
        STATIC,
        t("rn_pattern_hint"),
        lbl,
        16,
        64,
        428,
        16,
        CID_RN_HINT,
        hinst,
    );

    ctl(
        hwnd,
        STATIC,
        t("rn_find_label"),
        lbl,
        16,
        88,
        206,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("rn_replace_label"),
        lbl,
        238,
        88,
        206,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        16,
        106,
        206,
        24,
        CID_RN_FIND,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        238,
        106,
        206,
        24,
        CID_RN_REPLACE,
        hinst,
    );

    ctl(
        hwnd,
        STATIC,
        t("rn_preview_label"),
        lbl,
        16,
        138,
        300,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        w!("LISTBOX"),
        "",
        WINDOW_STYLE(LBS_NOTIFY as u32 | LBS_HASSTRINGS as u32)
            | WS_BORDER
            | WS_VSCROLL
            | WS_TABSTOP,
        16,
        158,
        428,
        176,
        CID_RN_PREVIEW,
        hinst,
    );
    // Same rect as the listbox above — only one of the two is ever shown (see
    // `rebuild_preview`), so an invalid pattern replaces the list in place rather
    // than growing the dialog.
    ctl(
        hwnd,
        STATIC,
        "",
        WS_BORDER, // SS_LEFT is 0: a plain left-aligned static needs no style bit
        16,
        158,
        428,
        176,
        CID_RN_ERROR,
        hinst,
    );
    if let Ok(err_ctl) = GetDlgItem(Some(hwnd), CID_RN_ERROR) {
        let _ = ShowWindow(err_ctl, SW_HIDE);
    }

    let prog = ctl(
        hwnd,
        w!("msctls_progress32"),
        "",
        WINDOW_STYLE(PBS_MARQUEE),
        16,
        342,
        428,
        8,
        CID_RN_PROGRESS,
        hinst,
    );
    let _ = ShowWindow(prog, SW_HIDE);

    ctl(
        hwnd,
        BUTTON,
        t("rn_rename_btn"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        260,
        360,
        90,
        30,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        356,
        360,
        88,
        30,
        IDCANCEL,
        hinst,
    );

    rebuild_preview(hwnd);
    LRESULT(0)
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
    match id {
        IDOK => start_rename(hwnd),
        IDCANCEL => request_close(hwnd),
        CID_RN_PATTERN | CID_RN_FIND | CID_RN_REPLACE if notify == EN_CHANGE => {
            rebuild_preview(hwnd);
        }
        _ => {}
    }
    LRESULT(0)
}

/// The live preview shows at most this many rows (the batch itself is unbounded).
const PREVIEW_ROWS: usize = 50;

/// Recompute the "old → new" list (first ~50 files) for whatever's in the
/// pattern/find/replace boxes right now — called on every keystroke. Uses
/// `rename_pattern_preview`, the EXACT function `start_rename`'s worker thread's
/// `rename_by_pattern` builds each real target name through (via `pattern_stem`), so
/// what's shown here is never a promise the apply step can break. Checks the pattern
/// against EVERY selected file, not just the shown 50 — a pattern that only breaks on
/// file 51 must still block OK rather than silently skip/error it later.
unsafe fn rebuild_preview(hwnd: HWND) {
    let Some(files) = RN_FILES.get() else {
        return;
    };
    let pattern = get_edit_text(hwnd, CID_RN_PATTERN);
    let find = get_edit_text(hwnd, CID_RN_FIND);
    let replace = get_edit_text(hwnd, CID_RN_REPLACE);

    let mut rows: Vec<String> = Vec::new();
    let mut error: Option<String> = None;
    for (i, p) in files.iter().enumerate() {
        let preview =
            sagethumbs2k_core::rename_pattern_preview(p, (i + 1) as u32, &pattern, &find, &replace);
        match preview {
            Ok(new_name) => {
                if rows.len() < PREVIEW_ROWS {
                    let old_name = Path::new(p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(p.as_str());
                    rows.push(format!("{old_name}  \u{2192}  {new_name}"));
                }
            }
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    let valid = error.is_none();

    set_edit_text(hwnd, CID_RN_ERROR, error.as_deref().unwrap_or(""));
    if let Ok(err_ctl) = GetDlgItem(Some(hwnd), CID_RN_ERROR) {
        let _ = ShowWindow(err_ctl, if valid { SW_HIDE } else { SW_SHOW });
    }
    if let Ok(list) = GetDlgItem(Some(hwnd), CID_RN_PREVIEW) {
        let _ = ShowWindow(list, if valid { SW_SHOW } else { SW_HIDE });
        if valid {
            SendMessageW(list, LB_RESETCONTENT, None, None);
            for row in &rows {
                let w = wide(row);
                SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
            }
            if files.len() > rows.len() {
                let hidden = (files.len() - rows.len()).to_string();
                let more = t("rn_preview_more").replace("{n}", &hidden);
                let w = wide(&more);
                SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
            }
        }
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, valid);
    }
}

/// `IDOK`: persist the pattern, then run the real rename on a worker thread (same
/// reasoning as `files_to_folder.rs::start_move` — keep the UI pump responsive for
/// the whole batch instead of freezing it). `on_rn_done` picks the result back up.
unsafe fn start_rename(hwnd: HWND) {
    if RN_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let Some(files) = RN_FILES.get().cloned() else {
        return;
    };
    let pattern = get_edit_text(hwnd, CID_RN_PATTERN);
    let find = get_edit_text(hwnd, CID_RN_FIND);
    let replace = get_edit_text(hwnd, CID_RN_REPLACE);
    let _ = settings::set_string(SETTING_LAST_PATTERN, &pattern);

    for id in [CID_RN_PATTERN, CID_RN_FIND, CID_RN_REPLACE] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(c, false);
        }
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }
    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_RN_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(1)), Some(LPARAM(30)));
    }
    RN_RUNNING.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let r = sagethumbs2k_core::rename_by_pattern(&files, &pattern, &find, &replace);
        *RN_RESULT.lock().unwrap() = Some((r.attempted, r.done, r.note.clone()));
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_RN_DONE,
            WPARAM(0),
            LPARAM(0),
        );
    });
}

/// `WM_RN_DONE`: pick up the worker's (attempted, done, note) — a clean pass closes
/// the dialog silently; a partial one (some files errored — locked, name clash after
/// the preview's own check, …) reports it first, same shape as
/// `files_to_folder.rs::on_f2f_done`'s partial-move message.
unsafe fn on_rn_done(hwnd: HWND) -> LRESULT {
    RN_RUNNING.store(false, Ordering::Relaxed);
    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_RN_PROGRESS) {
        SendMessageW(prog, PBM_SETMARQUEE, Some(WPARAM(0)), Some(LPARAM(0)));
        let _ = ShowWindow(prog, SW_HIDE);
    }
    let result = RN_RESULT.lock().unwrap().take();
    if let Some((attempted, done, _note)) = result {
        let errored = attempted.saturating_sub(done);
        if errored > 0 {
            let m = wide(
                &t("rn_done_partial")
                    .replace("{done}", &done.to_string())
                    .replace("{errored}", &errored.to_string()),
            );
            let cap = wide("SageThumbs 2K");
            MessageBoxW(
                Some(hwnd),
                PCWSTR(m.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
    }
    let _ = DestroyWindow(hwnd);
    LRESULT(0)
}

/// Close the dialog, or defer the close if a rename is still running — same
/// reasoning as `files_to_folder.rs::request_close`.
unsafe fn request_close(hwnd: HWND) {
    if RN_RUNNING.load(Ordering::Relaxed) {
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
            let _ = EnableWindow(b, false);
        }
    } else {
        let _ = DestroyWindow(hwnd);
    }
}
