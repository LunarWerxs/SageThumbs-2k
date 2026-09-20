//! The upload-result window — shows the uploaded link(s) in a selectable, read-only
//! edit with a **Copy** button (copies every link to the clipboard) and Close. Used by
//! the right-click "Upload" verb (`--upload-keep`, one line per image) and the
//! screenshot Upload button (`--upload`, a single link). The links are already on the
//! clipboard when this opens; Copy re-copies them (handy if the clipboard changed since,
//! or to grab them again after picking one out of the list). Modeled on `image_info.rs`.

use core::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{ES_MULTILINE, ES_READONLY, WINDOW_STYLE};

use crate::win::{
    result_buttons, result_edit, result_layout, result_window_proc, run_dialog, t, ResultWindow,
};

const ID_EDIT: i32 = 100;

thread_local! {
    /// (heading line, links joined by CRLF) — set before `run_dialog`, read in WM_CREATE.
    /// The edit shows the heading + the links; the Copy button copies ONLY the links.
    static RESULT: RefCell<(String, String)> =
        const { RefCell::new((String::new(), String::new())) };
}

/// Show the uploaded `links` (CRLF-separated — one per image) under `heading`, with a
/// Copy button that (re-)copies just the links to the clipboard.
pub fn show_upload_result(heading: &str, links: &str) {
    RESULT.with(|r| *r.borrow_mut() = (heading.to_string(), links.to_string()));
    unsafe {
        // `run_dialog`'s w/h are the TOTAL window size (no client adjustment), so the
        // client is ~30 design-px shorter than `h`. Size generously and keep the buttons
        // well inside the client — a too-short window clips the Copy/Close row.
        run_dialog(
            w!("SageThumbs2KUploadResult"),
            Some(result_window_proc::<UploadResult>),
            t("up_caption_file"),
            460,
            300,
            None,
        );
    }
}

/// Heading and links in a read-only, selectable, scrollable edit (a multi-image upload can
/// list many links); Copy re-copies ONLY the links, never the heading above them.
struct UploadResult;

impl ResultWindow for UploadResult {
    unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
        let l = result_layout(hwnd);
        let style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32);
        // The links are already CRLF-joined, the heading may not be; `result_edit` runs the
        // whole text through `to_crlf`, so neither can come out as `\r\r\n`.
        let text = RESULT.with(|r| {
            let (heading, links) = &*r.borrow();
            format!("{heading}\r\n\r\n{links}")
        });
        result_edit(hwnd, hinst, &l, l.m, style, ID_EDIT, &text);
        result_buttons(hwnd, hinst, &l);
    }

    unsafe fn copy_source(_hwnd: HWND) -> String {
        RESULT.with(|r| r.borrow().1.clone())
    }
}
