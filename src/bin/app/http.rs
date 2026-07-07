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

use windows::core::PCWSTR;
use windows::Win32::Networking::WinInet::{
    HttpOpenRequestW, HttpQueryInfoW, HttpSendRequestW, InternetCloseHandle, InternetConnectW,
    InternetOpenW, InternetSetOptionW, HTTP_QUERY_FLAG_NUMBER, HTTP_QUERY_STATUS_CODE,
    INTERNET_FLAG_NO_CACHE_WRITE, INTERNET_FLAG_PRAGMA_NOCACHE, INTERNET_FLAG_RELOAD,
    INTERNET_FLAG_SECURE, INTERNET_OPTION_CONNECT_TIMEOUT, INTERNET_OPTION_RECEIVE_TIMEOUT,
    INTERNET_OPTION_SEND_TIMEOUT, INTERNET_SERVICE_HTTP,
};

use crate::win::{wide, wininet_drain};

/// A completed HTTPS response: the numeric status code plus the (capped) body bytes.
pub(crate) struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Split an `https://host[:?]/path?query` URL into `(host, path_with_query)`. Returns
/// `None` for anything that isn't a clean `https://` URL or that contains control
/// characters (defense-in-depth against a malformed/hostile base URL). Port pinning is
/// not supported — the store is always on 443.
fn split_https(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    if rest.is_empty() || rest.bytes().any(|b| b < 0x20) {
        return None;
    }
    match rest.find('/') {
        Some(i) => Some((rest[..i].to_string(), rest[i..].to_string())),
        None => Some((rest.to_string(), "/".to_string())),
    }
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

unsafe fn request_raw(
    method: &str,
    host: &str,
    path: &str,
    headers: &str,
    body: &[u8],
    timeout_secs: u64,
    max_resp: usize,
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
    let flags = INTERNET_FLAG_SECURE
        | INTERNET_FLAG_RELOAD
        | INTERNET_FLAG_NO_CACHE_WRITE
        | INTERNET_FLAG_PRAGMA_NOCACHE;
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
    let body_ptr: Option<*const c_void> =
        if body.is_empty() { None } else { Some(body.as_ptr() as *const c_void) };

    let sent = HttpSendRequestW(req, hdr_slice, body_ptr, body.len() as u32).is_ok();

    let resp = if sent {
        let status = query_status(req).unwrap_or(0);
        // `wininet_drain` returns Some(empty) for a 0-byte body (e.g. 204), None only on
        // a read error or an over-cap body — either way we still hand back the status.
        let body = wininet_drain(req, max_resp).unwrap_or_default();
        Some(Resp { status, body })
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
    }

    #[test]
    fn split_https_rejects_non_https_and_control_chars() {
        assert_eq!(split_https("http://example.com/"), None);
        assert_eq!(split_https("ftp://example.com/"), None);
        assert_eq!(split_https("https://"), None);
        assert_eq!(split_https("https://bad\nhost/"), None);
    }
}
