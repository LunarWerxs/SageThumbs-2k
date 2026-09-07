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

use crate::{cred_store, http, oauth};

const STORE_BASE: &str = "https://studio.connections.icu/v1/app-data";
const TIMEOUT_SECS: u64 = 20;
const MAX_RESP: usize = 128 * 1024;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
/// Ceiling on how long a single push waits out a 429's `Retry-After` before giving up on
/// that attempt (E05 audit: named so the bound is a stated fact, not an implicit product of
/// two other constants). A relay asking for longer than this is asking for longer than a
/// worker thread should block one Settings session on.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
/// How many times [`push_snapshot`] will wait out a 429 and retry the SAME push, once
/// per relay-provided `retry_after_seconds` (each wait itself capped at
/// [`MAX_RATE_LIMIT_WAIT`]). Bounded to 1: a relay asking twice in one push is a relay that
/// wants the caller to back off past this session, which is what the durable pending
/// marker + "retry on next Settings open" path is for.
const MAX_PUSH_RATE_LIMIT_RETRIES: u32 = 1;
/// How many times [`push_snapshot`] retries a transient failure - no response at all, or a
/// 5xx - within ONE push attempt, backing off `2^n` seconds each time. Past this the whole
/// push fails and the durable pending marker carries the retry to the next Settings open
/// instead of blocking the worker thread indefinitely.
const MAX_PUSH_TRANSIENT_RETRIES: u32 = 2;
/// How many times [`push_snapshot`] will re-fetch the current version and retry after a 409
/// (another device wrote first) before giving up and telling the user to try again. Bounded
/// so two machines that are both actively syncing can't live-lock each other forever.
const MAX_PUSH_CONFLICT_RETRIES: u32 = 3;
const PENDING_VALUE: &str = "ConnectionsSyncPending";
// F16 (2026-09-05 audit): a SEPARATE marker from `PENDING_VALUE` above. That one means
// "a later push, after Save, failed and must retry" and its retry (`pull_on_open`'s
// has_pending_push branch) always re-pushes unconditionally. This one means "the very
// first sync after sign-in never completed", whose retry must redo the GET-or-seed
// decision (`sync_once`), not blindly push - the account may already hold real data from
// another device that a blind push would stomp. Kept distinct on purpose; both reuse the
// same name-parameterised marker primitives batch 1 introduced.
const INITIAL_SYNC_PENDING_VALUE: &str = "ConnectionsInitialSyncPending";

#[derive(Clone)]
struct CachedDoc {
    version: u64,
    settings: Value,
    etag: String,
}

static STORE_CACHE: Mutex<Option<CachedDoc>> = Mutex::new(None);
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static PUSH_WORKERS: AtomicUsize = AtomicUsize::new(0);

// ---- test-only network/sleep injection (see `sync_http_request`/`sync_sleep`) -----------

/// One scripted network response: `None` = a dead connection (no HTTP response at all);
/// `Some((status, body))` = the relay answered.
#[cfg(test)]
type ScriptedResponse = Option<(u16, Vec<u8>)>;

#[cfg(test)]
thread_local! {
    /// Scripted relay responses for [`push_snapshot`]-loop tests, drained front-to-back, so
    /// a test scripts a whole exchange (the base-version GET first, then each POST retry) in
    /// the order the real loop will make the calls.
    static SCRIPTED_RESPONSES: std::cell::RefCell<std::collections::VecDeque<ScriptedResponse>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Every duration [`sync_sleep`] was asked to wait, in call order, for a test to assert
    /// against instead of a real test run actually waiting.
    static RECORDED_SLEEPS: std::cell::RefCell<Vec<Duration>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Queue one scripted response for the next [`sync_http_request`] call on THIS thread.
/// Thread-local, so parallel `cargo test` threads never see each other's script.
#[cfg(test)]
fn script_response(resp: ScriptedResponse) {
    SCRIPTED_RESPONSES.with(|q| q.borrow_mut().push_back(resp));
}

/// Clear this thread's scripted-response queue and recorded sleeps, so one test's leftovers
/// (an unconsumed response, an unread sleep) can never leak into the next test on a reused
/// test-harness thread.
#[cfg(test)]
fn reset_test_network_state() {
    SCRIPTED_RESPONSES.with(|q| q.borrow_mut().clear());
    RECORDED_SLEEPS.with(|s| s.borrow_mut().clear());
}

#[cfg(test)]
fn recorded_sleeps() -> Vec<Duration> {
    RECORDED_SLEEPS.with(|s| s.borrow().clone())
}

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

/// The syncable-key allowlist — portable preferences ONLY.
///
/// Widened 2026-08-25 from 33 keys to 60. Everything the Quick preview viewer learned since this
/// list was written — every playback, layout and rendering preference it has — was silently
/// stranded on one machine, along with the PDF layout, the screenshot tool defaults, the convert
/// metadata switch and half a dozen others. None of them was excluded on purpose; they simply
/// arrived after the list did, and "not synced" is what a missing entry means.
///
/// [`NEVER_SYNCED`] now names every key that stays behind, with the reason, and the test at the
/// bottom of this file reads `settings.rs` and fails on any key that is on neither list. That
/// check is the durable half of this change: widening once only helps until the next setting.
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
    ("AppTheme", Kind::Dword),
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
    // ── Widened 2026-08-25 ──────────────────────────────────────────────────────────────
    // The Quick preview viewer, in full. Every one of these is a statement about how you like
    // to read things, and not one of them travelled before now.
    ("PreviewEnabled", Kind::Dword),
    ("PreviewArrowNav", Kind::Dword),
    ("PreviewHoldPeek", Kind::Dword),
    ("PreviewCloseOnFocusLoss", Kind::Dword),
    ("PreviewOpenFront", Kind::Dword),
    ("PreviewText", Kind::Dword),
    ("PreviewMarkdown", Kind::Dword),
    ("PreviewTocOpen", Kind::Dword),
    ("PreviewMdRemoteImg", Kind::Dword),
    ("PreviewHtml", Kind::Dword),
    ("PreviewUrlLive", Kind::Dword),
    ("PreviewPdfStrip", Kind::Dword),
    ("PreviewLoop", Kind::Dword),
    ("PreviewMuted", Kind::Dword),
    ("PreviewVolume", Kind::Dword),
    ("PreviewSpeed", Kind::Dword),
    // Documents and containers.
    ("PdfLayout", Kind::Dword),
    ("PdfMarginPt", Kind::Dword),
    ("ArchiveCollage", Kind::Dword),
    // Thumbnails and the convert verbs.
    ("VideoCoverArt", Kind::Dword),
    ("VideoOffset", Kind::Dword),
    ("KeepMetadata", Kind::Dword),
    ("FolderPrebuildVerb", Kind::Dword),
    // Screenshot tool defaults (its SAVE FOLDER stays behind — see NEVER_SYNCED).
    ("ShotDefaultTool", Kind::Dword),
    ("ShotDelaySec", Kind::Dword),
    ("EyeFormat", Kind::Dword),
];

/// Every setting that deliberately does NOT sync, with the reason it doesn't.
///
/// Read by `every_setting_is_classified` below, which is the only thing that consumes it at
/// runtime. Its real job is to be the written-down decision, and to fail the build's test run
/// when a new setting has no decision yet.
#[cfg_attr(not(test), allow(dead_code))]
const NEVER_SYNCED: &[&str] = &[
    // An absolute path on THIS PC.
    "ShotSaveDir",
    // Window geometry.
    "PreviewWinW",
    "PreviewWinH",
    // Local diagnostics and dev-machine flags.
    "Debug",
    "DevMachine",
    // Install state, not a preference.
    "InstallReported",
    // Not just a value: flipping it rewrites per-ProgID registry keys on THIS machine, and the
    // ProgIDs differ per machine — so a synced 1 would record a suppression never applied here.
    "HideTypeOverlay",
    // Its successor, and it inherits the reason. `CornerMark` now carries the overlay decision
    // as well as the badge one (see `settings::CornerMark`), and two of its three values mean
    // "suppress Explorer's overlay" — which only takes effect when `typeoverlay::sync` runs
    // against THIS machine's ProgIDs. A pulled value would be recorded and never applied, so
    // the setting would read as honoured while the corner still showed the other thing. The
    // badge half used to sync on its own; it cannot any more without lying about the other half.
    "CornerMark",
    // The eyedropper's recent-colours list is CONTENT, not a preference, and it only grows.
    // `EyeFormat` (which format you want them copied in) does sync.
    "EyeHistory",
    // The sign-in prompt's own schedule (see the app's `nudge.rs`). Half of it — how long this
    // copy has been installed, how many times it has been opened — describes one machine, so
    // syncing the blob would mix two machines' histories into one and make the gate meaningless.
    "SignInNudge",
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
    let Some(resp) = sync_http_request("GET", &store_url(), &headers, &[], TIMEOUT_SECS, MAX_RESP)
    else {
        mark_offline();
        return Err("couldn't reach the sync server".to_string());
    };
    clear_offline();
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
    let mut rate_limit_retries = 0;
    loop {
        let body = serde_json::json!({ "settings": snapshot, "baseVersion": base, "merge": true });
        let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
        let Some(resp) = sync_http_request(
            "POST",
            &store_url(),
            &auth_headers(token),
            &bytes,
            TIMEOUT_SECS,
            MAX_RESP,
        ) else {
            if transient_retries < MAX_PUSH_TRANSIENT_RETRIES {
                transient_retries += 1;
                sync_sleep(Duration::from_secs(1 << (transient_retries - 1)));
                continue;
            }
            mark_offline();
            return Err("couldn't reach the sync server".to_string());
        };
        clear_offline();
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
                // E05 follow-up audit, review item 5: checked BEFORE incrementing, so
                // `MAX_PUSH_CONFLICT_RETRIES` really does mean that many RETRIES (this many
                // 409s get a re-fetch-and-retry) rather than one fewer - the old
                // post-increment `conflicts >= MAX` gave up after only two retries against a
                // constant named and documented as three.
                if conflicts >= MAX_PUSH_CONFLICT_RETRIES {
                    return Err(
                        "sync kept conflicting with another device, please try again".to_string(),
                    );
                }
                conflicts += 1;
                // Stale baseVersion, take the server's current version and retry.
                let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                base = json
                    .get("current")
                    .and_then(|c| c.get("version"))
                    .and_then(Value::as_u64)
                    .or_else(|| store_get(token).ok().map(|(v, _)| v))
                    .unwrap_or(base);
            }
            429 if rate_limit_retries < MAX_PUSH_RATE_LIMIT_RETRIES => {
                rate_limit_retries += 1;
                sync_sleep(rate_limit_wait(&resp.body));
            }
            status if status >= 500 && transient_retries < MAX_PUSH_TRANSIENT_RETRIES => {
                transient_retries += 1;
                sync_sleep(Duration::from_secs(1 << (transient_retries - 1)));
            }
            _ => return Err(store_error(resp.status, &resp.body)),
        }
    }
}

/// The one place every store call in this module actually reaches the network - GET
/// (`store_get`), POST (`push_snapshot`'s retry loop), and DELETE (`store_delete`) all go
/// through this instead of `http::request` directly.
///
/// In `#[cfg(test)]` builds it first drains [`SCRIPTED_RESPONSES`] (a thread-local queue set
/// up with [`script_response`]) before ever touching the real HTTP stack, so a test can
/// splice in scripted relay behaviour - a 409 conflict, a 429 with `retry_after_seconds`, a
/// 5xx, a dead connection - and drive the REAL `push_snapshot` retry loop end to end, rather
/// than a re-implementation of its if-lets (E05 follow-up audit, review item 4a). When the
/// queue is empty (the common case, and always in a release build) this is exactly
/// `http::request`.
fn sync_http_request(
    method: &str,
    url: &str,
    headers: &str,
    body: &[u8],
    timeout_secs: u64,
    max_resp: usize,
) -> Option<http::Resp> {
    #[cfg(test)]
    {
        if let Some(scripted) = SCRIPTED_RESPONSES.with(|q| q.borrow_mut().pop_front()) {
            return scripted.map(|(status, body)| http::Resp {
                status,
                etag: None,
                body,
            });
        }
    }
    http::request(method, url, headers, body, timeout_secs, max_resp)
}

/// The one place [`push_snapshot`]'s retry loop actually sleeps between attempts. In
/// `#[cfg(test)]` builds this records the requested duration into [`RECORDED_SLEEPS`]
/// instead of blocking the thread, so a test can assert the loop computed the RIGHT wait
/// (a capped 429 `retry_after_seconds`, an exponential transient backoff) without a real
/// test run actually waiting out up to 30 seconds per case.
fn sync_sleep(duration: Duration) {
    #[cfg(test)]
    RECORDED_SLEEPS.with(|s| s.borrow_mut().push(duration));
    #[cfg(not(test))]
    std::thread::sleep(duration);
}

fn store_delete(token: &str) -> Result<(), String> {
    let Some(resp) = sync_http_request(
        "DELETE",
        &store_url(),
        &auth_headers(token),
        &[],
        TIMEOUT_SECS,
        MAX_RESP,
    ) else {
        mark_offline();
        return Err("couldn't reach the sync server".to_string());
    };
    clear_offline();
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
///
/// E05 follow-up audit: this is the ONE place every sync entry point (`pull_on_open`,
/// `push`, `disconnect`, `retry_initial_sync`) mints a token before doing anything else, so
/// it is the single choke point for the offline classification too. A dead network
/// (`oauth::RefreshOutcome::Unreachable`) marks offline; ANY response - including a
/// rejection - clears it, because a server that answered "no" is not the same failure as a
/// server nobody could reach, and a stale marker left by an earlier STORE failure must not
/// keep reading as offline once the sign-in server has since answered.
fn access_token() -> Result<String, String> {
    let rt = cred_store::load_refresh_token().ok_or_else(|| "not signed in".to_string())?;
    let tokens = match oauth::refresh(&rt) {
        Ok(tokens) => {
            clear_offline();
            tokens
        }
        Err(outcome) => {
            let (offline, message) = classify_refresh_error(outcome);
            if offline {
                mark_offline();
            } else {
                clear_offline();
            }
            return Err(message);
        }
    };
    persist_rotation(
        tokens.refresh_token.as_deref(),
        cred_store::save_refresh_token,
    )?;
    Ok(tokens.access_token)
}

/// Pure classification, no I/O: turn `oauth::refresh`'s outcome into (a) whether it counts
/// as offline for [`mark_offline`]/[`clear_offline`] purposes and (b) the error text
/// [`access_token`] returns. Split out so the mapping itself can be pinned by a test without
/// a real network call or touching this machine's registry/portable-ini state.
fn classify_refresh_error(outcome: oauth::RefreshOutcome) -> (bool, String) {
    match outcome {
        oauth::RefreshOutcome::Unreachable => {
            (true, "couldn't reach the sign-in server".to_string())
        }
        oauth::RefreshOutcome::Rejected(message) => (false, message),
    }
}

// ---- public orchestration (UI-facing) ------------------------------------

/// Whether a (decryptable) refresh token is stored on this machine.
pub(crate) fn is_signed_in() -> bool {
    cred_store::is_signed_in()
}

// The durable "a Save's push failed, retry it" marker, kept through `settings`' own
// root-value accessors rather than a direct `CURRENT_USER` open. The direct form got two
// things wrong at once (2026-09-05 audit, F06/F07): `Key::open` hands back a READ-ONLY key
// on which `remove_value` fails silently, so a successful push never cleared the marker and
// every later Settings open re-pushed before pulling; and a portable copy wrote the marker
// into the borrowed host's HKCU, where it was shared with an installed copy and left behind,
// instead of into the ini beside the settings it describes. `settings::remove_dword`
// documents the read-only trap and routes to the ini when portable; these three are
// name-parameterised so the round-trip is testable against a scratch value name.
fn set_marker(name: &str) {
    let _ = settings::set_dword(name, 1);
}

fn clear_marker(name: &str) {
    settings::remove_dword(name);
}

fn marker_set(name: &str) -> bool {
    settings::get_dword_opt(name).is_some_and(|value| value != 0)
}

pub(crate) fn mark_push_pending() {
    set_marker(PENDING_VALUE);
}

fn clear_push_pending() {
    clear_marker(PENDING_VALUE);
}

pub(crate) fn has_pending_push() -> bool {
    marker_set(PENDING_VALUE)
}

fn mark_initial_sync_pending() {
    set_marker(INITIAL_SYNC_PENDING_VALUE);
}

fn clear_initial_sync_pending() {
    clear_marker(INITIAL_SYNC_PENDING_VALUE);
}

/// Whether a sign-in on this machine authenticated but never finished its first sync
/// (F16, 2026-09-05 audit). The Settings UI reads this to keep the button and the status
/// line in agreement instead of each guessing from `is_signed_in()` alone.
pub(crate) fn has_initial_sync_pending() -> bool {
    marker_set(INITIAL_SYNC_PENDING_VALUE)
}

/// E05 audit: a THIRD marker, alongside `PENDING_VALUE`/`INITIAL_SYNC_PENDING_VALUE` above,
/// answering a different question from either: not "is there unsent work" but "did the most
/// recent attempt even reach the server". `store_get`/`push_snapshot`/`store_delete` set it
/// the moment `http::request` returns `None` (no response at all - DNS, TCP, TLS, or a
/// timeout) and clear it the moment ANY response comes back, even a rejection, because a
/// server that answered "no" is not the same failure as a server nobody could reach. This is
/// the small classification seam `SyncState::Offline` (`settings_dlg::sync`) reads from,
/// rather than pattern-matching the English "couldn't reach the sync server" text (see
/// DEVELOPMENT_GOTCHAS.md, "a string a test parses is an API").
const OFFLINE_VALUE: &str = "ConnectionsLastAttemptOffline";

fn mark_offline() {
    set_marker(OFFLINE_VALUE);
}

fn clear_offline() {
    clear_marker(OFFLINE_VALUE);
}

/// Did the most recent sync attempt (initial sync, push, or disconnect's delete) fail to
/// reach the server at all, as opposed to the server answering with a rejection?
pub(crate) fn last_attempt_was_offline() -> bool {
    marker_set(OFFLINE_VALUE)
}

/// Whether `name` is one of this module's sync-state markers. Retry state, not a
/// preference, so the settings export/import (`settings_io`) neither exports them nor
/// lets a backup from another machine set or clear them, in either storage backend.
pub(crate) fn is_sync_state_value(name: &str) -> bool {
    name.eq_ignore_ascii_case(PENDING_VALUE)
        || name.eq_ignore_ascii_case(INITIAL_SYNC_PENDING_VALUE)
        || name.eq_ignore_ascii_case(OFFLINE_VALUE)
}

/// Name-parameterised core of [`begin_push_worker`] (E05 follow-up audit, review item 4c):
/// takes the counter explicitly so a test can drive the real increment/decrement/clear
/// logic against a throwaway counter and marker name, instead of mirroring the conditional
/// on its own. The real counter is process-global (there is only one push in-flight tally),
/// but the DECISION doesn't need to be, and testing it through the real primitive is the
/// whole point of the split.
fn begin_push_worker_on(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::AcqRel);
}

/// Name-parameterised core of [`finish_push_worker`]. `marker_name` is whichever durable
/// pending marker `success` should clear once `counter` reaches zero.
fn finish_push_worker_on(counter: &AtomicUsize, marker_name: &str, success: bool) {
    let remaining = counter.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    if success && remaining == 0 {
        clear_marker(marker_name);
    }
}

// Name-parameterised so the outstanding-worker accounting is testable against a scratch
// counter and a scratch marker name, the same pattern `set_marker`/`clear_marker`/
// `marker_set` already use, rather than the real `PUSH_WORKERS` static and the real
// `PENDING_VALUE` marker.
fn begin_push_worker_on(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::AcqRel);
}

fn finish_push_worker_on(counter: &AtomicUsize, marker_name: &str, success: bool) {
    let remaining = counter.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    if success && remaining == 0 {
        clear_marker(marker_name);
    }
}

pub(crate) fn begin_push_worker() {
    begin_push_worker_on(&PUSH_WORKERS);
}

pub(crate) fn finish_push_worker(success: bool) {
    finish_push_worker_on(&PUSH_WORKERS, PENDING_VALUE, success);
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

/// Outcome of a sign-in that DID authenticate. Kept separate from a genuine sign-in
/// failure (F16, 2026-09-05 audit): `sync_client::connect`'s HTTP round-trips used to
/// persist the refresh token and identity BEFORE the initial GET/seed, then let a failure
/// there propagate as the same `Err(String)` as a failed login. The Settings UI could then
/// only tell "signed in" from `is_signed_in()` (true, since the credential really was
/// saved) while the message box said "sign-in failed", an unrecoverable-looking
/// contradiction, and retrying meant a second browser round-trip for no reason, since the
/// credential was already good.
pub(crate) enum ConnectOutcome {
    /// Sign-in succeeded and the initial pull/seed completed too.
    Synced { label: String },
    /// Sign-in succeeded; the initial pull/seed did not. The credential is already durably
    /// stored, and nothing here should be, or needs to be, discarded.
    InitialSyncPending { label: String, error: String },
}

/// Pure decision: turn whether the initial GET/seed succeeded into a [`ConnectOutcome`].
/// No network, no credential store: [`connect`] and [`retry_initial_sync`] both funnel
/// through this so the two paths can never disagree about what a failed initial sync means.
fn connect_outcome(label: String, initial_sync: Result<(), String>) -> ConnectOutcome {
    match initial_sync {
        Ok(()) => ConnectOutcome::Synced { label },
        Err(error) => ConnectOutcome::InitialSyncPending { label, error },
    }
}

/// Record whichever marker matches `outcome`, so the NEXT Settings open (or an explicit
/// retry) knows whether a first sync still needs to happen.
fn record_initial_sync_outcome(outcome: &ConnectOutcome) {
    match outcome {
        ConnectOutcome::Synced { .. } => clear_initial_sync_pending(),
        ConnectOutcome::InitialSyncPending { .. } => mark_initial_sync_pending(),
    }
}

/// GET the cloud document and either adopt it locally (it already holds data) or seed it
/// from the current local snapshot (it's empty). This is the one decision every sync entry
/// point (`connect`'s initial sync, `retry_initial_sync`, `pull_on_open`) needs to make the
/// same way. Returns whether any local values were changed by an adopted remote document.
fn sync_once(token: &str) -> Result<bool, String> {
    let (version, settings) = store_get(token)?;
    if version > 0 {
        Ok(apply_remote(&settings) > 0)
    } else {
        push_snapshot(token)?;
        Ok(false)
    }
}

/// Interactive sign-in: browser round-trip, securely store the refresh token + identity,
/// then do the initial pull (or seed the cloud from local if it's empty). Blocking, run
/// on a worker thread. A failed initial sync is reported as
/// [`ConnectOutcome::InitialSyncPending`], never as an `Err`. The sign-in itself worked,
/// and the caller must not treat this like a failed login (F16, 2026-09-05 audit).
pub(crate) fn connect() -> Result<ConnectOutcome, String> {
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

    let mut who = if !name.is_empty() {
        name
    } else if !email.is_empty() {
        email
    } else {
        sub
    };
    // Issue #227/G117: the credential and identity this sign-in just wrote now live in the
    // portable ini (see `cred_store`'s doc), not this PC's own account — say so on the very
    // screen that reports success, since that is the one moment every sign-in path (the
    // banner, the sync button, a credential this machine already had) is guaranteed to pass
    // through. Folded into the returned label rather than a second dialog: the caller
    // (`settings_dlg::sync`) already shows this string in its "signed in" message box(es).
    if settings::portable() {
        who = format!("{who}\n\n{}", crate::win::t("sync_portable_notice"));
    }

    let outcome = connect_outcome(who, sync_once(&tokens.access_token).map(|_| ()));
    record_initial_sync_outcome(&outcome);
    Ok(outcome)
}

/// Retry the initial sync after a `connect()` that authenticated but left
/// [`ConnectOutcome::InitialSyncPending`] (F16). Reuses the refresh token `connect` already
/// stored via [`access_token`], never calls `oauth::login`, so retrying costs no second
/// browser round-trip.
pub(crate) fn retry_initial_sync() -> Result<ConnectOutcome, String> {
    let _guard = sync_guard();
    let token = access_token()?;
    let label = signed_in_label().unwrap_or_default();
    let outcome = connect_outcome(label, sync_once(&token).map(|_| ()));
    record_initial_sync_outcome(&outcome);
    Ok(outcome)
}

/// Pull remote settings and apply them locally; seed the cloud if it's empty. Returns
/// `Ok(true)` if any values were applied (so the UI should refresh its controls).
/// Blocking, run on a worker thread. Also clears the initial-sync-pending marker on
/// success, so simply reopening Settings is itself a valid, login-free retry path.
pub(crate) fn pull_on_open() -> Result<bool, String> {
    let _guard = sync_guard();
    let token = access_token()?;
    if has_pending_push() {
        push_snapshot(&token)?;
        clear_push_pending();
    }
    let applied = sync_once(&token)?;
    clear_initial_sync_pending();
    Ok(applied)
}

/// Push the current local allowlisted settings to the cloud. Blocking — run on a worker
/// thread (called after the user applies settings changes).
pub(crate) fn push() -> Result<(), String> {
    let _guard = sync_guard();
    let token = access_token()?;
    push_snapshot(&token).map(|_| ())
}

/// Whether `disconnect`'s best-effort cloud-copy delete actually reached and succeeded
/// against the server (E05 audit). The credential and local markers are ALWAYS forgotten
/// regardless, disconnecting locally must not fail just because the network is down,
/// this exists so the UI can say honestly when the server copy might still be there,
/// rather than silently discarding a real failure the way `disconnect` used to (the whole
/// result was `let _ = store_delete(&token);`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisconnectOutcome {
    /// The server confirmed the document is gone (or was already gone).
    CloudCopyDeleted,
    /// Nothing to delete, this machine had no usable credential to delete with.
    WasNotSignedIn,
    /// The delete request reached the server and it refused, or never reached the server
    /// at all, [`last_attempt_was_offline`] tells the two apart if a caller needs to.
    CloudCopyKept,
}

/// Pure decision, no I/O (E05 follow-up audit, review item 2): turn "was this machine
/// signed in beforehand" plus "did the delete even get attempted, and how did it go" into a
/// [`DisconnectOutcome`]. [`disconnect`] funnels through this so the outcome can be pinned
/// without a real credential store or network.
///
/// `delete_attempted` is `None` when `access_token()` never even produced a token to delete
/// with (no credential, offline, or the sign-in server rejected the refresh), that is
/// `WasNotSignedIn` ONLY when this machine genuinely had no credential (`was_signed_in ==
/// false`); a signed-in machine whose token mint merely failed still has a cloud copy that
/// was never touched, so it must read as `CloudCopyKept`, never as "was never signed in."
fn disconnect_outcome(
    was_signed_in: bool,
    delete_attempted: Option<Result<(), String>>,
) -> DisconnectOutcome {
    match delete_attempted {
        Some(Ok(())) => DisconnectOutcome::CloudCopyDeleted,
        Some(Err(_)) => DisconnectOutcome::CloudCopyKept,
        None if was_signed_in => DisconnectOutcome::CloudCopyKept,
        None => DisconnectOutcome::WasNotSignedIn,
    }
}

/// Disconnect: best-effort delete the remote doc, then forget local credentials. The
/// remote delete is genuinely best-effort (no token, no connectivity, or the store
/// rejecting the request all fall through here), so the UI wording that invites this
/// action must not promise cloud erasure as a guarantee (2026-09-05 audit, F16 note), but
/// the RETURNED outcome, unlike the old `()`, lets the caller say so honestly instead of
/// claiming success it doesn't know it had.
pub(crate) fn disconnect() -> DisconnectOutcome {
    let _guard = sync_guard();
    // Read BEFORE `access_token()` can fail and BEFORE `cred_store::clear()` below removes
    // it, see `disconnect_outcome`'s doc for why this matters.
    let was_signed_in = cred_store::is_signed_in();
    let delete_attempted = access_token().ok().map(|token| store_delete(&token));
    let outcome = disconnect_outcome(was_signed_in, delete_attempted);
    cred_store::clear();
    clear_cache();
    clear_push_pending();
    clear_initial_sync_pending();
    clear_offline();
    outcome
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

    // ---- the classification guard ------------------------------------------------------

    /// `settings.rs` verbatim, embedded at COMPILE time.
    ///
    /// Reading it from disk at runtime would make this test depend on the working directory,
    /// which differs between `cargo test`, the CI job and a packaged run. `include_str!` resolves
    /// relative to THIS file, so the path is checked by the compiler and cannot silently miss.
    const SETTINGS_SRC: &str = include_str!("../../settings.rs");

    /// Every setting name `settings.rs` reads or writes.
    ///
    /// Deliberately a dumb scan for `…("Name"` after one of the registry accessors, rather than
    /// anything clever: a clever matcher that stops matching is indistinguishable from a repo
    /// with nothing left to classify, and `the_scan_actually_finds_settings` below is what keeps
    /// that from rotting into a no-op.
    fn settings_in_source() -> Vec<String> {
        const ACCESSORS: &[&str] = &[
            "get_dword(",
            "get_dword_opt(",
            "set_dword(",
            "remove_dword(",
            "set_dword_tracking_default(",
            "get_string_opt(",
            "set_string(",
        ];
        let mut names = Vec::new();
        for accessor in ACCESSORS {
            let mut rest = SETTINGS_SRC;
            while let Some(at) = rest.find(accessor) {
                rest = &rest[at + accessor.len()..];
                let trimmed = rest.trim_start();
                let Some(body) = trimmed.strip_prefix('"') else {
                    continue; // a variable, not a literal — nothing to classify from here
                };
                let Some(end) = body.find('"') else { continue };
                let name = &body[..end];
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !names.iter().any(|seen: &String| seen == name)
                {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        names
    }

    /// A settings key must be either synced or deliberately not — never merely forgotten.
    ///
    /// This is the check the 2026-08-25 widening was really about. `ALLOW` was written when the
    /// app had 33 settings and every one added afterwards defaulted to "does not sync", silently,
    /// including the entire Quick preview viewer. Nothing was ever red about it. Now a new
    /// setting fails here until someone files it into one list or the other on purpose.
    #[test]
    fn every_setting_is_classified() {
        let allowed: Vec<&str> = ALLOW.iter().map(|(k, _)| *k).collect();
        let unclassified: Vec<String> = settings_in_source()
            .into_iter()
            .filter(|k| !allowed.contains(&k.as_str()) && !NEVER_SYNCED.contains(&k.as_str()))
            .collect();
        assert!(
            unclassified.is_empty(),
            "these settings sync neither way — add each to ALLOW or NEVER_SYNCED: {unclassified:?}"
        );
    }

    /// The guard above is worth nothing if the scan silently finds nothing, so pin that it works.
    #[test]
    fn the_scan_actually_finds_settings() {
        let found = settings_in_source();
        assert!(
            found.len() > 50,
            "the settings scan found only {} names — the accessor list or the file shape changed",
            found.len()
        );
        for expected in ["EnableThumbs", "ShotSaveDir", "PreviewEnabled", "Lang"] {
            assert!(
                found.iter().any(|k| k == expected),
                "the scan missed {expected}, so it is no longer reading settings.rs correctly"
            );
        }
    }

    /// A key on both lists would make its exclusion meaningless, and nothing else would notice.
    #[test]
    fn no_setting_is_on_both_lists() {
        for (key, _) in ALLOW {
            assert!(
                !NEVER_SYNCED.contains(key),
                "{key} is in ALLOW and NEVER_SYNCED at the same time"
            );
        }
    }

    /// F06: the marker must actually CLEAR. `clear` used to go through a read-only
    /// `Key::open`, on which `remove_value` fails silently, so a successful push left the
    /// marker set for good and every later Settings open re-pushed. Round-trips a scratch
    /// name so the developer's real marker is never touched, and leaves nothing behind.
    #[test]
    fn the_pending_marker_clears_after_a_successful_push() {
        let name = format!("ConnectionsSyncPendingTest{}", std::process::id());
        clear_marker(&name);
        assert!(!marker_set(&name), "a never-set marker reads as clear");
        set_marker(&name);
        assert!(marker_set(&name), "mark must read back");
        clear_marker(&name);
        assert!(
            !marker_set(&name),
            "clear must DELETE the value, not fail silently on a read-only key"
        );
    }

    /// F06, the actual clearing condition: `finish_push_worker` only clears the pending
    /// marker when the push that just finished both succeeded AND was the last outstanding
    /// worker. `the_pending_marker_clears_after_a_successful_push` above only round-trips
    /// `set_marker`/`clear_marker` directly and never calls `begin_push_worker`/
    /// `finish_push_worker` at all, so it would stay green even if the `success &&
    /// remaining == 0` guard were removed or inverted. This drives the worker-accounting
    /// helpers themselves, against a scratch counter and a scratch marker name so the real
    /// `PUSH_WORKERS` static and the real marker are never touched.
    #[test]
    fn finish_push_worker_only_clears_the_marker_on_a_successful_last_finish() {
        let name = format!("ConnectionsSyncPendingWorkerTest{}", std::process::id());
        let counter = AtomicUsize::new(0);
        clear_marker(&name);

        // A single worker that fails must leave the marker set for retry.
        set_marker(&name);
        begin_push_worker_on(&counter);
        finish_push_worker_on(&counter, &name, false);
        assert!(
            marker_set(&name),
            "a failed push must not clear the pending marker"
        );

        // A fresh single worker finishing successfully must clear it.
        begin_push_worker_on(&counter);
        finish_push_worker_on(&counter, &name, true);
        assert!(
            !marker_set(&name),
            "the last outstanding worker succeeding must clear the pending marker"
        );

        // Two outstanding workers: the first to finish (even successfully) must not clear
        // the marker while the other is still outstanding.
        set_marker(&name);
        begin_push_worker_on(&counter);
        begin_push_worker_on(&counter);
        finish_push_worker_on(&counter, &name, true);
        assert!(
            marker_set(&name),
            "the marker must stay set while another worker is still outstanding"
        );
        finish_push_worker_on(&counter, &name, true);
        assert!(
            !marker_set(&name),
            "the marker must clear once the last outstanding worker finishes successfully"
        );

        clear_marker(&name);
    }

    /// The marker is state, not a preference: the settings export/import consults this to
    /// leave it alone, case-insensitively like every registry value name.
    #[test]
    fn the_pending_marker_is_classified_as_sync_state() {
        assert!(is_sync_state_value(PENDING_VALUE));
        assert!(is_sync_state_value("connectionssyncpending"));
        assert!(!is_sync_state_value("Theme"));
        assert!(!is_sync_state_value("ConnectionsSyncPendingX"));
    }

    // ---- F16: initial-sync-pending state (2026-09-05 audit) ----------------------------

    /// The new marker round-trips through the same name-parameterised primitives the
    /// push-pending marker already proved (`the_pending_marker_clears_after_a_successful_push`
    /// above), a scratch name, never the real `INITIAL_SYNC_PENDING_VALUE` key, so the
    /// developer's real sync state is never touched.
    #[test]
    fn the_initial_sync_pending_marker_round_trips() {
        let name = format!("ConnectionsInitialSyncPendingTest{}", std::process::id());
        clear_marker(&name);
        assert!(!marker_set(&name), "a never-set marker reads as clear");
        set_marker(&name);
        assert!(marker_set(&name), "mark must read back");
        clear_marker(&name);
        assert!(!marker_set(&name), "clear must delete the value");
    }

    #[test]
    fn the_initial_sync_pending_marker_is_classified_as_sync_state() {
        assert!(is_sync_state_value(INITIAL_SYNC_PENDING_VALUE));
        assert!(is_sync_state_value("connectionsinitialsyncpending"));
    }

    #[test]
    fn connect_outcome_is_synced_when_the_initial_sync_succeeds() {
        match connect_outcome("Ann".to_string(), Ok(())) {
            ConnectOutcome::Synced { label } => assert_eq!(label, "Ann"),
            ConnectOutcome::InitialSyncPending { .. } => {
                panic!("a successful initial sync must not read as pending")
            }
        }
    }

    /// This is the exact contradiction the 2026-09-05 audit (F16) found: `connect()` saves
    /// the refresh token and identity BEFORE the initial GET/seed, then a failure there
    /// used to propagate as a plain `Err`, indistinguishable from a failed login, even
    /// though the credential was already durably stored. Against the pre-fix `connect`,
    /// there was no `ConnectOutcome` at all (the return type was `Result<String, String>`),
    /// so this test could not even compile, let alone pass: it fails against that shape and
    /// passes once a failed initial sync is reported as `InitialSyncPending` rather than a
    /// bare error that looks like "sign-in failed".
    #[test]
    fn connect_outcome_is_initial_sync_pending_when_the_initial_sync_fails() {
        let outcome = connect_outcome("Ann".to_string(), Err("network unreachable".to_string()));
        match outcome {
            ConnectOutcome::InitialSyncPending { label, error } => {
                assert_eq!(label, "Ann");
                assert_eq!(error, "network unreachable");
            }
            ConnectOutcome::Synced { .. } => {
                panic!("a failed initial sync must not silently read as fully synced")
            }
        }
    }

    // ---- E05: the offline classification marker + the named retry bounds ---------------

    /// Same shape as `the_initial_sync_pending_marker_round_trips` above: exercises the
    /// real `set_marker`/`clear_marker`/`marker_set` primitives `mark_offline`/
    /// `clear_offline`/`last_attempt_was_offline` are thin wrappers over, via a scratch
    /// name so the developer's real offline flag is never touched.
    #[test]
    fn the_offline_marker_round_trips() {
        let name = format!("ConnectionsLastAttemptOfflineTest{}", std::process::id());
        clear_marker(&name);
        assert!(!marker_set(&name), "a never-set marker reads as clear");
        set_marker(&name);
        assert!(marker_set(&name), "mark must read back");
        clear_marker(&name);
        assert!(!marker_set(&name), "clear must delete the value");
    }

    #[test]
    fn the_offline_marker_is_classified_as_sync_state() {
        assert!(is_sync_state_value(OFFLINE_VALUE));
        assert!(is_sync_state_value("connectionslastattemptoffline"));
        assert!(!is_sync_state_value(OFFLINE_VALUE.trim_end_matches('e')));
    }

    /// The retry bounds are supposed to be STATED facts, not accidents of a hardcoded
    /// literal buried in a loop, pin the exact numbers so a future edit that quietly
    /// changes one shows up as a diff to a named test, not a silent behavior change.
    #[test]
    fn the_named_retry_bounds_have_the_documented_values() {
        assert_eq!(MAX_PUSH_TRANSIENT_RETRIES, 2);
        assert_eq!(MAX_PUSH_CONFLICT_RETRIES, 3);
        assert_eq!(MAX_PUSH_RATE_LIMIT_RETRIES, 1);
        assert_eq!(MAX_RATE_LIMIT_WAIT, Duration::from_secs(30));
    }

    /// The exact invariant `finish_push_worker` relies on ("success clears the marker,
    /// failure never does"), driven through the REAL [`begin_push_worker_on`]/
    /// [`finish_push_worker_on`] primitives (E05 follow-up audit, review item 4c) against a
    /// throwaway counter and marker name, rather than mirroring their conditional in the
    /// test itself. Against the pre-split shape (a bare `if success && remaining == 0`
    /// inline in `finish_push_worker`, addressing `PENDING_VALUE` unconditionally) there was
    /// no name-parameterised function to call here at all.
    #[test]
    fn a_failed_push_never_clears_the_pending_marker_only_a_successful_one_does() {
        let name = format!("ConnectionsSyncPendingMirrorTest{}", std::process::id());
        let counter = AtomicUsize::new(0);
        clear_marker(&name);
        set_marker(&name); // mirrors `mark_push_pending()` after Save

        begin_push_worker_on(&counter);
        finish_push_worker_on(&counter, &name, false);
        assert!(
            marker_set(&name),
            "a failed push must leave the retry marker set for the next Settings open"
        );

        begin_push_worker_on(&counter);
        finish_push_worker_on(&counter, &name, true);
        assert!(
            !marker_set(&name),
            "a successful push with no other worker in flight must clear it"
        );
    }

    #[test]
    fn disconnect_outcome_variants_are_distinguishable() {
        assert_ne!(
            DisconnectOutcome::CloudCopyDeleted,
            DisconnectOutcome::CloudCopyKept
        );
        assert_ne!(
            DisconnectOutcome::WasNotSignedIn,
            DisconnectOutcome::CloudCopyKept
        );
    }

    /// E05 follow-up audit, review item 2: the exact bug, a signed-in machine whose token
    /// refresh failed (offline, or the sign-in server rejected it) used to report
    /// `WasNotSignedIn`, the same "nothing to delete" wording as a machine that was never
    /// signed in at all, even though the cloud copy was never touched. Against the pre-fix
    /// `disconnect` (no `was_signed_in` capture, `Err(_) => WasNotSignedIn` unconditionally)
    /// this is exactly the case that read wrong.
    #[test]
    fn a_signed_in_machine_whose_token_mint_failed_keeps_the_cloud_copy_not_was_not_signed_in() {
        assert_eq!(
            disconnect_outcome(true, None),
            DisconnectOutcome::CloudCopyKept,
            "signed in, but no token to delete with, must not read as never-signed-in"
        );
    }

    #[test]
    fn a_machine_that_was_never_signed_in_reports_was_not_signed_in() {
        assert_eq!(
            disconnect_outcome(false, None),
            DisconnectOutcome::WasNotSignedIn
        );
    }

    #[test]
    fn a_successful_delete_reports_cloud_copy_deleted_regardless_of_prior_state() {
        assert_eq!(
            disconnect_outcome(true, Some(Ok(()))),
            DisconnectOutcome::CloudCopyDeleted
        );
    }

    /// E05 follow-up audit, review item 1: the exact bug, every sync entry point called
    /// `access_token()`, which on a dead network returned `Err(...)` WITHOUT ever calling
    /// `mark_offline()`, so a dead network plus Save rendered `SavedLocally`, never
    /// `Offline`. Against the pre-fix `access_token` (a bare `oauth::refresh(&rt)?` with no
    /// classification at all) there was no such mapping to call here.
    #[test]
    fn an_unreachable_refresh_classifies_as_offline_with_the_reach_error_text() {
        assert_eq!(
            classify_refresh_error(oauth::RefreshOutcome::Unreachable),
            (true, "couldn't reach the sign-in server".to_string())
        );
    }

    /// The other half of item 1: ANY response, including a rejection, must clear the
    /// offline classification, because a server that answered "no" is not the same failure
    /// as a server nobody could reach.
    #[test]
    fn a_rejected_refresh_classifies_as_not_offline_with_the_servers_own_message() {
        assert_eq!(
            classify_refresh_error(oauth::RefreshOutcome::Rejected(
                "bad refresh token".to_string()
            )),
            (false, "bad refresh token".to_string())
        );
    }

    #[test]
    fn a_failed_delete_attempt_keeps_the_cloud_copy() {
        assert_eq!(
            disconnect_outcome(true, Some(Err("sync failed (HTTP 500)".to_string()))),
            DisconnectOutcome::CloudCopyKept
        );
    }

    // ---- E05 follow-up audit: fake-server tests driving the REAL push_snapshot loop -----

    /// A GET response for `store_get` carrying the given version and empty settings, plus
    /// however many scripted responses the caller queues after it, every scenario below
    /// starts with `push_snapshot` fetching a base version before its retry loop even begins.
    fn script_base_version(version: u64) {
        script_response(Some((
            200,
            format!(r#"{{"version":{version},"settings":{{}}}}"#).into_bytes(),
        )));
    }

    fn conflict_body(current_version: u64) -> Vec<u8> {
        format!(r#"{{"current":{{"version":{current_version}}}}}"#).into_bytes()
    }

    /// Item 4a(i): a 409 is retried up to the bound, then the push gives up. Also pins the
    /// fix for item 5, `MAX_PUSH_CONFLICT_RETRIES` (3) must mean three RETRIES (four total
    /// conflict responses before giving up), not two. Against the pre-fix `conflicts >= 3`
    /// checked AFTER incrementing, this test's fourth queued 409 would never be reached ,
    /// the push gave up on the THIRD one instead.
    #[test]
    fn push_retries_a_409_conflict_up_to_the_bound_then_gives_up() {
        reset_test_network_state();
        script_base_version(1);
        for v in 2..=5 {
            script_response(Some((409, conflict_body(v))));
        }
        let err = push_snapshot("tok").unwrap_err();
        assert!(
            err.contains("kept conflicting"),
            "must give up with the conflict message, got {err:?}"
        );
    }

    /// The other half of item 5: exactly `MAX_PUSH_CONFLICT_RETRIES` conflicts, then a
    /// success, must succeed, proving the bound really does grant that many retries rather
    /// than one fewer.
    #[test]
    fn push_succeeds_after_exactly_the_conflict_bound_worth_of_retries() {
        reset_test_network_state();
        script_base_version(1);
        for v in 2..=(1 + u64::from(MAX_PUSH_CONFLICT_RETRIES)) {
            script_response(Some((409, conflict_body(v))));
        }
        script_response(Some((200, br#"{"version":99}"#.to_vec())));
        assert_eq!(push_snapshot("tok"), Ok(99));
    }

    /// Item 4a(ii): a 429 honours the relay's `retry_after_seconds`, capped by
    /// `MAX_RATE_LIMIT_WAIT`, asserted on the computed wait via [`recorded_sleeps`], never
    /// by actually sleeping.
    #[test]
    fn push_honours_retry_after_capped_by_the_rate_limit_ceiling() {
        reset_test_network_state();
        script_base_version(1);
        script_response(Some((429, br#"{"retry_after_seconds":9999}"#.to_vec())));
        script_response(Some((200, br#"{"version":2}"#.to_vec())));
        assert_eq!(push_snapshot("tok"), Ok(2));
        assert_eq!(
            recorded_sleeps(),
            vec![MAX_RATE_LIMIT_WAIT],
            "an outrageous retry_after_seconds must be capped, not honoured verbatim"
        );
    }

    #[test]
    fn push_honours_retry_after_when_it_is_under_the_cap() {
        reset_test_network_state();
        script_base_version(1);
        script_response(Some((429, br#"{"retry_after_seconds":5}"#.to_vec())));
        script_response(Some((200, br#"{"version":2}"#.to_vec())));
        assert_eq!(push_snapshot("tok"), Ok(2));
        assert_eq!(recorded_sleeps(), vec![Duration::from_secs(5)]);
    }

    /// Item 4a(iii): a 5xx is retried up to `MAX_PUSH_TRANSIENT_RETRIES`, then gives up.
    #[test]
    fn push_retries_a_5xx_up_to_the_transient_bound_then_gives_up() {
        reset_test_network_state();
        script_base_version(1);
        for _ in 0..=MAX_PUSH_TRANSIENT_RETRIES {
            script_response(Some((503, b"{}".to_vec())));
        }
        let err = push_snapshot("tok").unwrap_err();
        assert!(
            err.contains("sync failed"),
            "must give up with the store's own error mapping, got {err:?}"
        );
    }

    /// Item 4a(iv): no response at all (`None`) is retried as a transient failure and, once
    /// the bound is exhausted, `push_snapshot` reports the SAME "couldn't reach" wording
    /// `mark_offline` is paired with everywhere else in this module (see the
    /// `store_get`/`store_delete` branches, and `access_token`'s own classification test) -
    /// deliberately NOT asserted here via the real `last_attempt_was_offline()` marker, so
    /// this test never writes to this machine's actual registry/portable-ini state.
    #[test]
    fn push_gives_up_with_the_offline_wording_after_exhausting_transient_retries_on_no_response() {
        reset_test_network_state();
        script_base_version(1);
        for _ in 0..=MAX_PUSH_TRANSIENT_RETRIES {
            script_response(None);
        }
        let err = push_snapshot("tok").unwrap_err();
        assert_eq!(err, "couldn't reach the sync server");
    }
}
