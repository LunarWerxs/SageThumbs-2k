//! The upload-result window — shows the uploaded link(s) in a selectable, read-only
//! edit with a **Copy** button (copies every link to the clipboard) and Close. Used by
//! the right-click "Upload" verb (`--upload-keep`, one line per image) and the
//! screenshot Upload button (`--upload`, a single link). The links are already on the
//! clipboard when this opens; Copy re-copies them (handy if the clipboard changed since,
//! or to grab them again after picking one out of the list). Modeled on `image_info.rs`.
//!
//! Under each link sits when its host deletes it ("Expires 2026-09-24 23:50 (in 3 d)"), and a
//! **Recent uploads…** button opens the list of every earlier link with the time each has
//! left (`upload_history_dlg`). The link itself carries no hint of either, and the hosts range
//! from three hours to no expiry date.

use core::cell::RefCell;

use sagethumbs2k_core::upload_history::{duration_text, local_datetime, now_unix, Entry, Status};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, ES_MULTILINE, ES_READONLY, WINDOW_STYLE, WM_COMMAND, WS_TABSTOP,
};

use crate::upload_history_dlg::ExpiryWords;
use crate::win::{result_buttons, result_edit, result_layout, run_dialog, t};

const ID_EDIT: i32 = 100;
/// "Recent uploads…", bottom-left. 101 is the shared Copy button (`win::ID_RESULT_COPY`).
const ID_HISTORY: i32 = 102;

thread_local! {
    /// (the edit's text, the links joined by CRLF) — set before `run_dialog`, read in
    /// WM_CREATE. The edit shows the heading, the links and their expiry; Copy copies ONLY
    /// the links.
    static RESULT: RefCell<(String, String)> =
        const { RefCell::new((String::new(), String::new())) };
}

/// The links of `done`, CRLF-joined: what the clipboard gets and what Copy re-copies.
pub(crate) fn links_of(done: &[Entry]) -> String {
    done.iter()
        .map(|e| e.url.as_str())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// The one line said under a fresh link: when it expires, that it doesn't, or nothing for a
/// custom host whose policy we don't know (saying "unknown" under every link of someone's own
/// server would be noise).
pub(crate) fn expiry_line(e: &Entry, now: u64, w: &ExpiryWords) -> Option<String> {
    match e.status(now) {
        Status::Left(left) => Some(
            w.expires
                .replace("{date}", &local_datetime(now.saturating_add(left)))
                .replace("{left}", &duration_text(left, &w.dur)),
        ),
        Status::NoExpiry => Some(w.no_expiry.to_string()),
        Status::Expired(_) | Status::Unknown => None,
    }
}

/// The edit's text: the heading, a blank line, then each link with its expiry indented
/// beneath it.
pub(crate) fn result_text(heading: &str, done: &[Entry], now: u64, w: &ExpiryWords) -> String {
    let mut text = format!("{heading}\r\n");
    for e in done {
        text.push_str("\r\n");
        text.push_str(&e.url);
        if let Some(line) = expiry_line(e, now, w) {
            text.push_str("\r\n   ");
            text.push_str(&line);
        }
    }
    text
}

/// Show the uploaded `done` links under `heading`, each with its expiry, with a Copy button
/// that (re-)copies just the links to the clipboard.
pub fn show_upload_result(heading: &str, done: &[Entry]) {
    fill(heading, done, now_unix());
    unsafe {
        // `run_dialog`'s w/h are the TOTAL window size (no client adjustment), so the
        // client is ~30 design-px shorter than `h`. Size generously and keep the buttons
        // well inside the client — a too-short window clips the Copy/Close row.
        run_dialog(
            w!("SageThumbs2KUploadResult"),
            Some(upload_result_proc),
            t("up_caption_file"),
            500,
            320,
            None,
        );
    }
}

fn fill(heading: &str, done: &[Entry], now: u64) {
    let text = result_text(heading, done, now, &ExpiryWords::current());
    RESULT.with(|r| *r.borrow_mut() = (text, links_of(done)));
}

/// Headless capture (`--shot <out.png> --window upload`) over canned uploads: the window only
/// appears after a real upload, which a shot cannot arrange.
pub(crate) unsafe fn run_shot_upload_result(out: &str) -> bool {
    let now = now_unix();
    let done = crate::upload_history_dlg::sample_entries(now);
    let heading = t("up_done_all").replace("{total}", "2");
    fill(&heading, &done[..2], now);
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KUploadResultShot"),
            wndproc: Some(upload_result_proc),
            title: t("up_caption_file"),
            design_w: 500,
            design_h: 320,
        },
        |_hwnd, _hinst| {},
        20,
        8,
        false,
    )
}

/// Heading and links in a read-only, selectable, scrollable edit (a multi-image upload can
/// list many links); "Recent uploads…" on the left of the button row, Copy and Close on the
/// right.
unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    let l = result_layout(hwnd);
    let style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32);
    let text = RESULT.with(|r| r.borrow().0.clone());
    result_edit(hwnd, hinst, &l, l.m, style, ID_EDIT, &text);
    crate::win::ctl(
        hwnd,
        crate::win::BUTTON,
        t("btn_recent_uploads"),
        WS_TABSTOP,
        l.m,
        l.btn_y,
        (l.copy_x - l.gap - l.m).min(170),
        l.btn_h,
        ID_HISTORY,
        hinst,
    );
    result_buttons(hwnd, hinst, &l);
}

unsafe fn copy_source(_hwnd: HWND) -> String {
    RESULT.with(|r| r.borrow().1.clone())
}

extern "system" fn upload_result_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if let Some(r) = crate::dark::dark_ctlcolor(msg, wparam) {
            return r;
        }
        if msg == WM_COMMAND && crate::win::command_id(wparam) == ID_HISTORY {
            crate::upload_history_dlg::show_history(Some(hwnd));
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
    use super::*;
    use sagethumbs2k_core::upload_history::{Expiry, ENGLISH};

    fn words() -> ExpiryWords<'static> {
        ExpiryWords {
            dur: ENGLISH,
            expires: "Expires {date} (in {left})",
            no_expiry: "No expiry date",
            left: "{left} left, until {date}",
            expired: "Expired {date}",
            list_no_expiry: "No expiry date",
            unknown: "Expiry unknown",
            uploaded: "uploaded {date}",
        }
    }

    fn at(url: &str, expires: Expiry) -> Entry {
        Entry {
            uploaded: 1_000,
            expires,
            host: "h".into(),
            url: url.into(),
            name: "n.png".into(),
        }
    }

    #[test]
    fn each_link_carries_its_own_expiry_and_copy_gets_only_links() {
        let now = 1_000;
        let done = [
            at(
                "https://litter.catbox.moe/a.png",
                Expiry::At(now + 72 * 3600),
            ),
            at("https://files.catbox.moe/b.png", Expiry::NoDate),
            at("https://my.host/c.png", Expiry::Unknown),
        ];
        let text = result_text("Uploaded all 3", &done, now, &words());
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines[0], "Uploaded all 3");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "https://litter.catbox.moe/a.png");
        assert!(lines[3].starts_with("   Expires "), "{:?}", lines[3]);
        assert!(lines[3].ends_with("(in 3 d)"), "{:?}", lines[3]);
        assert_eq!(lines[4], "https://files.catbox.moe/b.png");
        assert_eq!(lines[5], "   No expiry date");
        assert_eq!(
            lines[6], "https://my.host/c.png",
            "an unknown host gets no line"
        );
        assert_eq!(lines.len(), 7);
        assert_eq!(
            links_of(&done),
            "https://litter.catbox.moe/a.png\r\nhttps://files.catbox.moe/b.png\r\nhttps://my.host/c.png"
        );
    }
}
