//! The "Recent uploads" window: every link this machine uploaded, newest first, with how long
//! each one has left before its host deletes it.
//!
//! Upload hosts range from three hours (uguu.se) through 72 hours (litterbox) and ~100 days
//! (x0.at, by size) to no expiry date (catbox.moe, which removes a file only after 2 years
//! without a view), and the link carries no hint of which, so a
//! link shared last week could be dead or good for months with nothing to tell them apart. A
//! user asked for a countdown on the link's page (2026-09-21); that page is the host's, so the
//! countdown lives here instead, fed by the list every upload writes
//! (`sagethumbs2k_core::upload_history`).
//!
//! Same shape as the other result windows (`win::result_window_proc`: a read-only edit, Copy,
//! Close, dark mode). Copy puts every link that has not expired on the clipboard, newest first.
//! Opened from Settings ▸ Screenshots, the upload-result window, the tray menu
//! (`--upload-history`), and headlessly by `--shot <out.png> --window uploads`.

use core::cell::RefCell;

use sagethumbs2k_core::upload_history::{
    duration_text, load, DurationWords, Entry, Expiry, Status,
};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{ES_MULTILINE, ES_READONLY, WINDOW_STYLE};

use crate::win::{
    result_buttons, result_edit, result_layout, result_window_proc, run_dialog, t, ResultWindow,
};

const ID_EDIT: i32 = 100;
const DESIGN_W: i32 = 640;
const DESIGN_H: i32 = 440;

thread_local! {
    /// (the edit's text, the links Copy puts on the clipboard) — set before `run_dialog`.
    static LIST: RefCell<(String, String)> =
        const { RefCell::new((String::new(), String::new())) };
}

/// Every translated phrase an expiry is said with, gathered once so the text builders stay
/// pure (and testable with plain English).
pub(crate) struct ExpiryWords<'a> {
    pub(crate) dur: DurationWords<'a>,
    /// Under a fresh link: "Expires {date} (in {left})".
    pub(crate) expires: &'a str,
    /// Under a fresh link to a host with no expiry.
    pub(crate) no_expiry: &'a str,
    /// In the list: "{left} left, until {date}".
    pub(crate) left: &'a str,
    /// In the list: "Expired {date}".
    pub(crate) expired: &'a str,
    /// In the list: a host with no expiry.
    pub(crate) list_no_expiry: &'a str,
    /// In the list: a custom host whose policy we don't know.
    pub(crate) unknown: &'a str,
    /// In the list: "uploaded {date}".
    pub(crate) uploaded: &'a str,
}

impl ExpiryWords<'static> {
    /// The words in the user's language.
    pub(crate) fn current() -> Self {
        ExpiryWords {
            dur: DurationWords {
                days: t("dur_d"),
                days_hours: t("dur_dh"),
                hours: t("dur_h"),
                hours_minutes: t("dur_hm"),
                minutes: t("dur_m"),
            },
            expires: t("up_expires"),
            no_expiry: t("up_no_expiry"),
            left: t("up_hist_left"),
            expired: t("up_hist_expired"),
            list_no_expiry: t("up_hist_no_expiry"),
            unknown: t("up_hist_unknown"),
            uploaded: t("up_hist_uploaded"),
        }
    }
}

/// Where one remembered link stands, in words.
fn status_words(e: &Entry, now: u64, w: &ExpiryWords) -> String {
    match e.status(now) {
        Status::Left(left) => w
            .left
            .replace("{left}", &duration_text(left, &w.dur))
            .replace(
                "{date}",
                &sagethumbs2k_core::unixtime::local_datetime(now.saturating_add(left)),
            ),
        Status::Expired(at) => w
            .expired
            .replace("{date}", &sagethumbs2k_core::unixtime::local_datetime(at)),
        Status::NoExpiry => w.list_no_expiry.to_string(),
        Status::Unknown => w.unknown.to_string(),
    }
}

/// The second line under a link: "shot.png · uploaded 2026-09-21 23:50 · 2 d 23 h left, until
/// 2026-09-24 23:50" (the name is left out when there isn't one).
fn detail_line(e: &Entry, now: u64, w: &ExpiryWords) -> String {
    let uploaded = w.uploaded.replace(
        "{date}",
        &sagethumbs2k_core::unixtime::local_datetime(e.uploaded),
    );
    let status = status_words(e, now, w);
    if e.name.is_empty() {
        format!("{uploaded} · {status}")
    } else {
        format!("{} · {uploaded} · {status}", e.name)
    }
}

/// (the window's text, the links Copy copies) for `entries` (newest first) at `now`. Copy
/// takes every link that has not expired: an expired one is dead, and pasting it anywhere
/// would only hand someone a 404.
pub(crate) fn history_text(
    entries: &[Entry],
    now: u64,
    heading: &str,
    empty: &str,
    w: &ExpiryWords,
) -> (String, String) {
    if entries.is_empty() {
        return (empty.to_string(), String::new());
    }
    let mut text = format!("{heading}\r\n");
    for e in entries {
        text.push_str("\r\n");
        text.push_str(&e.url);
        text.push_str("\r\n   ");
        text.push_str(&detail_line(e, now, w));
    }
    let live: Vec<&str> = entries
        .iter()
        .filter(|e| e.is_live(now))
        .map(|e| e.url.as_str())
        .collect();
    (text, live.join("\r\n"))
}

fn fill(entries: &[Entry], now: u64) {
    let pair = history_text(
        entries,
        now,
        t("up_hist_heading"),
        t("up_hist_empty"),
        &ExpiryWords::current(),
    );
    LIST.with(|l| *l.borrow_mut() = pair);
}

/// Open the list. `owner` makes it modal to whatever opened it (Settings, the upload result);
/// `None` is the tray's own `--upload-history` process.
pub(crate) fn show_history(owner: Option<HWND>) {
    fill(&load(), sagethumbs2k_core::unixtime::now());
    unsafe {
        run_dialog(
            w!("SageThumbs2KUploadHistory"),
            Some(result_window_proc::<History>),
            t("up_hist_caption"),
            DESIGN_W,
            DESIGN_H,
            owner,
        );
    }
}

/// One entry of each kind, for the headless shots (the real list is whatever this machine
/// happens to have uploaded, which a layout check must not depend on).
pub(crate) fn sample_entries(now: u64) -> Vec<Entry> {
    let e = |ago: u64, expires: Expiry, host: &str, url: &str, name: &str| Entry {
        uploaded: now.saturating_sub(ago),
        expires,
        host: host.to_string(),
        url: url.to_string(),
        name: name.to_string(),
    };
    vec![
        e(
            0,
            Expiry::At(now + 72 * 3600),
            "litterbox.catbox.moe",
            "https://litter.catbox.moe/q7x2ab.png",
            "screenshot.png",
        ),
        e(
            0,
            Expiry::At(now + 8_600_000),
            "x0.at",
            "https://x0.at/Hk3v.jpg",
            "holiday-photo.jpg",
        ),
        e(
            86_400,
            Expiry::NoDate,
            "catbox.moe",
            "https://files.catbox.moe/9f2kqe.png",
            "logo-final.png",
        ),
        e(
            5 * 86_400,
            Expiry::At(now - 4 * 86_400),
            "uguu.se",
            "https://h.uguu.se/aBcDeFgh.png",
            "screenshot.png",
        ),
    ]
}

/// Headless capture (`--shot <out.png> --window uploads`) over [`sample_entries`].
pub(crate) unsafe fn run_shot_history(out: &str) -> bool {
    let now = sagethumbs2k_core::unixtime::now();
    fill(&sample_entries(now), now);
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KUploadHistoryShot"),
            wndproc: Some(result_window_proc::<History>),
            title: t("up_hist_caption"),
            design_w: DESIGN_W,
            design_h: DESIGN_H,
        },
        |_hwnd, _hinst| {},
        20,
        8,
        false,
    )
}

struct History;

impl ResultWindow for History {
    unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
        let l = result_layout(hwnd);
        let style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32);
        let text = LIST.with(|r| r.borrow().0.clone());
        result_edit(hwnd, hinst, &l, l.m, style, ID_EDIT, &text);
        result_buttons(hwnd, hinst, &l);
    }

    unsafe fn copy_source(_hwnd: HWND) -> String {
        LIST.with(|r| r.borrow().1.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sagethumbs2k_core::upload_history::ENGLISH;

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

    #[test]
    fn the_list_says_each_state_and_copy_skips_the_dead_links() {
        let now = 1_800_000_000;
        let entries = sample_entries(now);
        let (text, copy) = history_text(&entries, now, "HEAD", "EMPTY", &words());
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines[0], "HEAD");
        assert_eq!(lines[2], "https://litter.catbox.moe/q7x2ab.png");
        assert!(
            lines[3].starts_with("   screenshot.png · uploaded "),
            "{:?}",
            lines[3]
        );
        assert!(lines[3].contains(" · 3 d left, until "), "{:?}", lines[3]);
        assert!(
            lines[5].contains(" · 99 d 12 h left, until "),
            "{:?}",
            lines[5]
        );
        assert!(lines[7].ends_with(" · No expiry date"), "{:?}", lines[7]);
        assert!(lines[9].contains(" · Expired "), "{:?}", lines[9]);
        assert_eq!(lines.len(), 10);
        assert_eq!(
            copy,
            "https://litter.catbox.moe/q7x2ab.png\r\nhttps://x0.at/Hk3v.jpg\r\nhttps://files.catbox.moe/9f2kqe.png",
            "the expired uguu link is not copied"
        );
    }

    #[test]
    fn an_empty_list_says_so_and_copies_nothing() {
        assert_eq!(
            history_text(&[], 5, "HEAD", "EMPTY", &words()),
            ("EMPTY".to_string(), String::new())
        );
    }

    #[test]
    fn a_nameless_entry_drops_the_name_column() {
        let mut e = sample_entries(1_000_000).remove(2);
        e.name.clear();
        let line = detail_line(&e, 1_000_000, &words());
        assert!(line.starts_with("uploaded "), "{line}");
        assert!(line.ends_with(" · No expiry date"), "{line}");
    }

    #[test]
    fn every_phrase_the_window_uses_exists_and_keeps_its_placeholders() {
        let w = ExpiryWords::current();
        for (phrase, holes) in [
            (w.expires, &["{date}", "{left}"][..]),
            (w.left, &["{date}", "{left}"][..]),
            (w.expired, &["{date}"][..]),
            (w.uploaded, &["{date}"][..]),
            (w.dur.days, &["{d}"][..]),
            (w.dur.days_hours, &["{d}", "{h}"][..]),
            (w.dur.hours, &["{h}"][..]),
            (w.dur.hours_minutes, &["{h}", "{m}"][..]),
            (w.dur.minutes, &["{m}"][..]),
        ] {
            for hole in holes {
                assert!(phrase.contains(hole), "{phrase:?} lost {hole}");
            }
        }
        assert!(!w.no_expiry.is_empty() && !w.list_no_expiry.is_empty() && !w.unknown.is_empty());
    }
}
