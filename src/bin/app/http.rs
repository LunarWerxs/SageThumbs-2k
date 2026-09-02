//! Minimal synchronous HTTPS client (WinINet) for the Connections settings-sync
//! feature — GET / POST / DELETE with a Bearer token and a JSON (or form) body,
//! returning the **HTTP status code** so the data-locker's 200 / 409 / 401 / 413 /
//! 429 contract can be honored.
//!
//! EXE-only: this module is compiled into `SageThumbs2K.exe` (the Settings app),
//! never into the crash-isolated shell-extension DLL (which must not do networking).
//! It deliberately mirrors the WinINet idiom already proven in `sponsors.rs` (GET via
//! `InternetOpenUrlW`) and `screenshot::upload` (POST via `InternetConnectW` +
//! `HttpOpenRequestW` + `HttpSendRequestW`), but adds `HttpQueryInfoW` to read the
//! status line — the store needs the code, not just the body.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Networking::WinInet::{
    HttpOpenRequestW, HttpQueryInfoW, HttpSendRequestW, InternetCloseHandle, InternetConnectW,
    InternetOpenW, InternetSetOptionW, HTTP_QUERY_ETAG, HTTP_QUERY_FLAG_NUMBER,
    HTTP_QUERY_STATUS_CODE, INTERNET_FLAG_NO_AUTO_REDIRECT, INTERNET_FLAG_NO_CACHE_WRITE,
    INTERNET_FLAG_PRAGMA_NOCACHE, INTERNET_FLAG_RELOAD, INTERNET_FLAG_SECURE,
    INTERNET_OPTION_CONNECT_TIMEOUT, INTERNET_OPTION_RECEIVE_TIMEOUT, INTERNET_OPTION_SEND_TIMEOUT,
    INTERNET_SERVICE_HTTP,
};

use crate::win::wide;

/// A completed HTTPS response: status, ETag (when present), and capped body.
pub(crate) struct Resp {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

/// Split an `https://host/path?query` URL into `(host, path_with_query)`. Returns
/// `None` for anything that isn't a clean HTTPS URL for port 443. Keeping this parser
/// strict avoids handing WinINet ambiguous authority forms (`userinfo@host`, a hidden
/// port, backslashes, or a fragment) when future callers accept configurable URLs.
/// Internationalized hosts must be supplied in their ASCII/Punycode form.
pub(crate) fn split_https(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    if rest.is_empty() || rest.bytes().any(|b| b <= 0x20 || b == 0x7f) || rest.contains(['\\', '#'])
    {
        return None;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let host = &rest[..authority_end];
    if host.is_empty()
        || host.contains(['@', ':'])
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return None;
    }
    let path = match rest.as_bytes().get(authority_end) {
        Some(b'/') => rest[authority_end..].to_string(),
        Some(b'?') => format!("/{}", &rest[authority_end..]),
        None => "/".to_string(),
        _ => return None,
    };
    Some((host.to_string(), path))
}

/// Percent-encode one query/form value, preserving the RFC 3986 *unreserved* set
/// (`A–Z a–z 0–9 - . _ ~`). Everything else — `:` `/` `&` `=` space, and all
/// non-ASCII — is escaped, so a value can't smuggle a second field into an
/// `x-www-form-urlencoded` body. Keeping the unreserved set intact also leaves
/// literals like `127.0.0.1` canonical rather than `127%2E0%2E0%2E1`.
pub(crate) fn form_enc(s: &str) -> String {
    const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, UNRESERVED).to_string()
}

/// Perform one HTTPS request. `method` is `"GET"` / `"POST"` / `"DELETE"`. `headers` is
/// a `\r\n`-separated header block (no trailing CRLF), possibly empty. `body` is the
/// request body (empty for GET/DELETE). Returns the status + body, or `None` on any
/// transport failure (bad URL, connect/send error). The body is capped at `max_resp`;
/// an over-cap response yields an empty body but the real status.
pub(crate) fn request(
    method: &str,
    url: &str,
    headers: &str,
    body: &[u8],
    timeout_secs: u64,
    max_resp: usize,
) -> Option<Resp> {
    let (host, path) = split_https(url)?;
    unsafe { request_raw(method, &host, &path, headers, body, timeout_secs, max_resp) }
}

/// Like [`request`], but exposes the knobs `sponsors.rs`'s fetch/download helpers need so they
/// can share this ONE WinINet core instead of hand-rolling their own (A130): `reload` controls
/// whether WinINet is told to bypass its cache (the manifest/self-update checks want a fresh
/// origin fetch every time; versioned/immutable sponsor images don't), and `on_progress` — when
/// given — is polled after every chunk read with the bytes read so far. It is a progress readout
/// only (issue #218/C18: this now routes through the shared `win::wininet_drain`, which has no
/// abort-callback contract) — a caller that wants to abort mid-download polls its own progress
/// counter on a separate thread instead, the way `sponsors::http_download_streaming` does.
///
/// `overall_timeout_secs` bounds the WHOLE call end-to-end (A123): WinINet's own
/// `INTERNET_OPTION_*_TIMEOUT`s are per-phase and — critically — the receive one resets on
/// every partial read, so a server that trickles one byte just often enough can otherwise hold
/// the connection open forever. This adds the wall-clock backstop that's missing without it,
/// the same role `decode.rs`'s ImageMagick subprocess elapsed guard plays for CPU-time budgets.
pub(crate) fn request_ex(
    method: &str,
    url: &str,
    reload: bool,
    timeout_secs: u64,
    overall_timeout_secs: u64,
    max_resp: usize,
    on_progress: Option<&mut dyn FnMut(u64)>,
) -> Option<Resp> {
    let (host, path) = split_https(url)?;
    let deadline = Instant::now() + Duration::from_secs(overall_timeout_secs.max(1));
    unsafe {
        request_raw_ex(
            method,
            &host,
            &path,
            "",
            &[],
            reload,
            timeout_secs,
            Some(deadline),
            max_resp,
            on_progress,
        )
    }
}

/// Whether `headers` (the `\r\n`-separated block passed to [`request`]/[`request_ex`]) carries
/// an `Authorization` header — the signal `request_raw_ex` uses to set
/// `INTERNET_FLAG_NO_AUTO_REDIRECT` (issue #227/P61). Line-anchored (checked per header line,
/// not a raw substring search of the whole block) so a value that happened to contain the text
/// `Authorization:` couldn't itself flip this on.
fn carries_authorization(headers: &str) -> bool {
    headers
        .split("\r\n")
        .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
}

unsafe fn request_raw(
    method: &str,
    host: &str,
    path: &str,
    headers: &str,
    body: &[u8],
    timeout_secs: u64,
    max_resp: usize,
) -> Option<Resp> {
    // `reload: true` + `deadline: None` reproduces this function's ORIGINAL behavior exactly
    // (unconditional cache-bypass flags, no overall wall-clock cap) — `request`'s existing
    // callers (oauth.rs, sync_client.rs, feedback.rs) keep their current behavior unchanged.
    request_raw_ex(
        method,
        host,
        path,
        headers,
        body,
        true,
        timeout_secs,
        None,
        max_resp,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn request_raw_ex(
    method: &str,
    host: &str,
    path: &str,
    headers: &str,
    body: &[u8],
    reload: bool,
    timeout_secs: u64,
    deadline: Option<Instant>,
    max_resp: usize,
    on_progress: Option<&mut dyn FnMut(u64)>,
) -> Option<Resp> {
    let agent = wide("SageThumbs2K");
    let session = InternetOpenW(PCWSTR(agent.as_ptr()), 0, PCWSTR::null(), PCWSTR::null(), 0);
    if session.is_null() {
        return None;
    }
    // Bound each phase so a dead host can't hang the Settings window / worker thread.
    let timeout_ms: u32 = (timeout_secs as u32) * 1000;
    for opt in [
        INTERNET_OPTION_CONNECT_TIMEOUT,
        INTERNET_OPTION_RECEIVE_TIMEOUT,
        INTERNET_OPTION_SEND_TIMEOUT,
    ] {
        let _ = InternetSetOptionW(
            Some(session),
            opt,
            Some(&timeout_ms as *const u32 as *const c_void),
            std::mem::size_of::<u32>() as u32,
        );
    }

    let host_w = wide(host);
    let conn = InternetConnectW(
        session,
        PCWSTR(host_w.as_ptr()),
        443,
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

    let verb = wide(method);
    let path_w = wide(path);
    let mut flags = INTERNET_FLAG_SECURE;
    if reload {
        flags |= INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE | INTERNET_FLAG_PRAGMA_NOCACHE;
    }
    // Issue #227/P61: a request carrying an `Authorization` header (the sync client's Bearer
    // token) must never let WinINet chase a redirect on its own — a malicious or MITM'd 30x
    // could otherwise send the token's request to an attacker-controlled host. A non-redirect
    // 3xx status is then just another non-2xx response to the caller, same as any other error.
    if carries_authorization(headers) {
        flags |= INTERNET_FLAG_NO_AUTO_REDIRECT;
    }
    let req = HttpOpenRequestW(
        conn,
        PCWSTR(verb.as_ptr()),
        PCWSTR(path_w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        None,
        flags,
        None,
    );
    if req.is_null() {
        let _ = InternetCloseHandle(conn);
        let _ = InternetCloseHandle(session);
        return None;
    }

    // Headers: WinINet wants a length-counted UTF-16 slice WITHOUT the trailing NUL
    // (matching `screenshot::upload::post`). Empty header block → pass None.
    let hdr_w = wide(headers);
    let hdr_slice: Option<&[u16]> = if headers.is_empty() {
        None
    } else {
        Some(&hdr_w[..hdr_w.len().saturating_sub(1)])
    };
    let body_ptr: Option<*const c_void> = if body.is_empty() {
        None
    } else {
        Some(body.as_ptr() as *const c_void)
    };

    let sent = HttpSendRequestW(req, hdr_slice, body_ptr, body.len() as u32).is_ok();

    let resp = if sent {
        let status = query_status(req).unwrap_or(0);
        let etag = query_text_header(req, HTTP_QUERY_ETAG);
        // `crate::win::wininet_drain` (issue #218/C18: the fork that used to live here was
        // folded back into that shared helper) returns Some(empty) for a 0-byte body (e.g.
        // 204), None only on a read error, an over-cap body, or an expired deadline — either
        // way we still hand back the status. Its progress callback takes `usize`; ours takes
        // `u64` (matching the byte counts the rest of this module already uses).
        let body = match on_progress {
            Some(cb) => {
                let mut wrapped = |n: usize| cb(n as u64);
                crate::win::wininet_drain(req, max_resp, deadline, Some(&mut wrapped))
            }
            None => crate::win::wininet_drain(req, max_resp, deadline, None),
        }
        .unwrap_or_default();
        Some(Resp { status, etag, body })
    } else {
        None
    };

    let _ = InternetCloseHandle(req);
    let _ = InternetCloseHandle(conn);
    let _ = InternetCloseHandle(session);
    resp
}

/// Read the numeric HTTP status code off a completed request via `HttpQueryInfoW`
/// with `HTTP_QUERY_FLAG_NUMBER` (fills a DWORD, no string parsing).
unsafe fn query_status(req: *mut c_void) -> Option<u16> {
    let mut code: u32 = 0;
    let mut len: u32 = std::mem::size_of::<u32>() as u32;
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

/// Read a short response header such as ETag. The locker ETag is only a quoted
/// integer, so a fixed buffer keeps this minimal and avoids a probing call.
unsafe fn query_text_header(req: *mut c_void, query: u32) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut len = std::mem::size_of_val(&buf) as u32;
    HttpQueryInfoW(
        req,
        query,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut len,
        None,
    )
    .ok()?;
    let chars = (len as usize / std::mem::size_of::<u16>()).min(buf.len());
    let value = String::from_utf16_lossy(&buf[..chars])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_https_parses_host_and_path() {
        assert_eq!(
            split_https("https://studio.connections.icu/v1/app-data/abc"),
            Some(("studio.connections.icu".into(), "/v1/app-data/abc".into()))
        );
        // No path → defaults to "/".
        assert_eq!(
            split_https("https://example.com"),
            Some(("example.com".into(), "/".into()))
        );
        // Query string rides along in the path component.
        assert_eq!(
            split_https("https://h.test/p?a=1&b=2"),
            Some(("h.test".into(), "/p?a=1&b=2".into()))
        );
        assert_eq!(
            split_https("https://h.test?a=1"),
            Some(("h.test".into(), "/?a=1".into()))
        );
    }

    #[test]
    fn split_https_rejects_non_https_and_ambiguous_authorities() {
        assert_eq!(split_https("http://example.com/"), None);
        assert_eq!(split_https("ftp://example.com/"), None);
        assert_eq!(split_https("https://"), None);
        assert_eq!(split_https("https://bad\nhost/"), None);
        assert_eq!(split_https("https://user@example.com/"), None);
        assert_eq!(split_https("https://example.com:8443/"), None);
        assert_eq!(split_https("https://example.com\\other/"), None);
        assert_eq!(split_https("https://example.com/path#fragment"), None);
        assert_eq!(split_https("https://exam ple.com/"), None);
    }

    /// `request_ex` (A130/A123's shared core for `sponsors.rs`) must reject a non-HTTPS URL
    /// via the same `split_https` gate as `request`, before ever touching WinINet — not just
    /// eventually fail after opening a session.
    #[test]
    fn request_ex_rejects_non_https_before_touching_wininet() {
        assert!(request_ex("GET", "http://example.com/", true, 5, 30, 4096, None).is_none());
        assert!(request_ex("GET", "not a url", true, 5, 30, 4096, None).is_none());
    }

    /// Issue #227/P61: `carries_authorization` is the gate `request_raw_ex` uses to add
    /// `INTERNET_FLAG_NO_AUTO_REDIRECT` — it must fire for the real header shape
    /// `sync_client::auth_headers` builds, and it must be a per-line match, not a raw
    /// substring search that a header VALUE could accidentally trip.
    #[test]
    fn carries_authorization_matches_a_real_header_and_is_line_anchored() {
        assert!(carries_authorization(
            "Authorization: Bearer abc123\r\nContent-Type: application/json"
        ));
        assert!(carries_authorization("authorization: bearer x")); // header names are case-insensitive
        assert!(!carries_authorization(""));
        assert!(!carries_authorization("Content-Type: application/json"));
        // The word appearing inside a header VALUE (not as a header name) must not match.
        assert!(!carries_authorization(
            "X-Note: no Authorization: header here"
        ));
    }
}
