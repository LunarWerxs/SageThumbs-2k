//! OAuth 2.0 **Authorization Code + PKCE via loopback redirect** (RFC 8252) against
//! Connections (`accounts.connections.icu`). This is the native-app sign-in for the
//! optional settings-sync feature.
//!
//! Fully synchronous — no async runtime. The flow:
//!   1. mint a PKCE `code_verifier`/`code_challenge` (RNG + SHA-256 via CNG),
//!   2. bind a throwaway loopback listener on `127.0.0.1:0` (OS picks the port),
//!   3. open the system browser to the authorize URL (the user signs in in their real
//!      browser — this app never sees a password),
//!   4. catch the `?code=` redirect on the loopback socket (a bounded, nonblocking
//!      accept loop — no dedicated runtime), verify `state`,
//!   5. exchange the code (+ verifier) for tokens at the token endpoint.
//!
//! `login()` blocks until the browser round-trip completes or times out, so callers run
//! it on a worker thread (the Settings UI does). EXE-only; never in the DLL.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;

use crate::http;

/// This app's public OAuth client id (== its data-locker `appId`). Registered with
/// Connections 2026-07-05; a public PKCE client, so there is NO client secret here.
pub(crate) const CLIENT_ID: &str = "c6e85c7caceb03d51c0b389435ed1906";

const AUTHORIZE: &str = "https://accounts.connections.icu/oauth/authorize";
const TOKEN: &str = "https://accounts.connections.icu/oauth/token";
const SCOPE: &str = "openid profile email";
/// How long to wait for the user to finish signing in before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 180;

/// The result of a successful token request. The access token is short-lived (kept in
/// memory only); the refresh token is what gets DPAPI-stored (see `cred_store`).
pub(crate) struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    #[allow(dead_code)]
    pub expires_in: u64,
}

// ---- PKCE + crypto (Windows CNG, no extra crate) -------------------------

/// `n` cryptographically-random bytes via the system-preferred RNG. `None` on the
/// (vanishingly unlikely) CNG failure.
fn random_bytes(n: usize) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut buf = vec![0u8; n];
    let status = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status.is_ok().then_some(buf)
}

/// SHA-256 via CNG's single-shot helper (same as `update.rs::sha256_hex`, raw bytes).
fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then_some(out)
}

/// URL-safe base64 without padding — the encoding PKCE + JWT segments use.
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A PKCE `(code_verifier, code_challenge)` pair. Verifier = 43 base64url chars of 32
/// random bytes; challenge = base64url(SHA-256(verifier)).
fn pkce() -> Option<(String, String)> {
    let verifier = b64url(&random_bytes(32)?);
    let challenge = b64url(&sha256(verifier.as_bytes())?);
    Some((verifier, challenge))
}

/// Percent-encode a query/form value, preserving the RFC 3986 *unreserved* set
/// (`A–Z a–z 0–9 - . _ ~`). This keeps `redirect_uri` canonical (`127.0.0.1`, not
/// `127%2E0%2E0%2E1`) — the exact form the loopback URI was registered as — while still
/// encoding `:` `/` space and the rest. Safe for both the authorize query and the
/// `x-www-form-urlencoded` token body (our values contain no spaces).
fn enc(s: &str) -> String {
    const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, UNRESERVED).to_string()
}

// ---- The interactive login -----------------------------------------------

/// Run the full loopback PKCE sign-in. Blocks (browser round-trip / timeout). Returns the
/// tokens, or a short human-readable error for the UI's error state.
pub(crate) fn login() -> Result<Tokens, String> {
    let (verifier, challenge) =
        pkce().ok_or_else(|| "couldn't start sign-in (system crypto unavailable)".to_string())?;
    let state = b64url(&random_bytes(16).ok_or_else(|| "couldn't start sign-in".to_string())?);

    // Bind a throwaway loopback listener on an OS-assigned ephemeral port. Connections does
    // RFC 8252 §7.3 any-port loopback matching, so the registered redirect_uri is the portless
    // `http://127.0.0.1/oauth/callback` and whatever port the OS hands us is accepted.
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("couldn't open a sign-in listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("couldn't read the sign-in listener port: {e}"))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/oauth/callback");

    let url = authorize_url(&redirect, &challenge, &state);
    // Open the user's real browser. Best-effort: if it doesn't open, the loopback simply
    // times out below and we surface that.
    unsafe { crate::win::open_url(&url) };

    let code = catch_code(&listener, Duration::from_secs(LOGIN_TIMEOUT_SECS), &state)?;
    exchange_code(&code, &redirect, &verifier)
}

/// Mint a fresh set of tokens from a stored refresh token (no browser). Used before every
/// store call so we always present a valid access token.
pub(crate) fn refresh(refresh_token: &str) -> Result<Tokens, String> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={CLIENT_ID}",
        enc(refresh_token)
    );
    token_request(body.as_bytes())
}

/// Extract `(sub, email)` from the id_token's JWT payload for the "Synced as …" row. We do
/// NOT verify the signature here — the token came straight from the token endpoint over
/// TLS and is used only for display; the data-locker verifies tokens server-side.
pub(crate) fn identity_from_tokens(t: &Tokens) -> Option<(String, String)> {
    let payload_seg = t.id_token.as_ref()?.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_seg.trim_end_matches('='))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let sub = json.get("sub").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let email = json.get("email").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    if sub.is_empty() {
        return None;
    }
    Some((sub, email))
}

// ---- internals -----------------------------------------------------------

fn authorize_url(redirect: &str, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE}?response_type=code&client_id={CLIENT_ID}&redirect_uri={}\
         &scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        enc(redirect),
        enc(SCOPE),
        enc(challenge),
        enc(state)
    )
}

fn exchange_code(code: &str, redirect: &str, verifier: &str) -> Result<Tokens, String> {
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={CLIENT_ID}&code_verifier={}",
        enc(code),
        enc(redirect),
        enc(verifier)
    );
    token_request(body.as_bytes())
}

fn token_request(body: &[u8]) -> Result<Tokens, String> {
    let resp = http::request(
        "POST",
        TOKEN,
        "Content-Type: application/x-www-form-urlencoded",
        body,
        20,
        128 * 1024,
    )
    .ok_or_else(|| "couldn't reach the sign-in server".to_string())?;
    if resp.status != 200 {
        return Err(token_error(resp.status, &resp.body));
    }
    parse_tokens(&resp.body)
}

fn parse_tokens(body: &[u8]) -> Result<Tokens, String> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "the sign-in server sent an unreadable reply".to_string())?;
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "no access token in the sign-in reply".to_string())?
        .to_string();
    Ok(Tokens {
        access_token,
        refresh_token: json.get("refresh_token").and_then(|v| v.as_str()).map(str::to_string),
        id_token: json.get("id_token").and_then(|v| v.as_str()).map(str::to_string),
        expires_in: json.get("expires_in").and_then(serde_json::Value::as_u64).unwrap_or(3600),
    })
}

/// Turn a non-200 token response into a short message (prefers the OAuth `error_description`).
fn token_error(status: u16, body: &[u8]) -> String {
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(d) = json.get("error_description").and_then(|v| v.as_str()) {
            return d.to_string();
        }
        if let Some(e) = json.get("error").and_then(|v| v.as_str()) {
            return e.to_string();
        }
    }
    format!("sign-in failed (HTTP {status})")
}

/// Bounded, nonblocking accept loop: wait for the browser to hit
/// `/oauth/callback?code=…&state=…`, verify `state`, ack with a friendly page, and return
/// the code. Non-callback hits (favicon, etc.) get a 404 and the loop keeps waiting until
/// `timeout` elapses.
fn catch_code(listener: &TcpListener, timeout: Duration, expected_state: &str) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("loopback setup failed: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for sign-in".to_string());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Some(result) = handle_conn(&mut stream, expected_state) {
                    return result;
                }
                // Not our callback → already 404'd inside; keep waiting.
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(format!("loopback accept failed: {e}")),
        }
    }
}

/// Handle one accepted connection. Returns `Some(Ok(code))` / `Some(Err(..))` when this was
/// the real callback (success or an explicit provider error), or `None` for an unrelated
/// request (which was answered with 404) so the caller keeps waiting.
fn handle_conn(stream: &mut TcpStream, expected_state: &str) -> Option<Result<String, String>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let target = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");

    if !target.starts_with("/oauth/callback") {
        respond_404(stream);
        return None;
    }

    let params = parse_query(target);
    if let Some(err) = params.get("error") {
        respond_html(stream, "Sign-in canceled", "You can close this tab and return to SageThumbs 2K.");
        return Some(Err(format!("sign-in was canceled ({err})")));
    }
    match (params.get("code"), params.get("state")) {
        (Some(code), Some(state)) if state == expected_state => {
            respond_html(
                stream,
                "Signed in",
                "You can close this tab and return to SageThumbs 2K.",
            );
            Some(Ok(code.clone()))
        }
        _ => {
            respond_html(stream, "Sign-in failed", "Something went wrong. Please try again in SageThumbs 2K.");
            Some(Err("sign-in response was missing a code or the state didn't match".to_string()))
        }
    }
}

/// Parse the `?a=b&c=d` query of a request target into decoded key/value pairs.
fn parse_query(target: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some((_, query)) = target.split_once('?') {
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let key = percent_encoding::percent_decode_str(k).decode_utf8_lossy().into_owned();
                let val = percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned();
                map.insert(key, val);
            }
        }
    }
    map
}

fn respond_html(stream: &mut TcpStream, title: &str, message: &str) {
    // The Connections mark, inline. The loopback server serves ONLY this one HTML response (no
    // static-asset route), so the "Sign in with Connections" brand must be self-contained — an
    // inline SVG, not an external `<img>`. Kept in visual sync with the canonical
    // connections-mark.svg (Google-palette connected nodes). Sits left of the outcome title on
    // every sign-in result page (success / canceled / failed).
    const CONNECTIONS_MARK: &str = "<svg viewBox=\"0 0 48 48\" xmlns=\"http://www.w3.org/2000/svg\" role=\"img\" aria-label=\"Connections\">\
<rect x=\"11\" y=\"9.5\" width=\"26\" height=\"7\" rx=\"3.5\" fill=\"#4285F4\"/>\
<circle cx=\"11\" cy=\"13\" r=\"7\" fill=\"#4285F4\"/>\
<circle cx=\"37\" cy=\"13\" r=\"7\" fill=\"#EA4335\"/>\
<circle cx=\"10\" cy=\"24\" r=\"4.5\" fill=\"#9AA0A6\"/>\
<rect x=\"23\" y=\"20.5\" width=\"15\" height=\"7\" rx=\"3.5\" fill=\"#FBBC05\"/>\
<circle cx=\"23\" cy=\"24\" r=\"7\" fill=\"#FBBC05\"/>\
<circle cx=\"38\" cy=\"24\" r=\"5.5\" fill=\"#F9AB00\"/>\
<rect x=\"14\" y=\"31.5\" width=\"26\" height=\"7\" rx=\"3.5\" fill=\"#34A853\"/>\
<circle cx=\"14\" cy=\"35\" r=\"7\" fill=\"#34A853\"/>\
<circle cx=\"40\" cy=\"35\" r=\"7\" fill=\"#34A853\"/></svg>";
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>SageThumbs 2K</title>\
         <style>body{{font-family:Segoe UI,system-ui,sans-serif;background:#1e1e1e;color:#eee;\
         display:flex;min-height:100vh;align-items:center;justify-content:center;text-align:center}}\
         .card{{max-width:28rem;padding:2rem}}\
         .hd{{display:flex;align-items:center;justify-content:center;gap:.6rem;margin:0 0 .5rem}}\
         .hd svg{{width:1.9rem;height:1.9rem;flex:none}}\
         h1{{font-size:1.4rem;margin:0}}\
         p{{opacity:.8;margin:0}}</style></head><body>\
         <div class=\"card\"><div class=\"hd\">{CONNECTIONS_MARK}<h1>{title}</h1></div><p>{message}</p></div></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn respond_404(stream: &mut TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = pkce().expect("CNG available on Windows test host");
        assert_eq!(verifier.len(), 43, "32 bytes → 43 base64url chars");
        assert_eq!(challenge.len(), 43, "SHA-256 (32 bytes) → 43 base64url chars");
        assert!(!verifier.contains(['+', '/', '=']), "verifier must be URL-safe, unpadded");
        // The challenge must equal base64url(SHA-256(verifier)) — the relying party recomputes this.
        let expected = b64url(&sha256(verifier.as_bytes()).unwrap());
        assert_eq!(challenge, expected);
    }

    #[test]
    fn authorize_url_has_required_params() {
        let u = authorize_url("http://127.0.0.1:52100/oauth/callback", "CHAL", "STATE");
        assert!(u.contains("response_type=code"));
        assert!(u.contains(&format!("client_id={CLIENT_ID}")));
        assert!(u.contains("code_challenge=CHAL"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("state=STATE"));
        // redirect_uri must be percent-encoded (no raw "://" or ":port/").
        assert!(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A52100%2Foauth%2Fcallback"));
        // scope space encoded.
        assert!(u.contains("scope=openid%20profile%20email"));
    }

    #[test]
    fn parse_query_decodes_pairs() {
        let m = parse_query("/oauth/callback?code=abc%2F123&state=xy%20z");
        assert_eq!(m.get("code").map(String::as_str), Some("abc/123"));
        assert_eq!(m.get("state").map(String::as_str), Some("xy z"));
    }

    #[test]
    fn parse_tokens_reads_fields() {
        let body = br#"{"access_token":"AT","refresh_token":"RT","id_token":"h.e.s","expires_in":1200}"#;
        let t = parse_tokens(body).unwrap();
        assert_eq!(t.access_token, "AT");
        assert_eq!(t.refresh_token.as_deref(), Some("RT"));
        assert_eq!(t.id_token.as_deref(), Some("h.e.s"));
        assert_eq!(t.expires_in, 1200);
    }

    #[test]
    fn parse_tokens_requires_access_token() {
        assert!(parse_tokens(br#"{"refresh_token":"RT"}"#).is_err());
    }

    #[test]
    fn identity_decodes_jwt_payload() {
        // A JWT with payload {"sub":"user-123","email":"a@b.com"} (base64url, unpadded).
        let payload = b64url(br#"{"sub":"user-123","email":"a@b.com"}"#);
        let t = Tokens {
            access_token: "AT".into(),
            refresh_token: None,
            id_token: Some(format!("header.{payload}.sig")),
            expires_in: 0,
        };
        assert_eq!(identity_from_tokens(&t), Some(("user-123".into(), "a@b.com".into())));
    }
}
