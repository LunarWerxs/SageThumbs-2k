//! The Convert dialog's failure report: a scrollable, COPYABLE list of the files a batch
//! could not convert and why.
//!
//! 2026-09-05 audit, F11. The completion message box already named the files that failed
//! (issue #34) but not the reason, and a message box cannot be copied out of by any means a
//! user would find, so a folder of failures had to be reproduced by hand one file at a time
//! to learn anything. This window carries the same summary line, every failure with its
//! FULL path and reason, and a Copy button; it replaces the message box only when something
//! failed, so a clean run still gets the one-line box it always did.
//!
//! Built on the same `win::result_wndproc` / `win::result_layout` pair as the Image-info,
//! Upload-links and OCR result windows, plus one button of its own: a partial run that DID
//! write something must still offer to reveal it, which is what the old box's "Open output
//! folder?" question did.

use core::cell::{Cell, RefCell};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, run_dialog, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY};

const ID_EDIT: i32 = 100;
/// This dialog's own button, past `ID_RESULT_COPY` (101) so it cannot collide with the
/// shared result-dialog ids.
const ID_OPEN_FOLDER: i32 = 102;

thread_local! {
    /// The report text, set before `run_dialog`, read in WM_CREATE and by Copy.
    static REPORT: RefCell<String> = const { RefCell::new(String::new()) };
    /// Whether this run produced anything to reveal (a total failure has nothing).
    static CAN_OPEN: Cell<bool> = const { Cell::new(false) };
    /// Set by the Open-folder button, read by the caller once the window closes.
    static OPEN_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Show `report` over `owner` and return whether the user asked to open the output folder.
/// `can_open` is false when the run wrote nothing, and then no such button is offered.
pub(crate) unsafe fn show_convert_failures(owner: HWND, report: &str, can_open: bool) -> bool {
    REPORT.with(|r| *r.borrow_mut() = report.to_string());
    CAN_OPEN.with(|c| c.set(can_open));
    OPEN_REQUESTED.with(|o| o.set(false));
    // Modal over the Convert dialog, like the format-settings popup: the batch is over, and
    // a report that could be left behind an unrelated window would be missed entirely.
    // Title matches the message box this replaces.
    //
    // The shared `result_wndproc` posts a quit when this window is destroyed, which the
    // modal pump does not consume. Harmless here and only here: the caller tears the
    // Convert dialog down the moment this returns, so that pending quit is the very thing
    // that ends its pump. Do not copy this window's modal form to a dialog whose owner
    // carries on afterwards.
    run_dialog(
        w!("SageThumbs2KConvertReport"),
        Some(report_wndproc),
        "SageThumbs 2K",
        560,
        380,
        Some(owner),
    );
    OPEN_REQUESTED.with(Cell::get)
}

/// Headless capture of the report window (`--shot <out.png> --window convert-report`),
/// built off-screen and `PrintWindow`ed like every other app-window shot. Canned content,
/// so the layout (scrollable list, the three-button row inside the client) is verifiable
/// without a batch that actually has to fail first.
pub(crate) unsafe fn run_shot_convert_report(out: &str) -> bool {
    REPORT.with(|r| {
        *r.borrow_mut() = concat!(
            "Converted 2 of 5 image(s).\n\n",
            "These files could not be converted:\n",
            "C:\\photos\\holiday\\DSC_0043.psd\n",
            "    cannot decode C:\\photos\\holiday\\DSC_0043.psd\n",
            "C:\\photos\\holiday\\scan (2).tif\n",
            "    Access is denied. (os error 5)\n",
            "C:\\photos\\holiday\\render.exr\n",
            "    convert: no writer for .webp"
        )
        .to_string();
    });
    CAN_OPEN.with(|c| c.set(true));
    OPEN_REQUESTED.with(|o| o.set(false));
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KConvertReport"),
        Some(report_wndproc),
        "SageThumbs 2K",
        560,
        380,
    ) else {
        return false;
    };
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
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
    // optional Open-folder button.
    if CAN_OPEN.with(Cell::get) {
        ctl(
            hwnd,
            BUTTON,
            t("btn_open_folder"),
            WS_TABSTOP,
            copy_x - gap - btn_w,
            btn_y,
            btn_w,
            btn_h,
            ID_OPEN_FOLDER,
            hinst,
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
        // The Open-folder button is this dialog's own; it records the request and closes,
        // so the reveal happens after the modal loop has given the owner back its input.
        // Everything else (create, Copy, close, quit) is the shared result-dialog behaviour.
        if msg == WM_COMMAND && (wparam.0 & 0xFFFF) as i32 == ID_OPEN_FOLDER {
            OPEN_REQUESTED.with(|o| o.set(true));
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        if let Some(r) = crate::win::result_wndproc(hwnd, msg, wparam, build, copy_source) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
