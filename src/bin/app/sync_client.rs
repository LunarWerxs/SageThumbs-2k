//! Connections settings-sync: the allowlisted push/pull between the local HKCU settings
//! and the per-user cloud document at `studio.connections.icu/v1/app-data/{appId}`.
//!
//! Only an explicit **allowlist** of portable preferences is synced — never machine-local
//! values (absolute paths, the upload-host config), local-only flags, or secrets. The
//! store is the "settings locker": one JSON object, ≤64 KB, optimistic-concurrency writes
//! (RFC 7386 deep-merge). EXE-only; the DLL never links this.
//!
//! Public API (all blocking → the Settings UI calls these on a worker thread):
//!   - [`is_signed_in`] / [`signed_in_label`] — UI state.
//!   - [`connect`] — interactive browser sign-in, store creds, initial pull/seed.
//!   - [`pull_on_open`] — pull remote → local (seed if the remote is empty).
//!   - [`push`] — push the current local allowlisted settings.
//!   - [`disconnect`] — delete the remote doc + forget local creds.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use sagethumbs2k_core::settings;
use windows_registry::CURRENT_USER;

use crate::{cred_store, http, oauth};

const STORE_BASE: &str = "https://studio.connections.icu/v1/app-data";
const TIMEOUT_SECS: u64 = 20;
const MAX_RESP: usize = 128 * 1024;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
const PENDING_VALUE: &str = "ConnectionsSyncPending";

#[derive(Clone)]
struct CachedDoc {
    version: u64,
    settings: Value,
    etag: String,
}

static STORE_CACHE: Mutex<Option<CachedDoc>> = Mutex::new(None);
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static PUSH_WORKERS: AtomicUsize = AtomicUsize::new(0);

fn cache() -> MutexGuard<'static, Option<CachedDoc>> {
    STORE_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn sync_guard() -> MutexGuard<'static, ()> {
    SYNC_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Copy)]
enum Kind {
    Dword,
    Str,
}

/// The syncable-key allowlist — portable preferences ONLY. Deliberately excludes
/// `ShotSaveDir` (absolute path), `Debug` (local diagnostics), the install-state
/// flags, `ModernMenuActive` (HKLM installer state), and everything under the `OAuth`
/// subkey (secrets). The `MenuItems\*` and `<ext>\Enabled` subkeys are deferred to v1.1
/// (they need subkey enumeration + the elevated re-register path, respectively).
const ALLOW: &[(&str, Kind)] = &[
    ("EnableThumbs", Kind::Dword),
    ("MaxSize", Kind::Dword),
    ("Width", Kind::Dword),
    ("Height", Kind::Dword),
    ("UseEmbedded", Kind::Dword),
    ("JPEG", Kind::Dword),
    ("PNG", Kind::Dword),
    ("EnableMenu", Kind::Dword),
    ("MenuAllFileTypes", Kind::Dword),
    ("MenuPreview", Kind::Dword),
    ("MenuQuickVerbs", Kind::Dword),
    ("PreviewChecker", Kind::Dword),
    ("FormatBadge", Kind::Dword),
    ("FormatBadgeStyle", Kind::Dword),
    ("ThumbChecker", Kind::Dword),
    // NOT synced: HideTypeOverlay. It is not just a value - flipping it rewrites
    // per-ProgID registry keys on THIS machine, and the ProgIDs differ per machine, so a
    // synced 1 would record a suppression that was never actually applied here.
    ("PreserveFileDate", Kind::Dword),
    ("ContainerSort", Kind::Dword),
    ("ContainerPreferCover", Kind::Dword),
    ("ContainerSkipScanlation", Kind::Dword),
    ("CvJpegQuality", Kind::Dword),
    ("CvWebpQuality", Kind::Dword),
    ("CvWebpLossless", Kind::Dword),
    ("CvPngLevel", Kind::Dword),
    ("CvMagickQuality", Kind::Dword),
    ("ScreenshotHotkey", Kind::Dword),
    ("ScreenshotQuickHotkey", Kind::Dword),
    ("CustomAction", Kind::Dword),
    ("CustomActionHotkey", Kind::Dword),
    ("ScreenshotHideTray", Kind::Dword),
    ("ShotUseSaveDir", Kind::Dword),
    ("UpdateAutoCheck", Kind::Dword),
    ("Lang", Kind::Str),
    ("MenuOrder", Kind::Str),
];

// ---- local <-> JSON ------------------------------------------------------

/// Snapshot the currently-stored allowlisted settings into a JSON object. Only values
/// that are actually PRESENT are included, so a machine that never touched a setting
/// won't push its default and clobber another machine's choice.
///
/// Goes through [`settings::get_dword_opt`] / [`settings::get_string_opt`] rather than
/// opening `CURRENT_USER` directly, because those redirect to the portable ini under
/// `settings::portable()` the same way every other setting in this codebase does. A
/// direct registry read here silently saw an EMPTY snapshot on every portable install
/// (real settings live only in the ini, never HKCU), so sync pushed nothing and pulled
/// nothing without ever surfacing an error.
fn read_local() -> Map<String, Value> {
    let mut map = Map::new();
    for (name, kind) in ALLOW {
        match kind {
            Kind::Dword => {
                if let Some(v) = settings::get_dword_opt(name) {
                    map.insert((*name).to_string(), Value::from(v));
                }
            }
            Kind::Str => {
                if let Some(s) = settings::get_string_opt(name) {
                    map.insert((*name).to_string(), Value::from(s));
                }
            }
        }
    }
    map
}

/// Apply a remote `settings` object to local storage — but ONLY allowlisted keys with the
/// expected type. Unknown keys are ignored (forward-compat + a hostile/expanded doc can't
/// write arbitrary registry values). Returns how many values were applied.
///
/// Same portable-redirect reasoning as [`read_local`]: writing straight to `CURRENT_USER`
/// meant a pulled setting never reached a portable install's actual backing store (the
/// ini), so `pull_on_open` silently applied zero values there every time.
fn apply_remote(settings_obj: &Value) -> u32 {
    let Some(obj) = settings_obj.as_object() else {
        return 0;
    };
    let mut applied = 0;
    for (name, kind) in ALLOW {
        let Some(val) = obj.get(*name) else { continue };
        let ok = match kind {
            // `u32::try_from` rather than `as u32`: an out-of-range remote value (a hostile
            // or corrupted doc) must be REJECTED, not silently truncated and then counted as
            // a successful apply - `as u32` on 4294967296 would wrap to 0 and still report
            // success, writing a value the remote document never actually held.
            Kind::Dword => val
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .is_some_and(|n| settings::set_dword(name, n).is_ok()),
            Kind::Str => val
                .as_str()
                .is_some_and(|s| settings::set_string(name, s).is_ok()),
        };
        if ok {
            applied += 1;
        }
    }
    applied
}

// ---- store transport -----------------------------------------------------

fn store_url() -> String {
    format!("{STORE_BASE}/{}", oauth::CLIENT_ID)
}

fn auth_headers(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\nContent-Type: application/json")
}

fn auth_headers_with_etag(token: &str, etag: Option<&str>) -> String {
    match etag {
        Some(etag) => format!("{}\r\nIf-None-Match: {etag}", auth_headers(token)),
        None => auth_headers(token),
    }
}

fn parse_etag_version(etag: &str) -> Option<u64> {
    etag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

fn clear_cache() {
    *cache() = None;
}

/// GET the current doc → `(version, settings)`. A never-written user is `(0, {})`.
/// Repeated reads are ETag-conditional; 304 reuses the cached document.
fn store_get(token: &str) -> Result<(u64, Value), String> {
    let cached = cache().clone();
    let headers = auth_headers_with_etag(token, cached.as_ref().map(|doc| doc.etag.as_str()));
    let resp = http::request("GET", &store_url(), &headers, &[], TIMEOUT_SECS, MAX_RESP)
        .ok_or_else(|| "couldn't reach the sync server".to_string())?;
    if resp.status == 304 {
        return cached
            .map(|doc| (doc.version, doc.settings))
            .ok_or_else(|| "the sync server returned 304 without a cached document".to_string());
    }
    if resp.status != 200 {
        return Err(store_error(resp.status, &resp.body));
    }
    let json: Value = serde_json::from_slice(&resp.body)
        .map_err(|_| "the sync server sent an unreadable reply".to_string())?;
    let version = resp
        .etag
        .as_deref()
        .and_then(parse_etag_version)
        .unwrap_or_else(|| json.get("version").and_then(Value::as_u64).unwrap_or(0));
    let settings = json
        .get("settings")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    *cache() = Some(CachedDoc {
        version,
        settings: settings.clone(),
        etag: resp.etag.unwrap_or_else(|| format!("\"{version}\"")),
    });
    Ok((version, settings))
}

/// POST the local snapshot as an RFC 7386 deep-merge write, retrying on a version
/// conflict (bounded). Returns the new version.
fn push_snapshot(token: &str) -> Result<u64, String> {
    let snapshot = Value::Object(read_local());
    validate_sync_snapshot(&snapshot)?;
    let mut base = store_get(token)?.0;
    let mut conflicts = 0;
    let mut transient_retries = 0;
    let mut rate_limit_retried = false;
    loop {
        let body = serde_json::json!({ "settings": snapshot, "baseVersion": base, "merge": true });
        let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let Some(resp) = http::request(
            "POST",
            &store_url(),
            &auth_headers(token),
            &bytes,
            TIMEOUT_SECS,
            MAX_RESP,
        ) else {
            if transient_retries < 2 {
                transient_retries += 1;
                std::thread::sleep(Duration::from_secs(1 << (transient_retries - 1)));
                continue;
            }
            return Err("couldn't reach the sync server".to_string());
        };
        match resp.status {
            200 => {
                let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                clear_cache();
                return Ok(json
                    .get("version")
                    .and_then(Value::as_u64)
                    .unwrap_or(base + 1));
            }
            409 => {
                conflicts += 1;
                if conflicts >= 3 {
                    return Err(
                        "sync kept conflicting with another device — please try again".to_string(),
                    );
                }
                // Stale baseVersion — take the server's current version and retry.
                let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                base = json
                    .get("current")
                    .and_then(|c| c.get("version"))
                    .and_then(Value::as_u64)
                    .or_else(|| store_get(token).ok().map(|(v, _)| v))
                    .unwrap_or(base);
            }
            429 if !rate_limit_retried => {
                rate_limit_retried = true;
                std::thread::sleep(rate_limit_wait(&resp.body));
            }
            status if status >= 500 && transient_retries < 2 => {
                transient_retries += 1;
                std::thread::sleep(Duration::from_secs(1 << (transient_retries - 1)));
            }
            _ => return Err(store_error(resp.status, &resp.body)),
        }
    }
}

fn store_delete(token: &str) -> Result<(), String> {
    let resp = http::request(
        "DELETE",
        &store_url(),
        &auth_headers(token),
        &[],
        TIMEOUT_SECS,
        MAX_RESP,
    )
    .ok_or_else(|| "couldn't reach the sync server".to_string())?;
    // 204 = deleted, 404 = already gone — both fine for "disconnect".
    if matches!(resp.status, 200 | 204 | 404) {
        clear_cache();
        Ok(())
    } else {
        Err(store_error(resp.status, &resp.body))
    }
}

/// Map the documented store status codes to short, friendly messages.
fn store_error(status: u16, body: &[u8]) -> String {
    match status {
        401 => return "your sign-in expired — please sign in again".to_string(),
        403 => return "this app isn't authorized for that account".to_string(),
        413 => return "your settings are too large to sync".to_string(),
        429 => {
            let seconds = retry_after_seconds(body).unwrap_or(1);
            return format!("syncing too often — retry after {seconds} seconds");
        }
        _ => {}
    }
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        if let Some(e) = json.get("error").and_then(Value::as_str) {
            return format!("sync failed: {e}");
        }
    }
    format!("sync failed (HTTP {status})")
}

fn retry_after_seconds(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("retry_after_seconds")?
        .as_u64()
        .filter(|seconds| *seconds > 0)
}

fn rate_limit_wait(body: &[u8]) -> Duration {
    Duration::from_secs(retry_after_seconds(body).unwrap_or(1)).min(MAX_RATE_LIMIT_WAIT)
}

fn ascii_tail(value: &str, prefix: &str, min: usize, allowed: impl Fn(u8) -> bool) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|tail| tail.len() >= min && tail.bytes().all(allowed))
}

fn credential_string(value: &str) -> Option<&'static str> {
    let value = value.trim();
    let alpha_num = |byte: u8| byte.is_ascii_alphanumeric();
    let alpha_num_dash = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');

    if [
        "sk_live_", "sk_test_", "pk_live_", "pk_test_", "rk_live_", "rk_test_",
    ]
    .iter()
    .any(|prefix| ascii_tail(value, prefix, 16, alpha_num))
    {
        return Some("a Stripe key");
    }
    if ascii_tail(value, "sk-", 20, alpha_num_dash) {
        return Some("an OpenAI-style API key");
    }
    if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| ascii_tail(value, prefix, 36, alpha_num))
    {
        return Some("a GitHub token");
    }
    if ascii_tail(value, "github_pat_", 22, |byte| {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }) {
        return Some("a GitHub fine-grained token");
    }
    if ["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"]
        .iter()
        .any(|prefix| ascii_tail(value, prefix, 10, |byte| alpha_num(byte) || byte == b'-'))
    {
        return Some("a Slack token");
    }
    if value.len() == 20
        && value.starts_with("AKIA")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Some("an AWS access key id");
    }
    if value.len() == 39 && ascii_tail(value, "AIza", 35, alpha_num_dash) {
        return Some("a Google API key");
    }
    let jwt = value.split('.').collect::<Vec<_>>();
    if value.starts_with("ey")
        && jwt.len() == 3
        && jwt[0].len() >= 10
        && jwt[1].len() >= 10
        && jwt[2].len() >= 5
        && jwt.iter().all(|part| part.bytes().all(alpha_num_dash))
    {
        return Some("a JWT");
    }
    if value.contains("-----BEGIN ") && value.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    None
}

fn credential_in_value(value: &Value, depth: usize) -> Option<&'static str> {
    match value {
        Value::String(value) => credential_string(value),
        Value::Array(values) if depth < 4 => values
            .iter()
            .find_map(|value| credential_in_value(value, depth + 1)),
        Value::Object(values) if depth < 4 => values
            .values()
            .find_map(|value| credential_in_value(value, depth + 1)),
        _ => None,
    }
}

fn validate_sync_snapshot(snapshot: &Value) -> Result<(), String> {
    if let Some(entries) = snapshot.as_object() {
        for (key, value) in entries {
            if let Some(what) = credential_in_value(value, 0) {
                return Err(format!(
                    "refusing to sync \"{key}\": its value is {what}; Connections stores settings, not credentials"
                ));
            }
        }
    }
    let bytes = serde_json::to_vec(snapshot).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        let biggest = snapshot
            .as_object()
            .and_then(|entries| {
                entries
                    .iter()
                    .map(|(key, value)| {
                        (
                            key,
                            serde_json::to_vec(value)
                                .map(|bytes| bytes.len())
                                .unwrap_or(0),
                        )
                    })
                    .max_by_key(|(_, bytes)| *bytes)
            })
            .map(|(key, bytes)| format!("; \"{key}\" alone is {bytes} bytes"))
            .unwrap_or_default();
        return Err(format!(
            "synced settings are {} bytes, over the {MAX_DOCUMENT_BYTES}-byte limit{biggest}",
            bytes.len()
        ));
    }
    Ok(())
}

/// Persist a freshly-rotated refresh token via `save`, failing loudly if the store
/// rejects it. Taking `save` as a parameter (rather than calling `cred_store` directly)
/// is what makes this — the actual decision logic A037 was about — testable without a
/// live DPAPI/HKCU credential store.
fn persist_rotation(rotated: Option<&str>, save: impl FnOnce(&str) -> bool) -> Result<(), String> {
    match rotated {
        // The server already rotated the token by the time we get here; if the local
        // write fails, the cache would otherwise hold a token the server has already
        // invalidated, and every future refresh would fail with no clue why.
        Some(new_rt) if !save(new_rt) => {
            Err("couldn't securely store your renewed sign-in".to_string())
        }
        _ => Ok(()),
    }
}

/// Mint a fresh access token from the stored refresh token (rotating + re-persisting it).
fn access_token() -> Result<String, String> {
    let rt = cred_store::load_refresh_token().ok_or_else(|| "not signed in".to_string())?;
    let tokens = oauth::refresh(&rt)?;
    persist_rotation(
        tokens.refresh_token.as_deref(),
        cred_store::save_refresh_token,
    )?;
    Ok(tokens.access_token)
}

// ---- public orchestration (UI-facing) ------------------------------------

/// Whether a (decryptable) refresh token is stored on this machine.
pub(crate) fn is_signed_in() -> bool {
    cred_store::is_signed_in()
}

pub(crate) fn mark_push_pending() {
    if let Ok(key) = CURRENT_USER.create(settings::ROOT) {
        let _ = key.set_u32(PENDING_VALUE, 1);
    }
}

fn clear_push_pending() {
    if let Ok(key) = CURRENT_USER.open(settings::ROOT) {
        let _ = key.remove_value(PENDING_VALUE);
    }
}

pub(crate) fn has_pending_push() -> bool {
    CURRENT_USER
        .open(settings::ROOT)
        .and_then(|key| key.get_u32(PENDING_VALUE))
        .is_ok_and(|value| value != 0)
}

pub(crate) fn begin_push_worker() {
    PUSH_WORKERS.fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn finish_push_worker(success: bool) {
    let remaining = PUSH_WORKERS
        .fetch_sub(1, Ordering::AcqRel)
        .saturating_sub(1);
    if success && remaining == 0 {
        clear_push_pending();
    }
}

/// The name (or, failing that, the relay email) to show in the "Synced as …" row, if
/// signed in. The `email` claim is an opaque per-app privacy-relay address
/// (`<hex>@privaterelay.connections.icu`), never the user's real inbox, so `name` is
/// preferred whenever we have one. A bare `sub` is never surfaced — `None` if all we have
/// is an id with no name and no email.
pub(crate) fn signed_in_label() -> Option<String> {
    let id = cred_store::load_identity()?;
    if !id.name.is_empty() {
        Some(id.name)
    } else if !id.email.is_empty() {
        Some(id.email)
    } else {
        None
    }
}

/// Interactive sign-in: browser round-trip, securely store the refresh token + identity,
/// then do the initial pull (or seed the cloud from local if it's empty). Returns the
/// display label (name/email/sub) for the UI. Blocking — run on a worker thread.
pub(crate) fn connect() -> Result<String, String> {
    let _guard = sync_guard();
    clear_cache();
    let tokens = oauth::login()?;
    let rt = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| "the sign-in server didn't return a refresh token".to_string())?;
    if !cred_store::save_refresh_token(&rt) {
        return Err("couldn't securely store your sign-in".to_string());
    }
    let (sub, email, name, picture) = oauth::identity_from_tokens(&tokens).unwrap_or_default();
    cred_store::save_identity(&sub, &email, &name, &picture);

    // Initial pull/seed using the access token we already hold (no refresh needed).
    let (version, settings) = store_get(&tokens.access_token)?;
    if version > 0 {
        apply_remote(&settings);
    } else {
        push_snapshot(&tokens.access_token)?;
    }
    Ok(if !name.is_empty() {
        name
    } else if !email.is_empty() {
        email
    } else {
        sub
    })
}

/// Pull remote settings and apply them locally; seed the cloud if it's empty. Returns
/// `Ok(true)` if any values were applied (so the UI should refresh its controls).
/// Blocking — run on a worker thread.
pub(crate) fn pull_on_open() -> Result<bool, String> {
    let _guard = sync_guard();
    let token = access_token()?;
    if has_pending_push() {
        push_snapshot(&token)?;
        clear_push_pending();
    }
    let (version, settings) = store_get(&token)?;
    if version > 0 {
        Ok(apply_remote(&settings) > 0)
    } else {
        push_snapshot(&token)?;
        Ok(false)
    }
}

/// Push the current local allowlisted settings to the cloud. Blocking — run on a worker
/// thread (called after the user applies settings changes).
pub(crate) fn push() -> Result<(), String> {
    let _guard = sync_guard();
    let token = access_token()?;
    push_snapshot(&token).map(|_| ())
}

/// Disconnect: best-effort delete the remote doc, then forget local credentials.
pub(crate) fn disconnect() {
    let _guard = sync_guard();
    if let Ok(token) = access_token() {
        let _ = store_delete(&token);
    }
    cred_store::clear();
    clear_cache();
    clear_push_pending();
}

/// Wait for a detached Save push, then make one final bounded attempt if a
/// previous failure left the durable pending marker set.
pub(crate) fn flush_pending(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while PUSH_WORKERS.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if !has_pending_push() || Instant::now() >= deadline || !is_signed_in() {
        return;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let (tx, rx) = std::sync::mpsc::channel();
    begin_push_worker();
    if std::thread::Builder::new()
        .name("sage-sync-flush".into())
        .spawn(move || {
            let result = push();
            finish_push_worker(result.is_ok());
            let _ = tx.send(result);
        })
        .is_err()
    {
        finish_push_worker(false);
        return;
    }
    let _ = rx.recv_timeout(remaining);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_excludes_machine_local_and_secrets() {
        let names: Vec<&str> = ALLOW.iter().map(|(n, _)| *n).collect();
        for banned in [
            "ShotSaveDir",
            "Debug",
            "InstallReported",
            "DevMachine",
            "Tombstone",
            "ModernMenuActive",
            "RefreshToken",
        ] {
            assert!(!names.contains(&banned), "{banned} must NEVER be synced");
        }
        for portable in [
            "EnableThumbs",
            "MenuOrder",
            "Lang",
            "ScreenshotHotkey",
            "JPEG",
        ] {
            assert!(names.contains(&portable), "{portable} should be syncable");
        }
    }

    #[test]
    fn allowlist_has_no_duplicates() {
        let mut names: Vec<&str> = ALLOW.iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate key in the sync allowlist");
    }

    #[test]
    fn apply_remote_ignores_non_allowlisted_and_wrong_types() {
        // A hostile/expanded doc: an off-list key + a wrong-typed on-list key. `apply_remote`
        // must not count either (and never writes the off-list key). We assert on the count
        // rather than touching the real registry for the on-list value.
        let doc = serde_json::json!({
            "ShotSaveDir": "C:\\evil\\path",   // off-list → ignored
            "SomeRandomKey": 1,                 // off-list → ignored
            "JPEG": "not-a-number"              // on-list but wrong type → not applied
        });
        // Only off-list / wrong-typed entries → nothing applies.
        assert_eq!(apply_remote(&doc), 0);
    }

    #[test]
    fn apply_remote_rejects_out_of_range_dword_instead_of_truncating() {
        // 4294967296 (2^32) is a valid JSON number and a valid u64, but doesn't fit a u32.
        // The old `as u32` cast wrapped it to 0 and still counted it as applied; `try_from`
        // must reject it instead, so nothing is written and the count stays 0.
        let doc = serde_json::json!({ "JPEG": 4294967296u64 });
        assert_eq!(apply_remote(&doc), 0);
    }

    #[test]
    fn credential_values_are_refused_even_when_nested() {
        let doc = serde_json::json!({
            "future": {
                "token": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
            }
        });
        let error = validate_sync_snapshot(&doc).unwrap_err();
        assert!(error.contains("GitHub token"), "{error}");
        assert!(validate_sync_snapshot(&serde_json::json!({"Lang": "sk"})).is_ok());
    }

    #[test]
    fn oversized_snapshots_are_rejected_locally() {
        let doc = serde_json::json!({"MenuOrder": "x".repeat(MAX_DOCUMENT_BYTES)});
        let error = validate_sync_snapshot(&doc).unwrap_err();
        assert!(error.contains("over the 65536-byte limit"), "{error}");
        assert!(error.contains("MenuOrder"), "{error}");
    }

    #[test]
    fn etag_and_rate_limit_contract_fields_are_parsed() {
        assert_eq!(parse_etag_version("\"42\""), Some(42));
        assert_eq!(parse_etag_version("W/\"7\""), Some(7));
        assert_eq!(
            retry_after_seconds(br#"{"retry_after_seconds":9}"#),
            Some(9)
        );
    }

    /// A037: a rotated refresh token the server already accepted must never be silently
    /// discarded just because the local DPAPI/HKCU write failed.
    #[test]
    fn persist_rotation_errors_when_a_rotated_token_fails_to_save() {
        let result = persist_rotation(Some("new-token"), |_| false);
        assert!(
            result.is_err(),
            "a failed save of a rotated token must surface an error, not vanish silently"
        );
    }

    #[test]
    fn persist_rotation_succeeds_when_the_store_accepts_the_rotated_token() {
        assert!(persist_rotation(Some("new-token"), |_| true).is_ok());
    }

    #[test]
    fn persist_rotation_is_a_no_op_when_the_server_did_not_rotate_the_token() {
        // `save` must not even be called when nothing rotated.
        let result = persist_rotation(None, |_| panic!("save must not run without a rotation"));
        assert!(result.is_ok());
    }
}
