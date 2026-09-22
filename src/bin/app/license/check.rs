//! The periodic entitlement check: throttle, the response parser and applying what it says.

use super::*;

/// How long a successful (or attempted) entitlement check holds off the next one.
/// The check is a network call on every launch's critical-ish path; 6 hours keeps a
/// business machine's status fresh without hammering the relay every open.
pub(super) const REFRESH_THROTTLE_SECS: u64 = 6 * 60 * 60;

/// Pure throttle decision: due when nothing has been recorded yet, or the last
/// attempt (success OR failure - see [`refresh_entitlement`]) is more than
/// [`REFRESH_THROTTLE_SECS`] old. Saturating, so a clock that jumped backwards just
/// means "not due yet," never a panic.
pub(super) fn refresh_due(now_unix: u64, last_check_unix: u64) -> bool {
    last_check_unix == 0 || now_unix.saturating_sub(last_check_unix) >= REFRESH_THROTTLE_SECS
}

/// What `GET /license/check` actually answers with, decoupled from the breadcrumb so
/// [`parse_check_response`] stays a pure function of the response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CheckResult {
    pub(super) entitled: bool,
    pub(super) status: String,
    /// The relay's optional `reason` token (a short `[a-z0-9_]` word such as
    /// `seat_revoked` / `contract_ended`), kept only when it has that shape.
    pub(super) reason: Option<String>,
    /// The relay's `maintenanceActive` flag: is this seat still inside its 12 months of
    /// updates. `None` when the relay sent none (an older deployment, or a Pay that does
    /// not answer it) - which is "unknown", never "no".
    ///
    /// Carried for completeness and for the debug log; `maint_unix` is what every decision
    /// actually reads, because a DATE can be compared against a release's publication date
    /// and a boolean cannot.
    pub(super) maintenance_active: Option<bool>,
    /// `maintenanceEndsAt` parsed to Unix seconds. `None` for absent, null, or anything
    /// that does not parse - all of which mean "no window on record" and leave every build
    /// offered.
    pub(super) maint_unix: Option<u64>,
}

/// Parse the ISO-8601 instant the relay sends for `maintenanceEndsAt` into Unix seconds.
///
/// Deliberately narrow, and pure so the boundaries can be pinned in tests: `YYYY-MM-DD`
/// optionally followed by `T`/space, `HH:MM[:SS]`, an optional fractional part, and an
/// optional `Z`. An offset other than `Z` is REFUSED rather than silently read as UTC -
/// this value decides whether a customer is offered a build, so guessing hours is worse
/// than answering "no window on record", which is the safe direction (every build stays
/// offered). Adding a full offset parser is a change to make when Pay actually sends one.
///
/// No chrono/time dependency for one call site, same trade `settings_dlg::format_unix_date`
/// already made in the other direction; the civil-days arithmetic below is Howard Hinnant's
/// `days_from_civil`, valid for any Gregorian date this app will ever see.
pub(crate) fn parse_iso_unix(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_at(s.char_indices().nth(10).map_or(s.len(), |(i, _)| i));
    let (year, month, day) = parse_iso_date(date)?;
    let (hour, minute, second) = if rest.is_empty() {
        (0, 0, 0)
    } else {
        parse_iso_time(rest)?
    };
    let days = days_from_civil(year, month, day);
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// `YYYY-MM-DD` with a plausible month and day (the civil-days arithmetic tolerates a 31st of
/// a short month; the relay never sends one, and refusing it here would only move the guess).
pub(super) fn parse_iso_date(date: &str) -> Option<(i64, i64, i64)> {
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// The part after the date: a `T`/`t`/space separator, `HH:MM[:SS]`, optional fraction,
/// optional `Z`. A numeric offset is refused (see [`parse_iso_unix`]).
pub(super) fn parse_iso_time(rest: &str) -> Option<(i64, i64, i64)> {
    let time = rest
        .strip_prefix('T')
        .or_else(|| rest.strip_prefix('t'))
        .or_else(|| rest.strip_prefix(' '))?;
    let time = time.trim_end_matches(['Z', 'z']);
    // Before the fraction is cut: in `12:34:56.789+02:00` the offset follows the fraction.
    if time.contains('+') || time.contains('-') {
        return None;
    }
    let time = time.split('.').next().unwrap_or(time);
    let mut hms = time.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next().map_or(Ok(0), str::parse).ok()?;
    if hms.next().is_some() || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some((h, mi, se))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic Gregorian date.
/// The year is shifted to start in March so the leap day lands last.
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Is `s` a reason token the way the relay defines one: 1-40 chars of `[a-z0-9_]`? Anything
/// else is dropped rather than stored, so a surprising relay can never put arbitrary text
/// into the breadcrumb or onto the screen.
pub(super) fn is_reason_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Map an HTTP status + raw body from `GET /license/check` to a [`CheckResult`].
/// `None` covers every failure shape (non-200, unparsable, missing `entitled`) - the
/// relay contract only documents 200 as a real answer; a 5xx is failure, not "no".
pub(super) fn parse_check_response(status: u16, body: &[u8]) -> Option<CheckResult> {
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let entitled = v.get("entitled").and_then(Value::as_bool)?;
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| is_reason_token(r))
        .map(String::from);
    let maintenance_active = v.get("maintenanceActive").and_then(Value::as_bool);
    let maint_unix = v
        .get("maintenanceEndsAt")
        .and_then(Value::as_str)
        .and_then(parse_iso_unix);
    Some(CheckResult {
        entitled,
        status,
        reason,
        maintenance_active,
        maint_unix,
    })
}

/// The periodic entitlement re-check. BLOCKING - run on a worker thread. Makes NO
/// network call at all (returns `None` immediately) unless this machine has some
/// reason to care about a business seat - either it is currently in Business mode,
/// or the breadcrumb remembers it once was - and even then, at most once every
/// [`REFRESH_THROTTLE_SECS`]. A Personal install that never touched a business key
/// never talks to the relay, which is the point: free is free, silently.
pub(crate) fn refresh_entitlement() -> Option<Entitlement> {
    refresh_entitlement_inner(false)
}

/// The explicit "Check now" click: the same call with the throttle skipped. A person
/// asking is not a timer, and the answer they get must be fresh.
pub(crate) fn refresh_entitlement_now() -> Option<Entitlement> {
    refresh_entitlement_inner(true)
}

pub(super) fn refresh_entitlement_inner(force: bool) -> Option<Entitlement> {
    start_trial_if_due();
    let mode = read_mode();
    let path = history_path()?;
    let history = read_history(&path);
    // A Personal copy is asked about only if a key was ever redeemed on it (so a licence
    // moved back to it is noticed); an evaluation that ran and was reinstalled as Personal
    // has nothing the relay could say about it.
    let has_redeemed = history
        .as_ref()
        .is_some_and(|h| h.last_positive_unix > 0 || !h.key_prefix.is_empty());
    if mode != Mode::Business && !has_redeemed {
        return None;
    }
    let now = now_unix();
    let last_check = history.as_ref().map_or(0, |h| h.last_check_unix);
    if !force && !refresh_due(now, last_check) {
        return None;
    }
    // Record the attempt now, before the network call, so a dead network (which
    // never reaches the code below) still holds the throttle for the next launch
    // rather than retrying every time - "leave the breadcrumb alone except
    // last_check_unix" from the contract, applied to every failure path at once.
    update_history(|h| h.last_check_unix = now);

    let fingerprint = machine_fingerprint()?;
    let url = format!(
        "{RELAY_BASE}/license/check?subject={}",
        crate::http::form_enc(&fingerprint)
    );
    let resp = crate::http::request_with_deadline(
        "GET",
        &url,
        "",
        &[],
        RELAY_TIMEOUT_SECS,
        RELAY_OVERALL_SECS,
        RELAY_MAX_RESP_BYTES,
    )?;
    apply_check_response(&path, now, resp.status, &resp.body)
}

/// The response-handling half of [`refresh_entitlement_inner`] (E05 follow-up audit, review
/// item 4b): given the relay's raw status/body for `GET /license/check`, decide what (if
/// anything) to write to the breadcrumb and return the resulting entitlement. Split out so a
/// test can drive the REAL decision with a hand-written failure response, rather than
/// re-implementing its if-let in the test and only ever proving the test's own copy correct.
pub(super) fn apply_check_response(
    path: &std::path::Path,
    now: u64,
    status: u16,
    body: &[u8],
) -> Option<Entitlement> {
    let result = parse_check_response(status, body)?;
    sagethumbs2k_core::safety::log_debugf!(
        "license: check entitled={} status={} maintenance_active={:?} maint_unix={:?}",
        result.entitled,
        result.status,
        result.maintenance_active,
        result.maint_unix
    );

    // The updates window is recorded on EVERY understood answer, entitled or not, and
    // separately from the entitlement branches below - it is a different fact with a
    // different lifetime (a perpetual licence outlives its update window by design), so a
    // seat that has gone quiet must not also lose the date the Licence page shows. A
    // response that carries no window leaves whatever is on record alone rather than
    // clearing it: "the relay didn't say" is not "the window is gone".
    if let Some(maint) = result.maint_unix {
        update_history_at(path, |h| h.maint_unix = maint);
    }

    if result.entitled {
        update_history_at(path, |h| {
            h.last_positive_unix = now;
            h.last_status = "active".to_string();
            h.last_reason.clear();
            h.was_business = true;
            h.revoked_unix = 0;
        });
    } else if result.status == "revoked" {
        update_history_at(path, |h| {
            h.last_status = "revoked".to_string();
            h.last_reason = result.reason.clone().unwrap_or_default();
            // The lock clock for a revoked seat runs from the FIRST time this machine
            // learned of it (see `licence_state::phase`), so a repeat answer must not
            // push the date out.
            if h.revoked_unix == 0 {
                h.revoked_unix = now;
            }
        });
    }
    // else: a definite, understood "not entitled, not revoked either" (e.g. a
    // machine that never redeemed anything) - the throttle bump already happened
    // and there's nothing else to change.

    let refreshed = read_history(path)?;
    Some(entitlement_from_cache(now, refreshed.last_positive_unix))
}

/// Permanently retire the one-time downgrade notice for this machine.
pub(crate) fn acknowledge_downgrade() {
    update_history(|h| h.downgrade_acknowledged = true);
}
