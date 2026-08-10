//! The **Send feedback** dialog — a suggestion / bug-report / format-request box
//! wired straight to the developer, so a user with an idea doesn't have to own a
//! GitHub account to be heard.
//!
//! Reached from two places, both in-process (no subprocess, unlike `--convert`):
//! the About box's "Send feedback" pill and Settings ▸ Advanced ▸ Feedback. Both
//! open it as a nested modal via [`run_dialog`] with `Some(owner)` — which pumps
//! with `pump_until_closed`, so this wndproc must **NOT** `PostQuitMessage` on
//! destroy (that would tear down the owner's message loop too). Same rule the
//! Convert dialog's per-format settings sheet follows.
//!
//! The submit is a single form-encoded POST. Nothing is sent until the user fills
//! the box and clicks Send — the contact field is optional and clearly labelled as
//! "only if you want a reply", so leaving it blank still delivers the message.
//! If the POST fails the text is put on the clipboard and the user is offered the
//! GitHub issue page, so a dead network never eats what they typed.

use core::cell::Cell;
use core::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{NMHDR, NMLINK, NM_CLICK, NM_RETURN};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_ctlcolor, dark_ctlcolor_dim, dark_theme_combo};
use crate::win::{
    combo_sel, ctl, get_edit_text, open_url, run_dialog, set_clipboard_text, t, wide,
    wm_dpichanged, wstr_to_string, BUTTON, COMBOBOX, EDIT, IDCANCEL, IDOK, STATIC, SYSLINK,
    URL_GITHUB,
};

/// Where a submitted message goes. Same host as the sponsor manifest / update check.
const FEEDBACK_URL: &str = "https://st2k.lunarwerx.com/feedback";

/// How long the POST may take before we call it a failure and offer the GitHub
/// fallback. Generous enough for a slow phone tether, short enough not to feel hung.
const SEND_TIMEOUT_SECS: u64 = 15;

/// Reply cap — we only need the status code, so anything the server says is noise.
const MAX_RESP: usize = 8 * 1024;

/// Length caps. Not a UI limit (the edits aren't restricted, so a pasted essay isn't
/// silently truncated mid-typing) — applied once at submit so one submission can't
/// post an unbounded body.
const MAX_MSG: usize = 4000;
const MAX_CONTACT: usize = 200;

/// The "what's this about?" buckets: (locale key for the label, wire value).
/// Order is the dropdown order; the first is the default because a suggestion box
/// is what this is *for* — a bug report is the second-most-likely reason to open it.
const CATS: &[(&str, &str)] = &[
    ("fb_cat_suggestion", "suggestion"),
    ("fb_cat_bug", "bug"),
    ("fb_cat_format", "format"),
    ("fb_cat_other", "other"),
];

const ID_HEAD: i32 = 100;
const ID_CAT_LBL: i32 = 101;
const ID_CAT: i32 = 102;
const ID_MSG_LBL: i32 = 103;
const ID_MSG: i32 = 104;
const ID_EMAIL_LBL: i32 = 105;
const ID_EMAIL: i32 = 106;
const ID_GH_LINK: i32 = 107;

/// Worker → UI: the POST finished. `wparam` is 1 on success, 0 otherwise.
const WM_FB_DONE: u32 = WM_APP + 3;

/// Design size of the dialog (whole window; [`run_dialog`] adjusts nothing, so the
/// client is ~30 design px shorter — `build` lays out against the real client rect).
const DLG_W: i32 = 470;
const DLG_H: i32 = 400;

thread_local! {
    /// True while a POST is in flight — guards against a double submit (the button is
    /// also disabled, but a keyboard default-button repeat shouldn't depend on that).
    static SENDING: Cell<bool> = const { Cell::new(false) };
}

/// The GitHub issue page, offered as the public alternative (and as the fallback when
/// the POST can't get through).
fn issues_url() -> String {
    format!("{URL_GITHUB}/issues/new")
}

/// Open the feedback dialog modal to `owner` (the About box or the Settings window).
pub(crate) unsafe fn show_feedback(owner: HWND) {
    run_dialog(
        w!("SageThumbs2KFeedback"),
        Some(feedback_wndproc),
        t("fb_title"),
        DLG_W,
        DLG_H,
        Some(owner),
    );
}

/// Headless capture (`--shot <out.png> --window feedback`) — built off-screen and
/// `PrintWindow`ed like every other app-window shot, so the layout is verifiable
/// without opening a window or touching the network.
pub(crate) unsafe fn run_shot_feedback(out: &str) -> bool {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KFeedback"),
        Some(feedback_wndproc),
        t("fb_title"),
        DLG_W,
        DLG_H,
    ) else {
        return false;
    };
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area in design px: `run_dialog`'s w/h size the
    // whole WINDOW, so hardcoding design coords would push the button row under the
    // frame. `ctl` re-scales design px to DPI, so divide the physical rect back to 96.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let cw = rc.right * 96 / dpi;
    let ch = rc.bottom * 96 / dpi;

    let m = 16;
    let (btn_w, btn_h, gap) = (92, 28, 8);
    let lbl = WINDOW_STYLE(0);

    // Bottom-anchored rows first, then the message box takes whatever is left — so the
    // free-text field is the part that grows, which is the part people actually use.
    let btn_y = ch - m - btn_h;
    let email_h = 24;
    let email_y = btn_y - 16 - email_h;
    let email_lbl_y = email_y - 20;
    let head_h = 34; // two wrapped lines of the intro
    let row_y = m + head_h + 12;
    let row_h = 24;
    let msg_lbl_y = row_y + row_h + 12;
    let msg_y = msg_lbl_y + 20;
    let msg_h = (email_lbl_y - 12 - msg_y).max(70);

    ctl(
        hwnd,
        STATIC,
        t("fb_heading"),
        lbl,
        m,
        m,
        cw - 2 * m,
        head_h,
        ID_HEAD,
        hinst,
    );

    // Row: "What's this about?" + the bucket dropdown.
    ctl(
        hwnd,
        STATIC,
        t("fb_category"),
        lbl,
        m,
        row_y + 4,
        112,
        18,
        ID_CAT_LBL,
        hinst,
    );
    let cat = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        m + 120,
        row_y,
        200,
        200,
        ID_CAT,
        hinst,
    );
    for (key, _) in CATS {
        let w = wide(t(key));
        SendMessageW(cat, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(cat, CB_SETCURSEL, Some(WPARAM(0)), None);
    dark_theme_combo(cat);

    // The message itself.
    ctl(
        hwnd,
        STATIC,
        t("fb_message"),
        lbl,
        m,
        msg_lbl_y,
        cw - 2 * m,
        18,
        ID_MSG_LBL,
        hinst,
    );
    let msg = ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32)
            | WS_VSCROLL
            | WS_BORDER
            | WS_TABSTOP,
        m,
        msg_y,
        cw - 2 * m,
        msg_h,
        ID_MSG,
        hinst,
    );
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar;
    // DarkMode_Explorer renders it dark (the face/text stay dark via WM_CTLCOLOREDIT).
    if crate::dark::is_dark() {
        crate::dark::dark_control(msg, w!("DarkMode_Explorer"));
    }

    // Optional reply address — muted label, because it must not read as required.
    ctl(
        hwnd,
        STATIC,
        t("fb_email_hint"),
        lbl,
        m,
        email_lbl_y,
        cw - 2 * m,
        18,
        ID_EMAIL_LBL,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        m,
        email_y,
        cw - 2 * m,
        email_h,
        ID_EMAIL,
        hinst,
    );

    // Footer: the public alternative on the left, Cancel + Send on the right.
    let link = format!("<a href=\"{}\">{}</a>", issues_url(), t("fb_github"));
    ctl(
        hwnd,
        SYSLINK,
        &link,
        WS_TABSTOP,
        m,
        btn_y + 5,
        cw - 2 * m - 2 * btn_w - 2 * gap,
        18,
        ID_GH_LINK,
        hinst,
    );
    let send_x = cw - m - btn_w;
    let cancel_x = send_x - gap - btn_w;
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        cancel_x,
        btn_y,
        btn_w,
        btn_h,
        IDCANCEL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("fb_send"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        send_x,
        btn_y,
        btn_w,
        btn_h,
        IDOK,
        hinst,
    );

    let _ = SetFocus(GetDlgItem(Some(hwnd), ID_MSG).ok());
}

/// The form body for one submission. `msg`/`contact` are already trimmed; both are
/// capped here so a single submit can't post an unbounded body.
fn build_body(cat: &str, msg: &str, contact: &str) -> String {
    let msg: String = msg.chars().take(MAX_MSG).collect();
    let contact: String = contact.chars().take(MAX_CONTACT).collect();
    // The developer's own test box (HKCU DevMachine=1) tags the request, so test
    // submissions are distinguishable from real ones. Empty on every real install.
    let dev = if sagethumbs2k_core::settings::is_dev_machine() {
        "&dev=1"
    } else {
        ""
    };
    let enc = crate::http::form_enc;
    format!(
        "cat={}&msg={}&contact={}&v={}&os={}{}",
        enc(cat),
        enc(&msg),
        enc(&contact),
        enc(env!("CARGO_PKG_VERSION")),
        enc(&crate::sponsors::os_tag()),
        dev,
    )
}

/// Is this a plausible single email address?
///
/// The reply field is email-only. It used to accept "email or other contact" and take
/// anything with an `@` in it, and what actually arrived was people's *complaints* typed
/// into the reply box — rows that can't be replied to and whose text isn't shown as the
/// message. Deliberately structural rather than RFC-exact: it rejects the junk that
/// really shows up (prose, `n/a`, bare handles) without bouncing an address someone owns.
/// Empty is still fine — the field stays optional.
///
/// Kept byte-for-byte in step with `LooksLikeEmail` in `scripts/packaging/installer.iss` (the
/// uninstall survey) and `looksLikeEmail` in `scripts/packaging/analytics/worker.js` (the server
/// gate); `tests::email_rule_matches_shared_table` locks the shared cases.
fn looks_like_email(s: &str) -> bool {
    // a@b.co is the shortest thing worth accepting; the upper bound matches the survey's
    // 120-char field, and MAX_CONTACT already caps what can be posted.
    if s.len() < 6 || s.len() > 120 {
        return false;
    }
    // One '@', and nothing outside printable ASCII or the delimiters that mean this is
    // prose/a URL rather than an address.
    let bad = |c: char| {
        c <= ' '
            || c > '~'
            || matches!(
                c,
                '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '"' | '<' | '>' | '\\' | '/'
            )
    };
    if s.chars().any(bad) {
        return false;
    }
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    if domain.contains('@') || local.is_empty() || local.len() > 64 || domain.is_empty() {
        return false;
    }
    if domain.starts_with('-') || domain.ends_with('-') {
        return false;
    }
    // The domain needs a dot that is neither first nor last, no empty labels, and a TLD of
    // two or more letters — which is what rules out `me@localhost` and `me@1`.
    let Some((host, tld)) = domain.rsplit_once('.') else {
        return false;
    };
    if host.is_empty() || host.starts_with('.') || host.contains("..") || host.ends_with('.') {
        return false;
    }
    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// A short complaint about the form, focused back on the offending field.
unsafe fn nag(hwnd: HWND, message: &str, focus_id: i32) {
    let body = wide(message);
    let cap = wide(t("fb_title"));
    MessageBoxW(
        Some(hwnd),
        PCWSTR(body.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_OK | MB_ICONINFORMATION,
    );
    let _ = SetFocus(GetDlgItem(Some(hwnd), focus_id).ok());
}

/// Validate, then POST on a worker thread. The UI stays live (the window is only
/// disabled where it matters — the Send button) and the outcome arrives as
/// [`WM_FB_DONE`].
unsafe fn on_send(hwnd: HWND) {
    if SENDING.with(|s| s.get()) {
        return;
    }
    let msg = get_edit_text(hwnd, ID_MSG).trim().to_string();
    if msg.chars().count() < 3 {
        nag(hwnd, t("fb_need_message"), ID_MSG);
        return;
    }
    // Email-only, and checked properly — a bare `contains('@')` let "discord: me#1234"
    // and whole sentences through, which is exactly what the field was filling up with.
    let contact = get_edit_text(hwnd, ID_EMAIL).trim().to_string();
    if !contact.is_empty() && !looks_like_email(&contact) {
        nag(hwnd, t("fb_bad_email"), ID_EMAIL);
        return;
    }
    let cat = CATS
        .get(combo_sel(hwnd, ID_CAT))
        .map_or("other", |&(_, wire)| wire);
    let body = build_body(cat, &msg, &contact);

    SENDING.with(|s| s.set(true));
    if let Ok(send) = GetDlgItem(Some(hwnd), IDOK) {
        let busy = wide(t("fb_sending"));
        let _ = SetWindowTextW(send, PCWSTR(busy.as_ptr()));
        let _ = EnableWindow(send, false);
    }

    // HWND isn't Send, so the raw handle value crosses the boundary and is rebuilt for
    // the (thread-safe) post. A window torn down first just makes the post a no-op.
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let ok = crate::http::request(
            "POST",
            FEEDBACK_URL,
            "Content-Type: application/x-www-form-urlencoded",
            body.as_bytes(),
            SEND_TIMEOUT_SECS,
            MAX_RESP,
        )
        .is_some_and(|r| (200..300).contains(&r.status));
        unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut c_void)),
                WM_FB_DONE,
                WPARAM(usize::from(ok)),
                LPARAM(0),
            );
        }
    });
}

/// The POST came back. Success closes the dialog with a thank-you; failure keeps the
/// text (on the clipboard as well) and offers the GitHub issue page instead.
unsafe fn on_done(hwnd: HWND, ok: bool) {
    SENDING.with(|s| s.set(false));
    if ok {
        let body = wide(t("fb_thanks"));
        let cap = wide(t("fb_title"));
        MessageBoxW(
            Some(hwnd),
            PCWSTR(body.as_ptr()),
            PCWSTR(cap.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
        let _ = DestroyWindow(hwnd);
        return;
    }

    // Restore the button so a retry is possible, and rescue the text either way.
    if let Ok(send) = GetDlgItem(Some(hwnd), IDOK) {
        let label = wide(t("fb_send"));
        let _ = SetWindowTextW(send, PCWSTR(label.as_ptr()));
        let _ = EnableWindow(send, true);
    }
    let _ = set_clipboard_text(&get_edit_text(hwnd, ID_MSG));
    let body = wide(t("fb_failed"));
    let cap = wide(t("fb_title"));
    let answer = MessageBoxW(
        Some(hwnd),
        PCWSTR(body.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_YESNO | MB_ICONWARNING,
    );
    if answer == IDYES {
        open_url(&issues_url());
    }
}

extern "system" fn feedback_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // The intro + the "optional" email note read as supporting text, not as
        // labels — muted BEFORE the generic static coloring claims them.
        if msg == WM_CTLCOLORSTATIC {
            let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
            if id == ID_HEAD || id == ID_EMAIL_LBL {
                return dark_ctlcolor_dim(wparam);
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = match GetModuleHandleW(None) {
                    Ok(h) => h.into(),
                    Err(_) => return LRESULT(-1),
                };
                SENDING.with(|s| s.set(false)); // fresh dialog, fresh state
                build(hwnd, hinst);
                LRESULT(0)
            }
            WM_FB_DONE => {
                on_done(hwnd, wparam.0 != 0);
                LRESULT(0)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                let code = (*nmhdr).code;
                if code == NM_CLICK || code == NM_RETURN {
                    let link = lparam.0 as *const NMLINK;
                    let url = wstr_to_string(&(*link).item.szUrl);
                    if !url.is_empty() {
                        open_url(&url);
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match (wparam.0 & 0xFFFF) as i32 {
                    IDOK => on_send(hwnd),
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            // No PostQuitMessage: opened as a nested modal, so `pump_until_closed`
            // exits on the window going away. Posting WM_QUIT here would leak into
            // the owner's loop and close Settings too.
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_encodes_and_caps_fields() {
        let body = build_body("bug", "it broke & burned", "me@example.com");
        assert!(body.starts_with("cat=bug&msg=it%20broke%20%26%20burned&"));
        assert!(body.contains("&contact=me%40example.com&"));
        assert!(body.contains(concat!("&v=", env!("CARGO_PKG_VERSION"))));
        assert!(body.contains("&os="));
    }

    #[test]
    fn body_truncates_an_oversized_message() {
        let long = "x".repeat(MAX_MSG * 2);
        let body = build_body("other", &long, "");
        // Only the cap survives, and an empty contact stays an empty pair (not absent,
        // so the server sees a consistent field set).
        assert!(body.contains(&format!("msg={}", "x".repeat(MAX_MSG))));
        assert!(!body.contains(&"x".repeat(MAX_MSG + 1)));
        assert!(body.contains("&contact=&"));
    }

    /// The one table all three implementations of the email rule are held to — this one,
    /// `LooksLikeEmail` in `scripts/packaging/installer.iss`, and `looksLikeEmail` in
    /// `scripts/packaging/analytics/worker.js`. It is READ from the shared fixture rather than
    /// duplicated here, so the three can't drift apart by someone editing a private copy;
    /// `scripts/check-email-rule.ps1` runs the other two against this same file.
    ///
    /// Path is relative to THIS source file (`src/bin/app/`) — moving this module to a
    /// different depth breaks it at compile time, which is the intended failure.
    const EMAIL_CASES_RAW: &str = include_str!("../../../tests/fixtures/email-rule-cases.txt");

    /// `(expected, value)` per case line; `#` comments and blank lines dropped.
    fn email_cases() -> Vec<(bool, &'static str)> {
        EMAIL_CASES_RAW
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let (want, value) = l
                    .split_once('|')
                    .unwrap_or_else(|| panic!("case line has no '|': {l:?}"));
                (want == "1", value)
            })
            .collect()
    }

    #[test]
    fn email_rule_matches_shared_table() {
        let cases = email_cases();
        // Guards the fixture itself: a truncated or unparsed file would otherwise make this
        // test pass by checking nothing at all.
        assert!(
            cases.len() >= 25,
            "shared fixture looks truncated: {} cases parsed",
            cases.len()
        );
        assert!(cases.iter().any(|&(want, _)| want));
        assert!(cases.iter().any(|&(want, _)| !want));
        for (want, input) in cases {
            assert_eq!(
                looks_like_email(input),
                want,
                "looks_like_email({input:?}) should be {want}"
            );
        }
    }

    #[test]
    fn email_rule_is_bounded_and_never_panics_on_odd_input() {
        // Length bounds, and the multi-byte input a char/byte mix-up would panic on.
        assert!(!looks_like_email(&format!(
            "{}@example.com",
            "x".repeat(65)
        )));
        assert!(!looks_like_email(&format!("me@{}.com", "x".repeat(200))));
        assert!(!looks_like_email("日本語@example.com"));
        assert!(!looks_like_email("me@例え.com"));
        assert!(!looks_like_email("\u{0}@example.com"));
        assert!(!looks_like_email("me@example.com\n"));
    }

    #[test]
    fn every_category_has_a_wire_value_and_the_default_is_suggestion() {
        assert_eq!(CATS[0].1, "suggestion");
        assert!(CATS.iter().all(|(key, wire)| {
            !key.is_empty() && !wire.is_empty() && wire.chars().all(|c| c.is_ascii_lowercase())
        }));
    }

    #[test]
    fn feedback_endpoint_and_issue_link_are_https() {
        assert!(crate::http::split_https(FEEDBACK_URL).is_some());
        assert!(crate::http::split_https(&issues_url()).is_some());
        assert!(issues_url().ends_with("/issues/new"));
    }
}
