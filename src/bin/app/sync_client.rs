//! Connections settings-sync: the allowlisted push/pull between the local HKCU settings
//! and the per-user cloud document at `studio.connectionsapi.com/v1/app-data/{appId}`
//! (the permanent backend domain since 2026-09-18; `connections.icu` was suspended).
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
mod store;
use store::*;
mod markers;
use markers::*;
mod allowlist;
use allowlist::*;
pub(crate) use markers::{
    begin_push_worker, finish_push_worker, has_initial_sync_pending, has_pending_push,
    is_sync_state_value, last_attempt_was_offline, mark_push_pending,
};

use serde_json::{Map, Value};

use st2k_base::settings;

use crate::{cred_store, http, oauth};

const STORE_BASE: &str = "https://studio.connectionsapi.com/v1/app-data";
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
/// write arbitrary registry values). Returns how many values were applied, or an error
/// naming every accepted value the LOCAL store refused to take.
///
/// Same portable-redirect reasoning as [`read_local`]: writing straight to `CURRENT_USER`
/// meant a pulled setting never reached a portable install's actual backing store (the
/// ini), so `pull_on_open` silently applied zero values there every time.
///
/// Until 2026-09-19 this only COUNTED the writes that succeeded, and `sync_once` wrapped
/// the count in `Ok`, so a read-only ini or a registry key the user cannot write reported a
/// clean sync while nothing had changed locally (audit concern 8). A value the remote
/// document holds in the wrong shape is still simply rejected - that is the hostile-doc
/// rule, not a local failure - but a value we accepted and could not write is an error.
fn apply_remote(settings_obj: &Value) -> Result<u32, String> {
    apply_remote_with(
        settings_obj,
        |name, n| settings::set_dword(name, n).map_err(|e| e.to_string()),
        |name, s| settings::set_string(name, s).map_err(|e| e.to_string()),
    )
}

/// [`apply_remote`] with the two local setters injected, so a test can make the local
/// store fail without a read-only registry or a second process for the portable ini
/// (the ini path is resolved once per process).
fn apply_remote_with(
    settings_obj: &Value,
    set_dword: impl Fn(&str, u32) -> Result<(), String>,
    set_string: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<u32, String> {
    let Some(obj) = settings_obj.as_object() else {
        return Ok(0);
    };
    let mut applied = 0;
    let mut failed: Vec<String> = Vec::new();
    for (name, kind) in ALLOW {
        match apply_entry(name, *kind, obj, &set_dword, &set_string) {
            Some(Ok(())) => applied += 1,
            Some(Err(e)) => failed.push(format!("{name} ({e})")),
            None => {}
        }
    }
    if failed.is_empty() {
        Ok(applied)
    } else {
        Err(format!(
            "{} pulled setting(s) could not be written to this PC's settings store ({applied} were): {}",
            failed.len(),
            failed.join(", ")
        ))
    }
}

/// Apply ONE allowlisted remote entry: look it up on `obj`, reject a wrong-shaped value
/// (the hostile-doc rule), and run it through the matching injected setter. Returns `None`
/// when the remote omits the key or holds it in the wrong shape, `Some(Ok)`/`Some(Err)`
/// with the setter's own result otherwise.
fn apply_entry(
    name: &str,
    kind: Kind,
    obj: &Map<String, Value>,
    set_dword: &impl Fn(&str, u32) -> Result<(), String>,
    set_string: &impl Fn(&str, &str) -> Result<(), String>,
) -> Option<Result<(), String>> {
    let val = obj.get(name)?;
    Some(match kind {
        // `u32::try_from` rather than `as u32`: an out-of-range remote value (a hostile
        // or corrupted doc) must be REJECTED, not silently truncated and then counted as
        // a successful apply - `as u32` on 4294967296 would wrap to 0 and still report
        // success, writing a value the remote document never actually held.
        Kind::Dword => set_dword(name, val.as_u64().and_then(|n| u32::try_from(n).ok())?),
        Kind::Str => set_string(name, val.as_str()?),
    })
}

// ---- store transport -----------------------------------------------------

fn credential_string(value: &str) -> Option<&'static str> {
    let value = value.trim();
    let alpha_num_dash = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');

    if let Some(what) = prefixed_provider_credential(value) {
        return Some(what);
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

/// Recognize a well-known prefixed provider token or key id at the start of `value`
/// (Stripe/OpenAI/GitHub/Slack tokens, an AWS access key id), or `None`.
fn prefixed_provider_credential(value: &str) -> Option<&'static str> {
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
        // A local write that fails is a failed sync, not a quiet zero (audit concern 8).
        Ok(apply_remote(&settings)? > 0)
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
    /// The delete request reached the server and it refused, or never reached the server at all.
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
mod tests;
