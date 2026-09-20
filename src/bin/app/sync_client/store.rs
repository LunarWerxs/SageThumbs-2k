//! The settings store over HTTP: get / push / delete with ETags, the rate-limit waits, and error text.

use super::*;

pub(super) fn store_url() -> String {
    format!("{STORE_BASE}/{}", oauth::CLIENT_ID)
}

pub(super) fn auth_headers(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\nContent-Type: application/json")
}

pub(super) fn auth_headers_with_etag(token: &str, etag: Option<&str>) -> String {
    match etag {
        Some(etag) => format!("{}\r\nIf-None-Match: {etag}", auth_headers(token)),
        None => auth_headers(token),
    }
}

pub(super) fn parse_etag_version(etag: &str) -> Option<u64> {
    etag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

pub(super) fn clear_cache() {
    *cache() = None;
}

/// GET the current doc → `(version, settings)`. A never-written user is `(0, {})`.
/// Repeated reads are ETag-conditional; 304 reuses the cached document.
pub(super) fn store_get(token: &str) -> Result<(u64, Value), String> {
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
pub(super) fn push_snapshot(token: &str) -> Result<u64, String> {
    let snapshot = Value::Object(read_local());
    validate_sync_snapshot(&snapshot)?;
    let mut base = store_get(token)?.0;
    let mut conflicts = 0;
    let mut transient_retries = 0;
    let mut rate_limit_retries = 0;
    loop {
        if let Some(version) = push_attempt(
            token,
            &snapshot,
            &mut base,
            &mut conflicts,
            &mut transient_retries,
            &mut rate_limit_retries,
        )? {
            return Ok(version);
        }
    }
}

/// One attempt of [`push_snapshot`]'s retry loop: POST the snapshot, fold the server's
/// reply into the retry counters and `base`, and report `Ok(Some(version))` when the loop
/// is finished or `Ok(None)` when it should try again.
fn push_attempt(
    token: &str,
    snapshot: &Value,
    base: &mut u64,
    conflicts: &mut u32,
    transient_retries: &mut u32,
    rate_limit_retries: &mut u32,
) -> Result<Option<u64>, String> {
    let body = serde_json::json!({ "settings": snapshot, "baseVersion": *base, "merge": true });
    let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let Some(resp) = sync_http_request(
        "POST",
        &store_url(),
        &auth_headers(token),
        &bytes,
        TIMEOUT_SECS,
        MAX_RESP,
    ) else {
        if *transient_retries < MAX_PUSH_TRANSIENT_RETRIES {
            *transient_retries += 1;
            sync_sleep(Duration::from_secs(1 << (*transient_retries - 1)));
            return Ok(None);
        }
        mark_offline();
        return Err("couldn't reach the sync server".to_string());
    };
    clear_offline();
    match resp.status {
        200 => {
            let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            clear_cache();
            Ok(Some(json
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(*base + 1)))
        }
        409 => {
            // E05 follow-up audit, review item 5: checked BEFORE incrementing, so
            // `MAX_PUSH_CONFLICT_RETRIES` really does mean that many RETRIES (this many
            // 409s get a re-fetch-and-retry) rather than one fewer - the old
            // post-increment `conflicts >= MAX` gave up after only two retries against a
            // constant named and documented as three.
            if *conflicts >= MAX_PUSH_CONFLICT_RETRIES {
                return Err(
                    "sync kept conflicting with another device, please try again".to_string(),
                );
            }
            *conflicts += 1;
            // Stale baseVersion, take the server's current version and retry.
            let json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
            *base = json
                .get("current")
                .and_then(|c| c.get("version"))
                .and_then(Value::as_u64)
                .or_else(|| store_get(token).ok().map(|(v, _)| v))
                .unwrap_or(*base);
            Ok(None)
        }
        429 if *rate_limit_retries < MAX_PUSH_RATE_LIMIT_RETRIES => {
            *rate_limit_retries += 1;
            sync_sleep(rate_limit_wait(&resp.body));
            Ok(None)
        }
        status if status >= 500 && *transient_retries < MAX_PUSH_TRANSIENT_RETRIES => {
            *transient_retries += 1;
            sync_sleep(Duration::from_secs(1 << (*transient_retries - 1)));
            Ok(None)
        }
        _ => Err(store_error(resp.status, &resp.body)),
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
pub(super) fn sync_http_request(
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
pub(super) fn sync_sleep(duration: Duration) {
    #[cfg(test)]
    RECORDED_SLEEPS.with(|s| s.borrow_mut().push(duration));
    #[cfg(not(test))]
    std::thread::sleep(duration);
}

pub(super) fn store_delete(token: &str) -> Result<(), String> {
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
pub(super) fn store_error(status: u16, body: &[u8]) -> String {
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

pub(super) fn retry_after_seconds(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("retry_after_seconds")?
        .as_u64()
        .filter(|seconds| *seconds > 0)
}

pub(super) fn rate_limit_wait(body: &[u8]) -> Duration {
    Duration::from_secs(retry_after_seconds(body).unwrap_or(1)).min(MAX_RATE_LIMIT_WAIT)
}

pub(super) fn ascii_tail(
    value: &str,
    prefix: &str,
    min: usize,
    allowed: impl Fn(u8) -> bool,
) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|tail| tail.len() >= min && tail.bytes().all(allowed))
}
