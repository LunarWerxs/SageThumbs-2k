//! Keyless screenshot upload. POSTs the captured PNG to a no-account, no-API-key
//! host and copies the returned URL to the clipboard.
//!
//! **No API key, no shared account.** Hosts like 0x0.st / catbox.moe accept an
//! anonymous multipart upload and rate-limit per **end-user IP** — so there's no
//! single key/account of ours to get hammered; each user's uploads are on their
//! own connection. Switch hosts by changing the one `HOST` line below.
//!
//! Runs in its OWN `--upload <png>` process (spawned by the toolbar's Upload
//! button) so the capture overlay never blocks on the network.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinInet::{
    HttpOpenRequestW, HttpSendRequestW, InternetCloseHandle, InternetConnectW, InternetOpenW,
    INTERNET_FLAG_SECURE, INTERNET_SERVICE_HTTP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK,
};

const HTTPS_PORT: u16 = 443;

use crate::win::{set_clipboard_text, wide};

/// A resolved upload endpoint (owned, because it can come from the registry).
struct UploadHost {
    host: String,
    path: String,
    /// The multipart field the file goes in.
    field: String,
    /// Any extra form fields the host wants (e.g. catbox's `reqtype=fileupload`).
    extra: Vec<(String, String)>,
}

/// Resolve the upload endpoint. Defaults to **catbox.moe** (keyless, no account,
/// permanent, per-IP), but is **overridable via HKCU** so a dead host can be
/// swapped with no rebuild, and so a user can point it at their own server / a
/// paid host they control (the only truly multi-year-stable option):
///
/// - `ScreenshotUploadUrl`   — full POST URL, MUST be `https://your.host/upload`
/// - `ScreenshotUploadField` — the file form-field name (default `file`)
/// - `ScreenshotUploadExtra` — one extra `key=value` form field (optional)
///
/// (0x0.st is the same keyless idea but currently has uploads disabled — AI-spam
/// abuse — so catbox is the default.)
///
/// Returns `Err(message)` for a misconfigured custom URL: the POST always runs over
/// TLS (port 443 + `INTERNET_FLAG_SECURE`), so an `http://` or scheme-less override
/// can't be honored as written. We reject it with a clear message INSTEAD of silently
/// treating it as HTTPS (the old behavior — it failed with a generic error) AND
/// instead of falling back to catbox (which would upload the user's screenshot to a
/// different host than they configured).
fn upload_host() -> Result<UploadHost, String> {
    let catbox = || UploadHost {
        host: "catbox.moe".into(),
        path: "/user/api.php".into(),
        field: "fileToUpload".into(),
        extra: vec![("reqtype".into(), "fileupload".into())],
    };
    let Ok(key) = windows_registry::CURRENT_USER.open(sagethumbs2k_core::settings::ROOT) else {
        return Ok(catbox());
    };
    let url = match key.get_string("ScreenshotUploadUrl") {
        Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => return Ok(catbox()),
    };
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!(
            "Custom screenshot upload host must start with https:// (uploads always use TLS).\n\n\
             Got: {url}\n\nFix HKCU\\Software\\SageThumbs2K\\ScreenshotUploadUrl."
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
    Ok(UploadHost { host, path, field, extra })
}

const MAX_RESP: usize = 64 * 1024; // a URL response is tiny; cap to be safe

/// Caption for the screenshot-upload completion dialogs.
const SHOT_CAPTION: &str = "SageThumbs 2K — Screenshot";
/// Caption for the right-click "Upload" verb's completion dialogs.
const FILE_CAPTION: &str = "SageThumbs 2K — Upload";

/// Upload `path` (a throwaway capture PNG), copy the resulting URL to the clipboard,
/// tell the user, then DELETE the temp file. Spawned by the capture overlay's Upload
/// button via `--upload <png>`.
pub(crate) unsafe fn run_upload(path: &str) {
    // Resolve (and validate) the endpoint first, so a misconfigured custom host
    // gives a specific message instead of a generic "couldn't upload".
    let host = match upload_host() {
        Ok(h) => h,
        Err(msg) => {
            let _ = std::fs::remove_file(path);
            notify(&msg, SHOT_CAPTION, true);
            return;
        }
    };
    let url = std::fs::read(path).ok().and_then(|bytes| upload(&bytes, "screenshot.png", &host));
    let _ = std::fs::remove_file(path);
    match url {
        Some(u) => {
            let _ = set_clipboard_text(&u);
            crate::upload_result::show_upload_result("Uploaded — the link is on your clipboard.", &u);
        }
        None => notify(
            "Couldn't upload the screenshot (no connection, or the host rejected it).",
            SHOT_CAPTION,
            true,
        ),
    }
}

/// Upload the USER files listed (one path per line) in `list_path` — the right-click
/// "Upload" verb's path — copy the resulting URL(s) to the clipboard (one per line),
/// and report. Unlike [`run_upload`], these are the user's own files and are **never
/// deleted**; only the temporary list file is removed. Spawned by the DLL verb via
/// `--upload-keep <list>`.
pub(crate) unsafe fn run_upload_keep(list_path: &str) {
    let host = match upload_host() {
        Ok(h) => h,
        Err(msg) => {
            let _ = std::fs::remove_file(list_path);
            notify(&msg, FILE_CAPTION, true);
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
    // returned link then stays viewable in a browser).
    let urls: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let name = std::path::Path::new(f)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("upload");
            std::fs::read(f).ok().and_then(|bytes| upload(&bytes, name, &host))
        })
        .collect();
    if urls.is_empty() {
        notify(
            "Couldn't upload (no connection, or the host rejected the file).",
            FILE_CAPTION,
            true,
        );
        return;
    }
    let joined = urls.join("\r\n");
    let _ = set_clipboard_text(&joined);
    let heading = if total == 1 {
        "Uploaded — the link is on your clipboard.".to_string()
    } else if urls.len() == total {
        format!("Uploaded all {total} images — the links are on your clipboard.")
    } else {
        format!(
            "Uploaded {} of {} images ({} failed) — the links are on your clipboard.",
            urls.len(),
            total,
            total - urls.len(),
        )
    };
    crate::upload_result::show_upload_result(&heading, &joined);
}

/// A simple completion message (the upload process has no window of its own).
unsafe fn notify(msg: &str, caption: &str, error: bool) {
    let body: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let cap: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
    let icon = if error { MB_ICONWARNING } else { MB_ICONINFORMATION };
    MessageBoxW(None, PCWSTR(body.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | icon);
}

/// Build the multipart body and POST it; return the response URL on success.
/// `filename` goes in the Content-Disposition so the host preserves the file's
/// extension (catbox keys the returned URL off it — a `.jpg` stays viewable).
unsafe fn upload(bytes: &[u8], filename: &str, h: &UploadHost) -> Option<String> {
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
    let resp = post(&h.host, &h.path, &headers, &body)?;
    let text = String::from_utf8_lossy(&resp);
    let url = text.trim();
    (url.starts_with("http") && url.len() < 2048).then(|| url.to_string())
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
