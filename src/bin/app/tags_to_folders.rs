//! The DLL's "Sort into folders ▸ By audio tag" verb on an audio selection
//! (`--tags-to-folders <listfile>`). Dialog: destination, a `$artist - $album`
//! folder-name template, and copy-vs-move. The sort engine is in the lib
//! (`st2k_actions::verbs::tags_to_folders`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Controls::PBS_MARQUEE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    checked, ctl, edit_field, get_edit_text, label, pick_folder, read_listfile, run_dialog,
    set_edit_text, t, wide, BM_SETCHECK_MSG, BUTTON, IDCANCEL, IDOK,
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
            // DPI, the deferred close a running sort needs, destroy, default.
            _ => crate::win::dialog_tail(hwnd, msg, wparam, lparam, request_close),
        }
    }
}

/// The folder holding the first selected file, if there is one.
fn first_file_folder() -> Option<String> {
    TTF_FILES
        .get()
        .and_then(|f| f.first())
        .and_then(|p| parent_folder(p))
}

/// The parent folder of `path` as a lossy UTF-8 string, or `None` when `path` has no
/// parent — a bare drive root, or an empty path.
fn parent_folder(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

/// The field's text, trimmed, or `fallback` when the field holds only whitespace (or is
/// empty): the dialog pre-fills every field, so a cleared one means "use the default",
/// never "use an empty string".
fn ttf_field_or(field: &str, fallback: &str) -> String {
    let value = field.trim();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

/// The y (96-DPI design px) of the dialog's button row: the physical client bottom
/// scaled back to design px, less the 12px bottom margin and the 30px the row is tall.
/// The caller floors the DPI at 96, so this never divides below 1:1.
fn button_row_y(client_bottom_px: i32, dpi: i32) -> i32 {
    client_bottom_px * 96 / dpi - 12 - 30
}

/// `WM_CREATE`: lay out the destination/template/missing-token edits, the move/copy radio
/// pair, and the sort/cancel button row anchored to the real client bottom.
unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst = crate::files_to_folder::module_instance();
    // Default destination = the first file's folder.
    let default_dest = first_file_folder().unwrap_or_default();

    label(hwnd, hinst, t("ttf_destination"), 16, 18, 90, 18);
    edit_field(hwnd, hinst, &default_dest, 110, 16, 268, 24, CID_TTF_DEST);
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

    label(hwnd, hinst, t("ttf_template"), 16, 56, 90, 18);
    edit_field(
        hwnd,
        hinst,
        t("ttf_template_default"),
        110,
        54,
        318,
        24,
        CID_TTF_TEMPLATE,
    );
    label(hwnd, hinst, t("ttf_tokens"), 110, 82, 318, 16);

    label(hwnd, hinst, t("ttf_missing"), 16, 112, 90, 18);
    edit_field(
        hwnd,
        hinst,
        t("ttf_missing_default"),
        110,
        110,
        160,
        24,
        CID_TTF_MISSING,
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
    let by = button_row_y(rc.bottom, dpi);
    crate::files_to_folder::ok_cancel_buttons(hwnd, hinst, "ttf_sort", 244, by, 92, 342);
    LRESULT(0)
}

/// `WM_COMMAND`: dispatch by control/menu id.
unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = crate::win::command_id(wparam);
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
    let dest = ttf_field_or(
        &get_edit_text(hwnd, CID_TTF_DEST),
        &first_file_folder().unwrap_or_else(|| ".".to_string()),
    );
    let template = ttf_field_or(
        &get_edit_text(hwnd, CID_TTF_TEMPLATE),
        t("ttf_template_default"),
    );
    let missing = ttf_field_or(
        &get_edit_text(hwnd, CID_TTF_MISSING),
        t("ttf_missing_default"),
    );
    let move_files = checked(hwnd, CID_TTF_MOVE);
    let Some(files) = TTF_FILES.get().cloned() else {
        return;
    };

    crate::files_to_folder::start_batch(
        hwnd,
        &TTF_RUNNING,
        &[
            CID_TTF_DEST,
            CID_TTF_BROWSE,
            CID_TTF_TEMPLATE,
            CID_TTF_MISSING,
            CID_TTF_MOVE,
            CID_TTF_COPY,
            IDOK,
        ],
        CID_TTF_PROGRESS,
        WM_TTF_DONE,
        move || {
            let (done, skipped) = st2k_actions::verbs::tags_to_folders(
                &files,
                std::path::Path::new(&dest),
                &template,
                &missing,
                move_files,
            );
            *TTF_RESULT.lock().unwrap() = Some((done, skipped, move_files));
        },
    );
}

/// The locale key of the finished-sort prompt, chosen by whether the batch moved or copied.
fn ttf_done_key(move_files: bool) -> &'static str {
    if move_files {
        "ttf_done_moved"
    } else {
        "ttf_done_copied"
    }
}

/// Fills the `{done}` and `{skipped}` placeholders of a finished-sort prompt.
fn ttf_done_message(prompt: &str, done: usize, skipped: usize) -> String {
    prompt
        .replace("{done}", &done.to_string())
        .replace("{skipped}", &skipped.to_string())
}

/// `WM_TTF_DONE`: report the done/skipped counts the worker thread produced, then close.
unsafe fn on_ttf_done(hwnd: HWND) -> LRESULT {
    TTF_RUNNING.store(false, Ordering::Relaxed);
    if let Some((done, skipped, move_files)) = TTF_RESULT.lock().unwrap().take() {
        let m = wide(&ttf_done_message(
            t(ttf_done_key(move_files)),
            done,
            skipped,
        ));
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

/// Close the dialog, or defer the close if a sort is still running — same
/// reasoning as `files_to_folder.rs::request_close`.
unsafe fn request_close(hwnd: HWND) {
    crate::files_to_folder::close_or_defer(hwnd, &TTF_RUNNING);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttf_field_or_keeps_a_trimmed_value() {
        assert_eq!(ttf_field_or("  C:\\music \t", "fallback"), "C:\\music");
    }

    #[test]
    fn ttf_field_or_falls_back_when_nothing_was_typed() {
        assert_eq!(ttf_field_or("   \n", "Unknown"), "Unknown");
        assert_eq!(ttf_field_or("", "Unknown"), "Unknown");
    }

    #[test]
    fn ttf_done_message_fills_both_placeholders() {
        let got = ttf_done_message("Moved {done} file(s).\n{skipped} skipped.", 3, 2);
        assert_eq!(got, "Moved 3 file(s).\n2 skipped.");
    }

    #[test]
    fn ttf_done_message_fills_placeholders_in_either_order() {
        assert_eq!(
            ttf_done_message("{skipped} skipped, {done} moved", 4, 1),
            "1 skipped, 4 moved"
        );
    }

    #[test]
    fn ttf_done_message_renders_a_zero_count_without_leaving_a_brace() {
        let got = ttf_done_message("{done} done, {skipped} skipped", 0, 0);
        assert_eq!(got, "0 done, 0 skipped");
    }

    #[test]
    fn ttf_done_key_names_the_move_prompt() {
        assert_eq!(ttf_done_key(true), "ttf_done_moved");
        assert_eq!(ttf_done_key(false), "ttf_done_copied");
    }

    #[test]
    fn parent_folder_keeps_the_folder() {
        assert_eq!(
            parent_folder("C:\\media\\song.mp3"),
            Some("C:\\media".to_string())
        );
    }

    #[test]
    fn parent_folder_has_none_for_a_drive_root() {
        assert_eq!(parent_folder("C:\\"), None);
    }

    #[test]
    fn button_row_y_scales_the_client_bottom_back_to_design_px() {
        assert_eq!(button_row_y(300, 96), 258);
        assert_eq!(button_row_y(300, 192), 108);
    }
}
