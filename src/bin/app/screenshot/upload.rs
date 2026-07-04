//! Keyless screenshot / file upload. POSTs the image to a no-account, no-API-key
//! host and copies the returned URL to the clipboard.
//!
//! **No API key, no shared account.** Hosts like x0.at / catbox.moe accept an
//! anonymous multipart upload and rate-limit per **end-user IP** — so there's no
//! single key/account of ours to get hammered; each user's uploads are on their
//! own connection.
//!
//! **Fallback chain (2026-07):** these keyless hosts keep dying one at a time
//! (0x0.st disabled itself over AI-spam abuse; catbox.moe paused uploads over
//! storage), so a single hardcoded host is a single point of failure. We now try
//! [`builtin_hosts`] IN ORDER until one returns a URL — permanent hosts first, an
//! expiring one last, across THREE independent operators (x0.at, catbox, uguu.se)
//! so no single operator outage can take the whole chain down. Some hosts reply
//! with the bare URL, others embed it in JSON — see [`extract_url`].
//!
//! **User-editable config:** the whole chain is overridable via a plain-text file
//! `%APPDATA%\SageThumbs2K\upload-hosts.conf` (auto-created, self-documenting — the
//! path + template live in `sagethumbs2k_core::upload_config`, shared with the
//! `st2k upload-hosts` CLI) so a user can add / reorder / replace hosts, or point at
//! their own server, with no rebuild. A legacy single-host HKCU override still works
//! too. See [`upload_hosts`] for the precedence.
//!
//! When every host refuses, the failure dialog shows **what each host actually
//! said** (e.g. "catbox.moe — Uploads paused…") so the user can tell a host outage
//! ("just wait") apart from a real connection problem.
//!
//! Runs in its OWN `--upload <png>` / `--upload-keep <list>` process (spawned by the
//! toolbar's Upload button / the DLL verb) so the shell never blocks on the network.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinInet::{
    HttpOpenRequestW, HttpSendRequestW, InternetCloseHandle, InternetConnectW, InternetOpenW,
    INTERNET_FLAG_SECURE, INTERNET_SERVICE_HTTP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetSystemMetrics, MessageBoxW,
    PeekMessageW, SendMessageW, TranslateMessage, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
    MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, WINDOW_STYLE, WM_SETFONT,
    WS_BORDER, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

const HTTPS_PORT: u16 = 443;

use crate::win::{set_clipboard_text, t, wide, SS_CENTER, SS_CENTERIMAGE};

/// A resolved upload endpoint (owned, because it can come from the registry / config).
struct UploadHost {
    host: String,
    path: String,
    /// The multipart field the file goes in.
    field: String,
    /// Any extra form fields the host wants (e.g. catbox's `reqtype=fileupload`).
    extra: Vec<(String, String)>,
    /// How the host returns the link: `false` → the reply IS the bare URL (x0.at,
    /// catbox); `true` → the URL is embedded in a JSON reply (uguu.se). See [`extract_url`].
    json: bool,
}

/// The built-in keyless hosts, tried in order until one returns a URL. All are
/// no-account / no-API-key and rate-limit per end-user IP, and all reply with the
/// bare link as plain text. Ordered **permanent-first, temporary-last**, so a normal
/// upload gets a permanent link and only falls back to an expiring one when every
/// permanent host is down.
fn builtin_hosts() -> Vec<UploadHost> {
    vec![
        // x0.at — 0x0-style keyless host; plain-text URL, field `file`, no extra
        // fields. Retention scales with size (small screenshots are effectively
        // long-lived). Currently the only *up* permanent keyless host.
        UploadHost {
            host: "x0.at".into(),
            path: "/".into(),
            field: "file".into(),
            extra: vec![],
            json: false,
        },
        // catbox.moe — keyless & PERMANENT. Kept in the chain so uploads return to it
        // automatically once its storage issue is resolved; it's simply skipped (its
        // "paused" reply isn't a URL) while it's down.
        UploadHost {
            host: "catbox.moe".into(),
            path: "/user/api.php".into(),
            field: "fileToUpload".into(),
            extra: vec![("reqtype".into(), "fileupload".into())],
            json: false,
        },
        // litterbox.catbox.moe — catbox's TEMPORARY host (separate storage), 72h max.
        // Last-resort permanent-operator fallback: a working 72-hour link beats a failed upload.
        UploadHost {
            host: "litterbox.catbox.moe".into(),
            path: "/resources/internals/api.php".into(),
            field: "fileToUpload".into(),
            extra: vec![
                ("reqtype".into(), "fileupload".into()),
                ("time".into(), "72h".into()),
            ],
            json: false,
        },
        // uguu.se — a THIRD, independent operator (not x0 / not catbox), so a full
        // outage of one operator can't take the whole chain down. Keyless, ~3h temp,
        // and returns the link inside a JSON reply (`{"files":[{"url":"…"}]}`, with
        // `\/`-escaped slashes) — hence `json: true`.
        UploadHost {
            host: "uguu.se".into(),
            path: "/upload.php".into(),
            field: "files[]".into(),
            extra: vec![],
            json: true,
        },
    ]
}

/// Resolve the upload endpoint(s), in precedence order:
///
/// 1. **The config FILE** (`%APPDATA%\SageThumbs2K\upload-hosts.conf`) — when it
///    defines ≥1 host, it fully controls the chain. This is the user-facing knob.
/// 2. **The legacy HKCU single-host override** (`ScreenshotUploadUrl` /
///    `…Field` / `…Extra`) — kept for back-compat.
/// 3. **The [`builtin_hosts`] fallback chain** — the shipped default.
///
/// A user-configured host (file or registry) is **authoritative**: we use ONLY what
/// they chose and do NOT fall through to the built-ins, so a file is never sent to a
/// host they didn't pick (privacy).
///
/// Returns `Err(message)` for a misconfigured registry URL: the POST always runs over
/// TLS (port 443 + `INTERNET_FLAG_SECURE`), so an `http://` or scheme-less override
/// can't be honored as written — we reject it with a clear message instead of silently
/// treating it as HTTPS or uploading to a different host than configured. (Bad *file*
/// lines are just skipped — a file can list many hosts, so one typo shouldn't abort.)
fn upload_hosts() -> Result<Vec<UploadHost>, String> {
    // Always make sure the self-documenting config file exists (all-commented =
    // "use the built-in defaults"), so it's there to find and edit. Path + template
    // live in the shared core module so the `st2k` CLI resolves the SAME file.
    let cfg = sagethumbs2k_core::upload_config::ensure_config();

    // 1) The config file wins when it defines any host.
    if let Some(path) = cfg {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let hosts = parse_hosts_config(&text);
            if !hosts.is_empty() {
                return Ok(hosts);
            }
        }
    }

    // 2) Legacy single-host registry override.
    if let Ok(key) = windows_registry::CURRENT_USER.open(sagethumbs2k_core::settings::ROOT) {
        if let Ok(raw) = key.get_string("ScreenshotUploadUrl") {
            let url = raw.trim().to_string();
            if !url.is_empty() {
                let Some(rest) = url.strip_prefix("https://") else {
                    return Err(format!(
                        "Custom screenshot upload host must start with https:// (uploads always use TLS).\n\n\
                         Got: {url}\n\nFix it in HKCU\\Software\\SageThumbs2K\\ScreenshotUploadUrl \
                         (or use the upload-hosts config file)."
                    ));
                };
                let (host, path) = match rest.find('/') {
                    Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
                    None => (rest.to_string(), "/".to_string()),
                };
                let field = key
                    .get_string("ScreenshotUploadField")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "file".into());
                let extra = key
                    .get_string("ScreenshotUploadExtra")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .and_then(|kv| kv.split_once('=').map(|(k, v)| vec![(k.to_string(), v.to_string())]))
                    .unwrap_or_default();
                return Ok(vec![UploadHost { host, path, field, extra, json: false }]);
            }
        }
    }

    // 3) Built-in fallback chain.
    Ok(builtin_hosts())
}

/// Ensure the config exists, then open it in the user's default text editor. Wired to
/// the Settings ▸ Screenshots "Edit upload hosts…" button. (Path + template come from
/// the shared `sagethumbs2k_core::upload_config` module — the `st2k` CLI opens the
/// same file.)
pub(crate) unsafe fn open_hosts_config() {
    let Some(path) = sagethumbs2k_core::upload_config::ensure_config() else { return };
    // If we couldn't create the file for some reason, open its folder instead.
    let target = if path.exists() {
        path.display().to_string()
    } else {
        path.parent().map(|d| d.display().to_string()).unwrap_or_default()
    };
    if target.is_empty() {
        return;
    }
    let file = wide(&target);
    let verb = wide("open");
    ShellExecuteW(
        None,
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Parse the config file into hosts. One host per non-blank, non-`#` line:
/// `https-url | field | response | extra=val | extra2=val …`
/// where `response` is `text` (the reply IS the URL; the default) or `json` (the URL
/// is embedded in a JSON reply). Malformed lines / non-`https://` URLs are skipped.
fn parse_hosts_config(text: &str) -> Vec<UploadHost> {
    let mut hosts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('|').map(str::trim);
        let Some(url) = parts.next() else { continue };
        let Some(rest) = url.strip_prefix("https://") else { continue }; // TLS-only
        let (host, path) = match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        };
        if host.is_empty() {
            continue;
        }
        let field = parts.next().filter(|s| !s.is_empty()).unwrap_or("file").to_string();
        let json = parts.next().map(|s| s.eq_ignore_ascii_case("json")).unwrap_or(false);
        let extra = parts
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        hosts.push(UploadHost { host, path, field, extra, json });
    }
    hosts
}

const MAX_RESP: usize = 64 * 1024; // a URL response is tiny; cap to be safe

/// Caption for the screenshot-upload completion dialogs.
fn shot_caption() -> &'static str {
    t("up_caption_shot")
}
/// Caption for the right-click "Upload" verb's completion dialogs.
fn file_caption() -> &'static str {
    t("up_caption_file")
}

/// A tiny topmost "Uploading…" pill (bottom-center of the primary monitor) shown while
/// `work` runs on a worker thread — the overlay/menu that launched us is already gone by
/// then, so without it the user stares at NOTHING for the seconds (and up to three host
/// retries) an upload takes, and reasonably assumes it silently failed. This thread pumps
/// messages so the pill actually paints; the pill is non-activating and owns no input.
unsafe fn with_busy_pill<T: Send + 'static>(
    text: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let (w, h) = (300, 40);
    let txt = wide(text);
    let pill = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        windows::core::w!("STATIC"),
        PCWSTR(txt.as_ptr()),
        WS_POPUP | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(SS_CENTER | SS_CENTERIMAGE),
        (sw - w) / 2,
        sh - h - 90, // above the taskbar area, bottom-center
        w,
        h,
        None,
        None,
        None,
        None,
    )
    .ok();
    if let Some(p) = pill {
        SendMessageW(
            p,
            WM_SETFONT,
            Some(windows::Win32::Foundation::WPARAM(crate::win::gui_font().0 as usize)),
            Some(windows::Win32::Foundation::LPARAM(1)),
        );
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut msg = MSG::default();
    let result = loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        match rx.recv_timeout(std::time::Duration::from_millis(30)) {
            Ok(v) => break v,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // Unreachable under panic=abort (a worker panic kills the process), but
            // don't hang the pill forever if it somehow happens.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::process::exit(1),
        }
    };
    if let Some(p) = pill {
        let _ = DestroyWindow(p);
    }
    result
}

/// Upload `path` (a throwaway capture PNG), copy the resulting URL to the clipboard,
/// tell the user, then DELETE the temp file. Spawned by the capture overlay's Upload
/// button via `--upload <png>`.
pub(crate) unsafe fn run_upload(path: &str) {
    // Resolve (and validate) the endpoint(s) first, so a misconfigured custom host
    // gives a specific message instead of a generic "couldn't upload".
    let hosts = match upload_hosts() {
        Ok(h) => h,
        Err(msg) => {
            let _ = std::fs::remove_file(path);
            notify(&msg, shot_caption(), true);
            return;
        }
    };
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    let result = with_busy_pill(t("up_busy_one"), move || match bytes {
        // SAFETY: upload_any only touches WinInet handles it creates + closes itself,
        // so running it on the pill's worker thread is fine.
        Ok(b) => unsafe { upload_any(&b, "screenshot.png", &hosts) },
        Err(e) => Err(format!("couldn't read the capture — {e}")),
    });
    match result {
        Ok(u) => {
            let _ = set_clipboard_text(&u);
            crate::upload_result::show_upload_result(t("up_done_one"), &u);
        }
        Err(reasons) => {
            notify(&upload_failed_msg(t("up_what_screenshot"), &reasons), shot_caption(), true)
        }
    }
}

/// Upload the USER files listed (one path per line) in `list_path` — the right-click
/// "Upload" verb's path — copy the resulting URL(s) to the clipboard (one per line),
/// and report. Unlike [`run_upload`], these are the user's own files and are **never
/// deleted**; only the temporary list file is removed. Spawned by the DLL verb via
/// `--upload-keep <list>`.
pub(crate) unsafe fn run_upload_keep(list_path: &str) {
    let hosts = match upload_hosts() {
        Ok(h) => h,
        Err(msg) => {
            let _ = std::fs::remove_file(list_path);
            notify(&msg, file_caption(), true);
            return;
        }
    };
    // The DLL writes the selection CRLF-joined; tolerate either ending, drop blanks.
    let files: Vec<String> = std::fs::read_to_string(list_path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_file(list_path); // the list is ours; the images are NOT
    if files.is_empty() {
        return;
    }
    let total = files.len();
    // Upload each file under its real name so the host keeps the extension (the
    // returned link then stays viewable in a browser). Remember the last failure
    // reason so an all-fail run can show WHY (host paused vs. no connection). The
    // whole batch runs behind the "Uploading…" pill — multi-file menu uploads can
    // take a while and previously gave zero sign anything was happening.
    let busy = if total == 1 {
        t("up_busy_one").to_string()
    } else {
        t("up_busy_many").replace("{n}", &total.to_string())
    };
    let (urls, last_reason) = with_busy_pill(&busy, move || {
        let mut urls: Vec<String> = Vec::new();
        let mut last_reason: Option<String> = None;
        for f in &files {
            let name = std::path::Path::new(f)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("upload");
            match std::fs::read(f) {
                // SAFETY: upload_any only touches WinInet handles it creates + closes
                // itself, so running it on the pill's worker thread is fine.
                Ok(bytes) => match unsafe { upload_any(&bytes, name, &hosts) } {
                    Ok(u) => urls.push(u),
                    Err(why) => last_reason = Some(why),
                },
                Err(e) => last_reason = Some(format!("couldn't read {name} — {e}")),
            }
        }
        (urls, last_reason)
    });
    if urls.is_empty() {
        let reasons = last_reason.unwrap_or_else(|| "no readable files".to_string());
        let what = if total == 1 { t("up_what_file") } else { t("up_what_any_files") };
        notify(&upload_failed_msg(what, &reasons), file_caption(), true);
        return;
    }
    let joined = urls.join("\r\n");
    let _ = set_clipboard_text(&joined);
    let heading = if total == 1 {
        t("up_done_one").to_string()
    } else if urls.len() == total {
        t("up_done_all").replace("{total}", &total.to_string())
    } else {
        t("up_done_partial")
            .replace("{ok}", &urls.len().to_string())
            .replace("{total}", &total.to_string())
            .replace("{failed}", &(total - urls.len()).to_string())
    };
    crate::upload_result::show_upload_result(&heading, &joined);
}

/// Body for the "couldn't upload" dialog. Includes what each host actually said, so a
/// host outage ("just wait") is distinguishable from a real connection problem.
fn upload_failed_msg(what: &str, reasons: &str) -> String {
    let cfg = sagethumbs2k_core::upload_config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "%APPDATA%\\SageThumbs2K\\upload-hosts.conf".to_string());
    t("up_failed").replace("{what}", what).replace("{reasons}", reasons).replace("{cfg}", &cfg)
}

/// A simple completion message (the upload process has no window of its own).
unsafe fn notify(msg: &str, caption: &str, error: bool) {
    let body = wide(msg);
    let cap = wide(caption);
    let icon = if error { MB_ICONWARNING } else { MB_ICONINFORMATION };
    MessageBoxW(None, PCWSTR(body.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | icon);
}

/// Try each host in order; return the first URL, or — if all fail — a multi-line
/// summary of what each host said (`host — reason`), one per line.
unsafe fn upload_any(bytes: &[u8], filename: &str, hosts: &[UploadHost]) -> Result<String, String> {
    let mut reasons: Vec<String> = Vec::new();
    for h in hosts {
        match upload_one(bytes, filename, h) {
            Ok(url) => return Ok(url),
            Err(why) => reasons.push(format!("{} — {}", h.host, why)),
        }
    }
    Err(reasons.join("\n"))
}

/// Build the multipart body and POST it to ONE host; return the response URL on
/// success, or the host's own reason on failure (its response text, first line,
/// clipped — surfaced to the user so an outage is visible). `filename` goes in the
/// Content-Disposition so the host preserves the file's extension (catbox keys the
/// returned URL off it — a `.jpg` stays viewable).
unsafe fn upload_one(bytes: &[u8], filename: &str, h: &UploadHost) -> Result<String, String> {
    let boundary = "----st2kBoundary8x9f2aQ1z";
    let mut body: Vec<u8> = Vec::new();
    for (name, val) in &h.extra {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            h.field
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let headers = format!("Content-Type: multipart/form-data; boundary={boundary}");
    let resp = match post(&h.host, &h.path, &headers, &body) {
        Some(r) => r,
        None => return Err("no response (no connection?)".to_string()),
    };
    let text = String::from_utf8_lossy(&resp);
    match extract_url(&text, h.json) {
        Some(url) => Ok(url),
        // No link in the reply (an HTML error page, a "paused" notice, …) — surface
        // the host's own words so an outage is visible.
        None => Err(short_reason(&text)),
    }
}

/// Pull the upload link out of a host's reply. Plain hosts (`json == false`) return
/// the bare URL as the whole body; JSON hosts embed it (often with `\/`-escaped
/// slashes). Returns None when there's no usable link (an error page / "paused"
/// notice), so the caller can surface the host's reason instead.
fn extract_url(body: &str, json: bool) -> Option<String> {
    let t = body.trim();
    if !json {
        // Plain reply: the whole (trimmed) body must BE a single URL token.
        return (t.starts_with("http") && t.len() < 2048 && !t.contains(char::is_whitespace))
            .then(|| t.to_string());
    }
    // JSON reply: take the first embedded http(s) URL, un-escaping `\/`.
    let start = t.find("http")?;
    let rest: Vec<char> = t[start..].chars().collect();
    let mut url = String::new();
    let mut i = 0;
    while i < rest.len() {
        let c = rest[i];
        if c == '\\' {
            // Inside a JSON string only `\/` is meaningful in a URL; any other escape
            // (or a bare `\`) ends it.
            if rest.get(i + 1) == Some(&'/') {
                url.push('/');
                i += 2;
                continue;
            }
            break;
        }
        if c == '"' || c == '\'' || c.is_whitespace() || matches!(c, '<' | '>' | ',' | '}' | ']' | ')') {
            break;
        }
        url.push(c);
        i += 1;
    }
    (url.starts_with("http") && url.len() >= 12 && url.len() < 2048).then_some(url)
}

/// Condense a host's response into one short line for the failure dialog.
fn short_reason(body: &str) -> String {
    let first = body.trim().lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "empty or unreadable response".to_string();
    }
    let clipped: String = first.chars().take(180).collect();
    if clipped.len() < first.len() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// A minimal WinInet HTTPS POST (mirrors `sponsors.rs::http_fetch`, but with a body).
unsafe fn post(host: &str, path: &str, headers: &str, body: &[u8]) -> Option<Vec<u8>> {
    let agent = wide("SageThumbs2K");
    let session = InternetOpenW(PCWSTR(agent.as_ptr()), 0, PCWSTR::null(), PCWSTR::null(), 0);
    if session.is_null() {
        return None;
    }
    let host_w = wide(host);
    let conn = InternetConnectW(
        session,
        PCWSTR(host_w.as_ptr()),
        HTTPS_PORT,
        PCWSTR::null(),
        PCWSTR::null(),
        INTERNET_SERVICE_HTTP,
        0,
        None,
    );
    if conn.is_null() {
        let _ = InternetCloseHandle(session);
        return None;
    }
    let verb = wide("POST");
    let path_w = wide(path);
    let req = HttpOpenRequestW(
        conn,
        PCWSTR(verb.as_ptr()),
        PCWSTR(path_w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        None,
        INTERNET_FLAG_SECURE,
        None,
    );
    if req.is_null() {
        let _ = InternetCloseHandle(conn);
        let _ = InternetCloseHandle(session);
        return None;
    }
    let hdr_w = wide(headers);
    let sent = HttpSendRequestW(
        req,
        Some(&hdr_w[..hdr_w.len().saturating_sub(1)]),
        Some(body.as_ptr() as *const c_void),
        body.len() as u32,
    )
    .is_ok();

    // Drain via the shared helper, which caps the body and returns None on over-cap
    // (the old inline loop here returned the TRUNCATED body — a corrupt URL).
    let out = if sent { crate::win::wininet_drain(req, MAX_RESP) } else { None };
    let _ = InternetCloseHandle(req);
    let _ = InternetCloseHandle(conn);
    let _ = InternetCloseHandle(session);
    out
}
