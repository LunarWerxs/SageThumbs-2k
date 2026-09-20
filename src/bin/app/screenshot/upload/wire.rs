//! One upload over the wire: the multipart POST, the response and the URL pulled out of it.

use super::*;

/// Sanitize a value going inside a multipart `Content-Disposition` quoted string — a
/// filename or a config-supplied field name/value. RFC 6266's rule is
/// backslash-escape `"` and `\`; CR/LF can't be escaped at all (a raw one would terminate
/// the header line, letting the rest of the "line" be read as extra header/part-boundary
/// content), so those are replaced with a space rather than passed through. NTFS itself
/// refuses `"` and control characters in a filename through the ordinary Win32 API, but the
/// NT native API, a WSL mount, or a non-Windows SMB server can all create one that carries
/// them anyway — and that filename reaches here unchanged (`upload_files`/`run_upload`
/// take it straight from `Path::file_name`).
pub(super) fn mime_escape(s: &str) -> String {
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
pub(super) unsafe fn upload_one(
    bytes: &[u8],
    filename: &str,
    h: &UploadHost,
) -> Result<String, String> {
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
pub(super) fn interpret_response(status: u16, body: &[u8], json: bool) -> Result<String, String> {
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
pub(super) fn extract_url(body: &str, json: bool) -> Option<String> {
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

pub(super) fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Condense a host's response into one short line for the failure dialog.
pub(super) fn short_reason(body: &str) -> String {
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
pub(super) struct PostResp {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
}

/// Overall wall-clock budget for draining a response body (G218/C18): WinINet's own
/// per-read receive timeout resets on every partial read, so a host that trickles the
/// reply one byte at a time never trips it and can hang the "Uploading…" pill (and block
/// `upload_any` from ever falling through to the next configured host) indefinitely. This
/// matches the 20 s already set on the connect/send/receive `InternetSetOptionW` calls
/// below — well past a slow but working upload, well short of "did it freeze?".
pub(super) const DRAIN_DEADLINE_SECS: u64 = 20;

/// A minimal WinInet HTTPS POST (mirrors `sponsors.rs::http_fetch`, but with a body).
pub(super) unsafe fn post(host: &str, path: &str, headers: &str, body: &[u8]) -> Option<PostResp> {
    let session = crate::http::open_session()?;
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
        let status = crate::http::query_status(req).unwrap_or(0);
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
