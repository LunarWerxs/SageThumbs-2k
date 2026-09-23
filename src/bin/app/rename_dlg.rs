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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::{PBM_SETMARQUEE, PBS_MARQUEE};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use st2k_base::settings;

use crate::dark::dark_ctlcolor;
use crate::win::{
    ctl, edit_field, get_edit_text, label, read_listfile, run_dialog, set_edit_text, t, wide,
    EM_SETSEL, IDCANCEL, IDOK, STATIC,
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
/// Posted by a preview worker when its rows are ready (`RN_PREVIEW` holds them).
const WM_RN_PREVIEW: u32 = 0x8000 + 43; // WM_APP + 43

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

/// The live preview is computed on a worker thread, one per keystroke, and only the newest
/// one's answer is shown: every `rebuild_preview` bumps this generation, a worker that
/// finishes behind a newer one drops its rows, and a worker that notices it has been
/// superseded stops walking early. Until 2026-09-19 the walk ran on the UI thread, every
/// selected file on every keystroke (audit concern 2); a `{w}` pattern over a folder of
/// PSDs froze the dialog for the length of the decodes.
static RN_PREVIEW_GEN: AtomicU32 = AtomicU32::new(0);
/// A finished preview: its generation, the rows to show, and the first pattern error.
struct PreviewResult {
    gen: u32,
    rows: Vec<String>,
    error: Option<String>,
}
/// The newest finished preview. Read once by `on_rn_preview`.
static RN_PREVIEW: Mutex<Option<PreviewResult>> = Mutex::new(None);

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
            // The same red the Settings status lines use.
            return crate::dark::dark_ctlcolor_tinted(wparam, crate::dark::STATUS_RED);
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
            WM_RN_PREVIEW => on_rn_preview(hwnd),
            // DPI, the deferred close a running rename needs, destroy, default.
            _ => crate::win::dialog_tail(hwnd, msg, wparam, lparam, request_close),
        }
    }
}

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst = crate::files_to_folder::module_instance();
    let lbl = WINDOW_STYLE(0);

    let last_pattern = settings::get_string_opt(SETTING_LAST_PATTERN)
        .unwrap_or_else(|| t("rn_pattern_default").to_string());

    label(hwnd, hinst, t("rn_pattern_label"), 16, 16, 300, 18);
    let pattern_edit = edit_field(hwnd, hinst, &last_pattern, 16, 36, 428, 24, CID_RN_PATTERN);
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

    label(hwnd, hinst, t("rn_find_label"), 16, 88, 206, 18);
    label(hwnd, hinst, t("rn_replace_label"), 238, 88, 206, 18);
    edit_field(hwnd, hinst, "", 16, 106, 206, 24, CID_RN_FIND);
    edit_field(hwnd, hinst, "", 238, 106, 206, 24, CID_RN_REPLACE);

    label(hwnd, hinst, t("rn_preview_label"), 16, 138, 300, 18);
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

    crate::files_to_folder::ok_cancel_buttons(hwnd, hinst, "rn_rename_btn", 260, 360, 90, 356);

    rebuild_preview(hwnd);
    LRESULT(0)
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let (id, notify) = crate::win::command_parts(wparam);
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

    // Nothing can be applied until the newest preview has checked every file.
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }
    let gen = RN_PREVIEW_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let superseded = || RN_PREVIEW_GEN.load(Ordering::Acquire) != gen;
        let Some((rows, error)) = compute_preview(files, &pattern, &find, &replace, superseded)
        else {
            return; // a newer keystroke owns the preview now
        };
        *RN_PREVIEW.lock().unwrap() = Some(PreviewResult { gen, rows, error });
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_RN_PREVIEW,
            WPARAM(0),
            LPARAM(0),
        );
    });
}

/// The preview's rows ("old → new", first [`PREVIEW_ROWS`]) and the first pattern error, for
/// `files` under `pattern`/`find`/`replace`. Checks EVERY file, not just the shown rows.
/// `superseded` is polled between files; `None` when it says a newer preview has taken
/// over, so a long walk is abandoned rather than finished for nobody.
fn compute_preview(
    files: &[String],
    pattern: &str,
    find: &str,
    replace: &str,
    superseded: impl Fn() -> bool,
) -> Option<(Vec<String>, Option<String>)> {
    let mut rows: Vec<String> = Vec::new();
    let mut error: Option<String> = None;
    for (i, p) in files.iter().enumerate() {
        if superseded() {
            return None;
        }
        let preview =
            sagethumbs2k_core::rename_pattern_preview(p, (i + 1) as u32, pattern, find, replace);
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
    Some((rows, error))
}

/// `WM_RN_PREVIEW`: show the newest finished preview, ignoring one a later keystroke has
/// already outdated.
unsafe fn on_rn_preview(hwnd: HWND) -> LRESULT {
    let Some(files) = RN_FILES.get() else {
        return LRESULT(0);
    };
    let Some(PreviewResult { gen, rows, error }) = RN_PREVIEW.lock().unwrap().take() else {
        return LRESULT(0);
    };
    if gen != RN_PREVIEW_GEN.load(Ordering::Acquire) {
        return LRESULT(0);
    }
    let valid = error.is_none();

    set_edit_text(hwnd, CID_RN_ERROR, error.as_deref().unwrap_or(""));
    if let Ok(err_ctl) = GetDlgItem(Some(hwnd), CID_RN_ERROR) {
        let _ = ShowWindow(err_ctl, if valid { SW_HIDE } else { SW_SHOW });
    }
    apply_preview_list(hwnd, valid, &rows, files.len());
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, valid && !RN_RUNNING.load(Ordering::Relaxed));
    }
    LRESULT(0)
}

/// Show or hide the preview listbox and, when `valid`, fill it with the precomputed
/// `rows` plus a trailing "…N more" row when `file_total` exceeds them.
unsafe fn apply_preview_list(hwnd: HWND, valid: bool, rows: &[String], file_total: usize) {
    let Ok(list) = GetDlgItem(Some(hwnd), CID_RN_PREVIEW) else {
        return;
    };
    let _ = ShowWindow(list, if valid { SW_SHOW } else { SW_HIDE });
    if !valid {
        return;
    }
    SendMessageW(list, LB_RESETCONTENT, None, None);
    for row in rows {
        let w = wide(row);
        SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    if file_total > rows.len() {
        let hidden = (file_total - rows.len()).to_string();
        let more = t("rn_preview_more").replace("{n}", &hidden);
        let w = wide(&more);
        SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
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
    crate::files_to_folder::close_or_defer(hwnd, &RN_RUNNING);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker's half of the live preview (audit concern 2, 2026-09-19): rows for the
    /// first `PREVIEW_ROWS` files, the pattern checked against EVERY file, and a walk that
    /// stops the moment a newer keystroke has taken over.
    #[test]
    fn compute_preview_checks_every_file_and_caps_the_rows() {
        let files: Vec<String> = (1..=PREVIEW_ROWS + 5)
            .map(|i| format!("C:\\nowhere\\pic{i}.png"))
            .collect();
        let (rows, error) = compute_preview(&files, "{name}-{n:2}", "", "", || false).unwrap();
        assert!(error.is_none());
        assert_eq!(
            rows.len(),
            PREVIEW_ROWS,
            "the list shows at most PREVIEW_ROWS rows"
        );
        assert!(
            rows[0].starts_with("pic1.png") && rows[0].ends_with("pic1-01.png"),
            "{}",
            rows[0]
        );
        // A find/replace acts on the expanded name.
        let (rows, _) = compute_preview(&files[..1], "{name}", "pic", "photo", || false).unwrap();
        assert!(rows[0].ends_with("photo1.png"), "{}", rows[0]);
    }

    #[test]
    fn compute_preview_reports_a_pattern_error_and_disables_nothing_else() {
        let files = vec!["C:\\nowhere\\a.png".to_string()];
        let (rows, error) = compute_preview(&files, "{nope}", "", "", || false).unwrap();
        assert!(rows.is_empty());
        assert!(
            error.is_some(),
            "an unknown placeholder is the error the dialog shows"
        );
    }

    #[test]
    fn compute_preview_abandons_a_superseded_walk() {
        let files: Vec<String> = (1..=10).map(|i| format!("C:\\nowhere\\{i}.png")).collect();
        let seen = std::cell::Cell::new(0u32);
        let superseded = || {
            seen.set(seen.get() + 1);
            seen.get() > 3
        };
        assert!(compute_preview(&files, "{name}", "", "", superseded).is_none());
        assert!(
            seen.get() <= 4,
            "stopped polling once superseded: {}",
            seen.get()
        );
    }
}
