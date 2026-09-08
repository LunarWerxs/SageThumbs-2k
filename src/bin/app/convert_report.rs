//! The Convert dialog's failure report: a scrollable, COPYABLE list of the files a batch
//! could not convert and why, with a button to run just those again.
//!
//! 2026-09-05 audit, F11. The completion message box already named the files that failed
//! (issue #34) but not the reason, and a message box cannot be copied out of by any means a
//! user would find, so a folder of failures had to be reproduced by hand one file at a time
//! to learn anything. This window carries the same summary line, every failure with its
//! FULL path and reason, and a Copy button; it replaces the message box only when something
//! failed, so a clean run still gets the one-line box it always did.
//!
//! E01 of the same audit adds the retry. The report already knew exactly which inputs
//! failed, but received them as rendered text, so the only way to act on it was to select
//! those files in Explorer again by hand. It now receives the structured failures beside
//! the text and offers "Retry failed", which hands the caller the failed inputs, verbatim,
//! to run through the same batch with the same settings. The retry's own report comes back
//! through this window too, so a second failure can be retried again.
//!
//! Built on the same `win::result_wndproc` / `win::result_layout` pair as the Image-info,
//! Upload-links and OCR result windows, plus two buttons of its own: a partial run that DID
//! write something must still offer to reveal it, which is what the old box's "Open output
//! folder?" question did, and the retry.

use core::cell::{Cell, RefCell};

use sagethumbs2k_core::FileOutcome;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, run_dialog, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY};

const ID_EDIT: i32 = 100;
/// This dialog's own buttons, past `ID_RESULT_COPY` (101) so they cannot collide with the
/// shared result-dialog ids.
const ID_OPEN_FOLDER: i32 = 102;
const ID_RETRY: i32 = 103;

/// What the user chose on the report, for the Convert dialog to act on once this window
/// has closed and the modal loop has given the owner back its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReportAction {
    Close,
    /// Reveal the run's first output in Explorer.
    OpenFolder,
    /// Run exactly these inputs again, with the same settings. They are the failed entries'
    /// paths as the batch was given them, so the retry reads what the first run read.
    Retry(Vec<String>),
}

/// The button that closed the window; [`ReportAction`] is built from it on the way out,
/// since a `Cell` wants `Copy` and the retry list does not.
#[derive(Clone, Copy)]
enum Choice {
    Close,
    OpenFolder,
    Retry,
}

thread_local! {
    /// The report text, set before `run_dialog`, read in WM_CREATE and by Copy.
    static REPORT: RefCell<String> = const { RefCell::new(String::new()) };
    /// The failed inputs, verbatim: what Retry hands back.
    static RETRY_INPUTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Whether this run produced anything to reveal (a total failure has nothing).
    static CAN_OPEN: Cell<bool> = const { Cell::new(false) };
    /// Set by the Open-folder and Retry buttons, read by the caller once the window closes.
    static CHOICE: Cell<Choice> = const { Cell::new(Choice::Close) };
}

/// Show `report` over `owner`, with `failed` behind its Retry button, and return what the
/// user asked for. `can_open` is false when the run wrote nothing, and then no Open-folder
/// button is offered.
pub(crate) unsafe fn show_convert_failures(
    owner: HWND,
    report: &str,
    failed: &[FileOutcome],
    can_open: bool,
) -> ReportAction {
    REPORT.with(|r| *r.borrow_mut() = report.to_string());
    RETRY_INPUTS.with(|r| *r.borrow_mut() = failed.iter().map(|f| f.input.clone()).collect());
    CAN_OPEN.with(|c| c.set(can_open));
    CHOICE.with(|c| c.set(Choice::Close));
    // Modal over the Convert dialog, like the format-settings popup: the batch is over, and
    // a report that could be left behind an unrelated window would be missed entirely.
    // Title matches the message box this replaces.
    run_dialog(
        w!("SageThumbs2KConvertReport"),
        Some(report_wndproc),
        "SageThumbs 2K",
        560,
        380,
        Some(owner),
    );
    match CHOICE.with(Cell::get) {
        Choice::Close => ReportAction::Close,
        Choice::OpenFolder => ReportAction::OpenFolder,
        Choice::Retry => ReportAction::Retry(RETRY_INPUTS.with(|r| r.borrow().clone())),
    }
}

/// Headless capture of the report window (`--shot <out.png> --window convert-report`),
/// built off-screen and `PrintWindow`ed like every other app-window shot. Canned failures
/// rendered through the same `failure_report` the real run uses, so the layout (scrollable
/// list, the four-button row inside the client) is verifiable without a batch that actually
/// has to fail first, and the shot cannot drift from what a user sees.
pub(crate) unsafe fn run_shot_convert_report(out: &str) -> bool {
    let failed = [
        FileOutcome::failed(
            r"C:\photos\holiday\DSC_0043.psd",
            None,
            r"cannot decode C:\photos\holiday\DSC_0043.psd",
        ),
        FileOutcome::failed(
            r"C:\photos\holiday\scan (2).tif",
            None,
            "Access is denied. (os error 5)",
        ),
        FileOutcome::failed(
            r"C:\photos\holiday\render.exr",
            None,
            "convert: no writer for .webp",
        ),
    ];
    let counts = t("cv_done").replace("{ok}", "2").replace("{total}", "5");
    REPORT.with(|r| *r.borrow_mut() = crate::convert::failure_report(&counts, &failed));
    RETRY_INPUTS.with(|r| *r.borrow_mut() = failed.iter().map(|f| f.input.clone()).collect());
    CAN_OPEN.with(|c| c.set(true));
    CHOICE.with(|c| c.set(Choice::Close));
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KConvertReport"),
            wndproc: Some(report_wndproc),
            title: "SageThumbs 2K",
            design_w: 560,
            design_h: 380,
        },
        |_hwnd, _hinst| {},
        20,
        8,
        // This is the one capture that skips the final repaint (matches the original ritual).
        true,
    )
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Shared with the Image-info, Upload-links and OCR result windows. See
    // `win::result_layout` for why this comes off the real client rect, not the design size.
    let crate::win::ResultLayout {
        cw,
        m,
        btn_w,
        btn_h,
        gap,
        btn_y,
        close_x,
        copy_x,
        ..
    } = crate::win::result_layout(hwnd);
    let edit_h = (btn_y - gap - m).max(48);

    // Read-only and vertically scrollable: the list is as long as the run's failures, and
    // nothing here is meant to be edited. No ES_AUTOHSCROLL wrap either, a full path is
    // long, and folding it mid-path makes it harder to read than scrolling does.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        edit_style,
        m,
        m,
        cw - 2 * m,
        edit_h,
        ID_EDIT,
        hinst,
    );
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar.
    // Re-theme to DarkMode_Explorer so the scrollbar renders dark (the edit's own bg/text
    // stay dark via WM_CTLCOLOREDIT in `dark_ctlcolor`).
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    // Edit controls want CRLF line breaks (a lone LF renders as a box).
    let text = REPORT.with(|r| sagethumbs2k_core::clipboard::to_crlf(&r.borrow()).into_owned());
    let w = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));

    // Buttons bottom-right, inside the client: Close rightmost, then Copy, then the
    // optional Open-folder button, then Retry. `leftmost` walks left as each is placed.
    let mut leftmost = copy_x;
    if CAN_OPEN.with(Cell::get) {
        leftmost -= gap + btn_w;
        ctl(
            hwnd,
            BUTTON,
            t("btn_open_folder"),
            WS_TABSTOP,
            leftmost,
            btn_y,
            btn_w,
            btn_h,
            ID_OPEN_FOLDER,
            hinst,
        );
    }
    if RETRY_INPUTS.with(|r| !r.borrow().is_empty()) {
        // Sized to its label rather than the shared 82px: "Retry failed" runs long in
        // several languages, and a clipped verb on the one button that acts is worse than
        // an uneven row. The row has room for it, which the locale test below checks.
        let label = t("btn_retry_failed");
        let retry_w = crate::convert::cv_btn_w(hwnd, label, btn_w);
        leftmost -= gap + retry_w;
        ctl(
            hwnd, BUTTON, label, WS_TABSTOP, leftmost, btn_y, retry_w, btn_h, ID_RETRY, hinst,
        );
    }
    ctl(
        hwnd,
        BUTTON,
        t("btn_copy"),
        WS_TABSTOP,
        copy_x,
        btn_y,
        btn_w,
        btn_h,
        ID_RESULT_COPY,
        hinst,
    );
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

/// What the Copy button puts on the clipboard: the whole report, paths and all. The EDIT is
/// read-only, so its contents can never differ from the stored text.
unsafe fn copy_source(_hwnd: HWND) -> String {
    REPORT.with(|r| r.borrow().clone())
}

extern "system" fn report_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        // The Open-folder and Retry buttons are this dialog's own; each records the request
        // and closes, so the reveal or the retry happens after the modal loop has given the
        // owner back its input. Everything else (create, Copy, close) is the shared
        // result-dialog behaviour.
        if msg == WM_COMMAND {
            let choice = match (wparam.0 & 0xFFFF) as i32 {
                ID_OPEN_FOLDER => Some(Choice::OpenFolder),
                ID_RETRY => Some(Choice::Retry),
                _ => None,
            };
            if let Some(choice) = choice {
                CHOICE.with(|c| c.set(choice));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
        }
        // NOT the shared WM_DESTROY, which posts a quit for the top-level result dialogs'
        // pump. This window is modal, and `run_dialog`'s modal pump ends the moment the
        // window is gone with no quit needed; the posted one would sit in the queue until
        // the Convert dialog's own pump read it, which ended that dialog mid-retry. Before
        // the retry existed the stray quit was harmless only because the caller tore the
        // Convert dialog down the moment this returned.
        if msg == WM_DESTROY {
            return LRESULT(0);
        }
        if let Some(r) = crate::win::result_wndproc(hwnd, msg, wparam, build, copy_source) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    /// The Retry button is sized to its label, but the row it shares is finite: Close, Copy
    /// and Open folder take three shared-width slots from the right, and what is left of
    /// the 560px window's client (about 254px once the frame and the margins are paid) is
    /// the most any translation of `btn_retry_failed` may need. Walks all 36 shipped
    /// locales through the same sizing the real window uses, so a translation long enough
    /// to push the button off the left edge fails here rather than shipping clipped.
    #[test]
    fn every_locale_retry_label_fits_the_button_row() {
        const RETRY_W_MAX: i32 = 240;
        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
            let label = pairs
                .iter()
                .find(|(k, _)| *k == "btn_retry_failed")
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("{code}: btn_retry_failed missing"));
            let w = crate::convert::cv_btn_col(unsafe { crate::win::design_text_w(label) }, 82);
            assert!(
                w <= RETRY_W_MAX,
                "{code}: \"{label}\" needs a {w}px button, over the {RETRY_W_MAX}px the row \
                 has left beside Open folder, Copy and Close; shorten the translation"
            );
        }
    }
}
