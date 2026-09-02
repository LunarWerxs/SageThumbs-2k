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
    HttpOpenRequestW, HttpQueryInfoW, HttpSendRequestW, InternetCloseHandle, InternetConnectW,
    InternetOpenW, InternetSetOptionW, HTTP_QUERY_FLAG_NUMBER, HTTP_QUERY_STATUS_CODE,
    INTERNET_FLAG_SECURE, INTERNET_OPTION_CONNECT_TIMEOUT, INTERNET_OPTION_RECEIVE_TIMEOUT,
    INTERNET_OPTION_SEND_TIMEOUT, INTERNET_SERVICE_HTTP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetSystemMetrics, MessageBoxW, PeekMessageW,
    SendMessageW, TranslateMessage, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MSG, PM_REMOVE,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, WINDOW_STYLE, WM_SETFONT, WS_BORDER, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
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
/// no-account / no-API-key and rate-limit per end-user IP; ordered
/// **permanent-first, temporary-last**, so a normal upload gets a permanent link and
/// only falls back to an expiring one when every permanent host is down.
///
/// Built from [`sagethumbs2k_core::upload_config::BUILTIN_HOSTS`] rather than its
/// own hardcoded list, so this chain and the config template's "current built-in
/// defaults" comment can never drift apart — x0.at (currently the only *up* permanent
/// keyless host), catbox.moe (kept in the chain so uploads return to it automatically
/// once its storage issue clears; its "paused" reply just isn't a URL while it's down),
/// litterbox.catbox.moe (catbox's separate-storage 72h TEMPORARY host, the last-resort
/// permanent-operator fallback), and uguu.se (a THIRD, independent operator, ~3h temp,
/// JSON reply — `{"files":[{"url":"…"}]}` with `\/`-escaped slashes).
fn builtin_hosts() -> Vec<UploadHost> {
    sagethumbs2k_core::upload_config::BUILTIN_HOSTS
        .iter()
        .map(|&(host, path, field, extra, json)| UploadHost {
            host: host.into(),
            path: path.into(),
            field: field.into(),
            extra: extra.iter().map(|&(k, v)| (k.into(), v.into())).collect(),
            json,
        })
        .collect()
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

    // 2) Legacy single-host override. Routed through settings::get_string_opt (not a
    // direct CURRENT_USER open) so a portable install (marker-INI backend) reads the
    // same value it can actually set — opening the registry here would silently miss
    // a portable override and could pick up stale machine-registry state instead.
    if let Some(raw) = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadUrl") {
        let url = raw.trim().to_string();
        if !url.is_empty() {
            let Some((host, path)) = crate::http::split_https(&url) else {
                return Err(format!(
                    "Custom screenshot upload host must be a valid https:// URL on port 443 \
                     (uploads always use TLS).\n\n\
                     Got: {url}\n\nFix it in HKCU\\Software\\SageThumbs2K\\ScreenshotUploadUrl \
                     (or use the upload-hosts config file)."
                ));
            };
            let field = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadField")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "file".into());
            let extra = sagethumbs2k_core::settings::get_string_opt("ScreenshotUploadExtra")
                .filter(|s| !s.is_empty())
                .and_then(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| vec![(k.to_string(), v.to_string())])
                })
                .unwrap_or_default();
            return Ok(vec![UploadHost {
                host,
                path,
                field,
                extra,
                json: false,
            }]);
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
    let Some(path) = sagethumbs2k_core::upload_config::ensure_config() else {
        return;
    };
    // If we couldn't create the file for some reason, open its folder instead.
    let target = if path.exists() {
        path.display().to_string()
    } else {
        path.parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default()
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
        let Some((host, path)) = crate::http::split_https(url) else {
            continue;
        };
        let field = parts
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("file")
            .to_string();
        let json = parts
            .next()
            .map(|s| s.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        let extra = parts
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        hosts.push(UploadHost {
            host,
            path,
            field,
            extra,
            json,
        });
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
///
/// `pub(crate)` because the OCR helper (`crate::ocr_result`) has the identical problem:
/// it is a fresh process with no window of its own while the WinRT engine spins up.
pub(crate) unsafe fn with_busy_pill<T: Send + 'static>(
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
            Some(windows::Win32::Foundation::WPARAM(
                crate::win::gui_font().0 as usize,
            )),
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
    // The temp file is gone the moment we read it (the pill's worker thread can run
    // long enough — up to three host retries — that leaving it around risks a second
    // process racing the same path). Keep the bytes themselves alive past the upload
    // attempt so a total failure below can still recover the capture instead of
    // reporting it lost with nothing left to show for it.
    let recovery_bytes = bytes.as_ref().ok().cloned();
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
            let base = upload_failed_msg(t("up_what_screenshot"), &reasons);
            // Every host failed and the temp capture is already deleted — write the
            // in-memory bytes back out so the shot isn't gone for good, and say where.
            let msg = match recovery_bytes.and_then(|b| save_recovery_copy(&b)) {
                Some(p) => format!("{base}\n\nSaved a copy to:\n{}", p.display()),
                None => base,
            };
            notify(&msg, shot_caption(), true);
        }
    }
}

/// Write `bytes` (a whole PNG) into `dir` under the standard timestamped capture
/// name. Split out from [`save_recovery_copy`] so the write itself is testable
/// without touching the registry-backed save-folder setting.
///
/// Routes through [`super::output::unique_name_in`] rather than writing
/// `dir.join(name)` directly: `timestamped_name` only has 1-second resolution, so two
/// failed-upload recoveries (or a recovery landing in the same second as an ordinary
/// Ctrl+S capture) into the same folder would otherwise silently overwrite each other —
/// in exactly the path whose whole purpose is not losing the shot.
fn write_recovery_copy(dir: &std::path::Path, bytes: &[u8]) -> Option<std::path::PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let name = unsafe { super::output::timestamped_name() };
    let path = super::output::unique_name_in(dir, &name);
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// Recover a failed upload's bytes to the user's normal capture save location (their
/// configured folder, or Desktop) — the same place a manual Ctrl+S would have gone.
fn save_recovery_copy(bytes: &[u8]) -> Option<std::path::PathBuf> {
    write_recovery_copy(
        &std::path::PathBuf::from(super::effective_save_dir()),
        bytes,
    )
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
        let what = if total == 1 {
            t("up_what_file")
        } else {
            t("up_what_any_files")
        };
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
    t("up_failed")
        .replace("{what}", what)
        .replace("{reasons}", reasons)
        .replace("{cfg}", &cfg)
}

/// A simple completion message (the upload process has no window of its own).
unsafe fn notify(msg: &str, caption: &str, error: bool) {
    let body = wide(msg);
    let cap = wide(caption);
    let icon = if error {
        MB_ICONWARNING
    } else {
        MB_ICONINFORMATION
    };
    MessageBoxW(
        None,
        PCWSTR(body.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_OK | icon,
    );
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

/// Sanitize a value going inside a multipart `Content-Disposition` quoted string — a
/// filename or a config-supplied field name/value. RFC 6266's rule is
/// backslash-escape `"` and `\`; CR/LF can't be escaped at all (a raw one would terminate
/// the header line, letting the rest of the "line" be read as extra header/part-boundary
/// content), so those are replaced with a space rather than passed through. NTFS itself
/// refuses `"` and control characters in a filename through the ordinary Win32 API, but the
/// NT native API, a WSL mount, or a non-Windows SMB server can all create one that carries
/// them anyway — and that filename reaches here unchanged (`upload_files`/`run_upload`
/// take it straight from `Path::file_name`).
fn mime_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' | '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Build the multipart body and POST it to ONE host; return the response URL on
/// success, or the host's own reason on failure (its response text, first line,
/// clipped — surfaced to the user so an outage is visible). `filename` goes in the
/// Content-Disposition so the host preserves the file's extension (catbox keys the
/// returned URL off it — a `.jpg` stays viewable).
unsafe fn upload_one(bytes: &[u8], filename: &str, h: &UploadHost) -> Result<String, String> {
    let boundary = "----st2kBoundary8x9f2aQ1z";
    let filename = mime_escape(filename);
    let mut body: Vec<u8> = Vec::new();
    for (name, val) in &h.extra {
        let (name, val) = (mime_escape(name), mime_escape(val));
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{val}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            mime_escape(&h.field)
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
    interpret_response(resp.status, &resp.body, h.json)
}

/// Decide whether a completed response is a real upload, from the STATUS first. A
/// host's 4xx/5xx error page can still contain something that looks like a URL (an
/// ad link, a docs link, …), so the body is only scraped for one once the status is
/// confirmed 2xx — a status we couldn't even query (0) counts as failure too, same
/// as any other non-2xx.
fn interpret_response(status: u16, body: &[u8], json: bool) -> Result<String, String> {
    let text = String::from_utf8_lossy(body);
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} — {}", short_reason(&text)));
    }
    match extract_url(&text, json) {
        Some(url) => Ok(url),
        // 2xx but no link in the reply (a "paused" notice, an unexpected body, …) —
        // surface the host's own words so an outage is visible.
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
        return (is_http_url(t) && t.len() < 2048 && !t.contains(char::is_whitespace))
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
        if c == '"'
            || c == '\''
            || c.is_whitespace()
            || matches!(c, '<' | '>' | ',' | '}' | ']' | ')')
        {
            break;
        }
        url.push(c);
        i += 1;
    }
    (is_http_url(&url) && url.len() >= 12 && url.len() < 2048).then_some(url)
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
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

/// A POST response: the HTTP status (so the caller can require 2xx before trusting
/// the body) plus the capped body itself.
struct PostResp {
    status: u16,
    body: Vec<u8>,
}

/// Overall wall-clock budget for draining a response body (G218/C18): WinINet's own
/// per-read receive timeout resets on every partial read, so a host that trickles the
/// reply one byte at a time never trips it and can hang the "Uploading…" pill (and block
/// `upload_any` from ever falling through to the next configured host) indefinitely. This
/// matches the 20 s already set on the connect/send/receive `InternetSetOptionW` calls
/// below — well past a slow but working upload, well short of "did it freeze?".
const DRAIN_DEADLINE_SECS: u64 = 20;

/// A minimal WinInet HTTPS POST (mirrors `sponsors.rs::http_fetch`, but with a body).
unsafe fn post(host: &str, path: &str, headers: &str, body: &[u8]) -> Option<PostResp> {
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
    // Explicit timeouts. Without them a stalled host runs out WinInet's generous defaults while
    // the "Uploading…" pill sits there with nothing to cancel it — and `upload_any` can't fall
    // through to the NEXT configured host until this one gives up. 20 s is well past a slow but
    // working upload and well short of "did it freeze?".
    for opt in [
        INTERNET_OPTION_CONNECT_TIMEOUT,
        INTERNET_OPTION_SEND_TIMEOUT,
        INTERNET_OPTION_RECEIVE_TIMEOUT,
    ] {
        let ms: u32 = 20_000;
        let _ = InternetSetOptionW(
            Some(req),
            opt,
            Some(&ms as *const u32 as *const c_void),
            size_of::<u32>() as u32,
        );
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
    // (the old inline loop here returned the TRUNCATED body — a corrupt URL). Read the
    // status BEFORE draining (HttpQueryInfoW wants it off the still-open request) so a
    // 4xx/5xx page can never be scraped for a URL as if it were a success.
    let resp = if sent {
        let status = query_status(req).unwrap_or(0);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(DRAIN_DEADLINE_SECS);
        crate::win::wininet_drain(req, MAX_RESP, Some(deadline), None)
            .map(|body| PostResp { status, body })
    } else {
        None
    };
    let _ = InternetCloseHandle(req);
    let _ = InternetCloseHandle(conn);
    let _ = InternetCloseHandle(session);
    resp
}

/// Read the numeric HTTP status off a completed request (mirrors `http.rs::query_status`).
unsafe fn query_status(req: *mut c_void) -> Option<u16> {
    let mut code: u32 = 0;
    let mut len: u32 = size_of::<u32>() as u32;
    HttpQueryInfoW(
        req,
        HTTP_QUERY_STATUS_CODE | HTTP_QUERY_FLAG_NUMBER,
        Some(&mut code as *mut u32 as *mut c_void),
        &mut len,
        None,
    )
    .ok()?;
    Some(code as u16)
}

#[cfg(test)]
mod tests {
    use super::{
        extract_url, interpret_response, mime_escape, parse_hosts_config, write_recovery_copy,
    };

    #[test]
    fn upload_response_requires_an_exact_web_scheme() {
        assert_eq!(
            extract_url("https://files.example.test/a.png", false).as_deref(),
            Some("https://files.example.test/a.png")
        );
        assert_eq!(extract_url("httpx://files.example.test/a.png", false), None);
        assert_eq!(
            extract_url("javascript:https://files.example.test/a.png", false),
            None
        );
        assert_eq!(
            extract_url("https://files.example.test/a b.png", false),
            None
        );
        assert_eq!(
            extract_url(r#"{"url":"https:\/\/files.example.test\/a.png"}"#, true).as_deref(),
            Some("https://files.example.test/a.png")
        );
    }

    #[test]
    fn upload_config_rejects_ambiguous_https_authorities() {
        let text = "\
https://good.example/upload | file | text
https://user@bad.example/upload | file | text
https://bad.example:8443/upload | file | text
https://bad example/upload | file | text
";
        let hosts = parse_hosts_config(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "good.example");
        assert_eq!(hosts[0].path, "/upload");
    }

    #[test]
    fn non_2xx_status_is_rejected_even_when_the_body_contains_a_url() {
        // A 4xx/5xx error page that happens to embed something URL-shaped (an ad
        // link, a status-page link, …) must never be reported to the user as their
        // own upload link — the status has to gate the body scrape, not the body alone.
        let body = b"<html>502 Bad Gateway. See https://status.example.test/incident/1</html>";
        let err = interpret_response(502, body, false).unwrap_err();
        assert!(
            err.contains("502"),
            "failure reason should surface the status code, got: {err}"
        );
    }

    /// A filename or extra-field value carrying a `"` or an embedded CR/LF must
    /// not be able to break out of its `Content-Disposition` quoted string or inject a raw
    /// header/multipart-boundary line into the body. NTFS refuses these through the ordinary
    /// Win32 API, but the NT native API, a WSL mount, or a non-Windows SMB share can all
    /// create a filename that carries them, and it reaches `upload_one` unchanged.
    #[test]
    fn mime_escape_neutralizes_quotes_and_line_breaks() {
        assert_eq!(mime_escape("plain.png"), "plain.png");

        let injected = "evil\".png\r\nContent-Disposition: form-data; name=\"x";
        let escaped = mime_escape(injected);
        assert!(
            !escaped.contains('\r') && !escaped.contains('\n'),
            "no CR/LF may survive — a raw one would let extra header/boundary lines through"
        );
        assert!(
            escaped.contains("evil\\\".png") && escaped.contains("name=\\\"x"),
            "the quote must survive, but only in escaped (backslash-preceded) form: {escaped}"
        );

        let escaped = mime_escape("a\\b\"c\r\nd");
        assert!(!escaped.contains('\r') && !escaped.contains('\n'));
        // Every remaining `"` must be preceded by a backslash (properly escaped), and every
        // backslash must itself have been doubled — otherwise a `\"` sequence produced by
        // escaping could be misread as an unescaped quote by the receiving parser.
        let mut chars = escaped.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                assert!(
                    matches!(chars.next(), Some('\\') | Some('"')),
                    "a lone backslash must not appear unescaped"
                );
            } else {
                assert_ne!(c, '"', "an unescaped quote must never survive");
            }
        }
    }

    #[test]
    fn a_2xx_plain_reply_still_extracts_the_url() {
        assert_eq!(
            interpret_response(200, b"https://files.example.test/a.png", false).as_deref(),
            Ok("https://files.example.test/a.png")
        );
    }

    #[test]
    fn an_unqueryable_status_is_treated_as_failure_not_success() {
        // query_status returns None (mapped to 0 by the caller) on a request WinInet
        // couldn't report a status for — that must not be silently treated as OK.
        let err = interpret_response(0, b"https://files.example.test/a.png", false).unwrap_err();
        assert!(err.contains('0'));
    }

    #[test]
    fn upload_failure_recovery_writes_the_bytes_back_to_disk() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_upload_recovery_test_{}_{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let bytes = b"not a real png, just proving the bytes survive";

        let path = write_recovery_copy(&dir, bytes).expect("recovery write should succeed");

        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two recovery copies landing in the same second (the real trigger — a batch
    /// upload where more than one file fails, or a recovery racing an ordinary Ctrl+S save)
    /// used to write `dir.join(timestamped_name())` directly, so the second write silently
    /// clobbered the first. Routing through `output::unique_name_in` must give the second
    /// call its own path and leave the first file's bytes intact.
    #[test]
    fn same_second_recovery_copies_do_not_clobber_each_other() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_upload_recovery_collision_{}_{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let first = b"first failed upload's bytes";
        let second = b"a second, different, failed upload's bytes";

        let p1 = write_recovery_copy(&dir, first).expect("first recovery write should succeed");
        let p2 = write_recovery_copy(&dir, second).expect("second recovery write should succeed");

        assert_ne!(p1, p2, "the second write must not collide with the first");
        assert_eq!(
            std::fs::read(&p1).unwrap(),
            first,
            "the first file must survive untouched"
        );
        assert_eq!(std::fs::read(&p2).unwrap(), second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
