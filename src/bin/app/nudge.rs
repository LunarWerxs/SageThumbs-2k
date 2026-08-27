//! App glue for the sign-in nudge: persistence, identity, and the one place that decides.
//!
//! [`crate::nudge_engine`] is a **verbatim** vendored copy of LunarWerx's shared engine
//! (`packages/connections-connect/ports/nudge.rs`), the same file QuickDictate carries. Keeping it
//! byte-identical is the point: a drift in how often SageThumbs asks, versus every other app that
//! ships this prompt, then shows up as a plain `diff` instead of as two products that quietly
//! disagree. The engine is never edited here — everything SageThumbs-specific lives in this file.
//!
//! What this file owns:
//!
//!   * **Persistence.** One JSON string in the same place every other SageThumbs preference lives
//!     ([`sagethumbs2k_core::settings`] — HKCU on an installed copy, the portable store otherwise),
//!     so a portable copy carries the prompt's memory with it and an uninstall takes it away.
//!   * **Signed-in truth.** The engine asks the host; the host asks [`crate::sync_client`].
//!   * **Hand-rolled JSON.** No serde derive, because this crate has no serde derive — the app
//!     builds its request bodies out of `serde_json::Value` too (see `sync_client.rs`), and adding
//!     a proc-macro dependency to a shell extension for ten fields is not a trade worth making.
//!
//! Every read degrades to "start over, ask later" and every write is best-effort. Nothing in this
//! file may fail upward: it decides whether to draw a banner, and a registry that will not answer
//! is not a reason for Explorer's thumbnail host to be unhappy.

use serde_json::{json, Value};

use crate::nudge_engine::{
    Ask, Cadence, Campaign, Config, NudgeState, Outcome, PendingAsk, StopReason,
};

/// The slug the landing page keys off. Must match the `sagethumbs` entry in Connections'
/// `nudge-apps.ts` registry — a mismatch is not an error anywhere, it silently downgrades the page
/// the user lands on to its generic form, which is exactly the kind of failure nobody notices.
const APP_ID: &str = "sagethumbs";
const APP_NAME: &str = "SageThumbs 2K";

/// Registry value name, beside every other preference.
const STATE_VALUE: &str = "SignInNudge";

// ===== on-disk shape =====

fn cadence_name(c: Cadence) -> &'static str {
    match c {
        Cadence::Default => "default",
        Cadence::Monthly => "monthly",
        Cadence::Never => "never",
    }
}

fn cadence_from(name: &str) -> Cadence {
    match name {
        "monthly" => Cadence::Monthly,
        "never" => Cadence::Never,
        // Anything unrecognized — a hand-edit, a value from a future build — falls back to the
        // ladder, the quietest of the three that still asks.
        _ => Cadence::Default,
    }
}

fn campaign_name(c: Campaign) -> &'static str {
    match c {
        Campaign::SignIn => "sign-in",
        Campaign::Discover => "discover",
    }
}

fn campaign_from(name: &str) -> Option<Campaign> {
    match name {
        "sign-in" => Some(Campaign::SignIn),
        "discover" => Some(Campaign::Discover),
        _ => None,
    }
}

fn stop_name(r: StopReason) -> &'static str {
    match r {
        StopReason::UserOptedOut => "user-opted-out",
        StopReason::LadderExhausted => "ladder-exhausted",
        StopReason::Declined => "declined",
    }
}

fn stop_from(name: &str) -> StopReason {
    match name {
        "user-opted-out" => StopReason::UserOptedOut,
        "ladder-exhausted" => StopReason::LadderExhausted,
        // An unreadable stop reason is still a stop. Guessing "not stopped" would resurrect an
        // engine the user had already silenced — the worst thing this file could do.
        _ => StopReason::Declined,
    }
}

fn to_json(s: &NudgeState) -> Value {
    json!({
        "v": s.version,
        "installed_at": s.installed_at,
        "session_count": s.session_count,
        "last_ask_at": s.last_ask_at,
        "ask_count": s.ask_count,
        "consecutive_declines": s.consecutive_declines,
        "cadence": cadence_name(s.cadence),
        "stopped": s.stopped.map(stop_name),
        "pending_ask": s.pending_ask.as_ref().map(|p| json!({
            "at": p.at,
            "trigger": p.trigger,
            "campaign": campaign_name(p.campaign),
        })),
        "converted": s.converted.iter().map(|c| campaign_name(*c)).collect::<Vec<_>>(),
    })
}

/// Parse a stored blob, or `None` if it is not something this app wrote.
///
/// Two plausibility checks that are not paranoia. `v` and `installed_at` are both zero in anything
/// that merely *shaped* like our value — an empty object, a truncated write, a hand-typed `{}` —
/// and `installed_at: 0` claims an install in 1970, which satisfies the engine's one-week age gate
/// on the spot. A corrupted value would then open a prompt that should have waited a week.
fn from_json(v: &Value) -> Option<NudgeState> {
    let version = v.get("v")?.as_u64()? as u32;
    let installed_at = v.get("installed_at")?.as_u64()?;
    if version == 0 || installed_at == 0 {
        return None;
    }
    let num = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(NudgeState {
        version,
        installed_at,
        session_count: num("session_count") as u32,
        last_ask_at: v.get("last_ask_at").and_then(Value::as_u64),
        ask_count: num("ask_count") as u32,
        consecutive_declines: num("consecutive_declines") as u32,
        cadence: cadence_from(v.get("cadence").and_then(Value::as_str).unwrap_or("")),
        stopped: v.get("stopped").and_then(Value::as_str).map(stop_from),
        pending_ask: v.get("pending_ask").and_then(|p| {
            Some(PendingAsk {
                at: p.get("at")?.as_u64()?,
                trigger: p.get("trigger")?.as_str()?.to_string(),
                campaign: campaign_from(p.get("campaign")?.as_str()?)?,
            })
        }),
        converted: v
            .get("converted")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| campaign_from(c.as_str()?))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

// ===== the live state =====

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn config() -> Config {
    let mut cfg = Config::new(APP_ID, APP_NAME);
    cfg.app_version = Some(env!("CARGO_PKG_VERSION").to_string());
    cfg
}

fn load() -> NudgeState {
    let now = now_ms();
    let parsed = sagethumbs2k_core::settings::get_string_opt(STATE_VALUE)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| from_json(&v));
    match parsed {
        Some(mut state) => {
            // `sanitize` is the engine's own repair pass — a moved clock, a rolled-back release, a
            // hand-edit. Every branch of it degrades toward asking LESS, so running it on load is
            // strictly safer than trusting what was stored.
            state.sanitize(now);
            state
        }
        None => NudgeState::new(now),
    }
}

fn persist(state: &NudgeState) {
    let _ = sagethumbs2k_core::settings::set_string(STATE_VALUE, &to_json(state).to_string());
}

fn with_state<T>(f: impl FnOnce(&mut NudgeState) -> T) -> T {
    // Read-modify-write against the registry each time rather than caching in a static.
    //
    // This app is several processes — the settings EXE, the CLI, the shell extension host — and a
    // cached copy in one of them would be stale the moment another wrote. There are at most a
    // handful of these calls per run, so the read costs nothing worth optimizing away.
    let mut state = load();
    let out = f(&mut state);
    persist(&state);
    out
}

// ===== the app-facing surface =====

/// Count this launch of the settings app. Call once, before the dialog is built.
///
/// This is also what settles an ask the last run left on screen when the user closed the window:
/// the engine counts an unanswered ask as a decline, so forgetting to report an outcome makes
/// SageThumbs ask *less*.
pub(crate) fn start_session() {
    let cfg = config();
    with_state(|s| s.start_session(&cfg, now_ms()));
}

/// Decide whether to ask right now. Returns `None` far more often than not.
pub(crate) fn consider(trigger: &str) -> Option<Ask> {
    let cfg = config();
    let signed_in = crate::sync_client::is_signed_in();
    with_state(|s| s.consider(&cfg, trigger, signed_in, now_ms()))
}

/// Report what the user did with the ask that is on screen.
pub(crate) fn record(outcome: Outcome) {
    let cfg = config();
    with_state(|s| s.record(&cfg, outcome));
}

/// They signed in some other way (the Data & Backup page, or credentials already on the machine).
/// Retires the sign-in campaign so it is never asked again.
pub(crate) fn mark_signed_in() {
    with_state(|s| s.mark_signed_in());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every enum the stored shape carries must survive a round trip. A silent mismatch would
    /// reset the ladder on every launch, and nothing in the UI would show it.
    #[test]
    fn enum_names_round_trip() {
        for c in [Cadence::Default, Cadence::Monthly, Cadence::Never] {
            assert_eq!(cadence_from(cadence_name(c)), c);
        }
        for c in [Campaign::SignIn, Campaign::Discover] {
            assert_eq!(campaign_from(campaign_name(c)), Some(c));
        }
        for r in [
            StopReason::UserOptedOut,
            StopReason::LadderExhausted,
            StopReason::Declined,
        ] {
            assert_eq!(stop_from(stop_name(r)), r);
        }
    }

    /// A full state must survive serialization unchanged. Written as one round trip rather than
    /// field-by-field assertions so that ADDING a field to the engine and forgetting it here fails
    /// this test instead of silently dropping it on every save.
    #[test]
    fn state_round_trips_through_json() {
        let mut state = NudgeState::new(1_000);
        state.session_count = 7;
        state.ask_count = 2;
        state.last_ask_at = Some(900);
        state.consecutive_declines = 1;
        state.cadence = Cadence::Monthly;
        state.stopped = Some(StopReason::LadderExhausted);
        state.pending_ask = Some(PendingAsk {
            at: 950,
            trigger: "settings-changed".into(),
            campaign: Campaign::SignIn,
        });
        state.converted.insert(Campaign::Discover);

        let back = from_json(&to_json(&state)).expect("round trip");
        assert_eq!(back, state);
    }

    /// Anything that is not a value this app wrote must read as absent, never as a usable state.
    /// The `installed_at: 0` case is the dangerous one: it parses fine and claims a 1970 install,
    /// which would satisfy the week-old gate immediately.
    #[test]
    fn implausible_values_read_as_absent() {
        for raw in [
            "{}",
            "[]",
            "null",
            "{\"v\":1}",
            "{\"v\":1,\"installed_at\":0}",
            "{\"v\":0,\"installed_at\":5000}",
        ] {
            let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            assert!(from_json(&value).is_none(), "input {raw:?}");
        }
    }

    /// The slug is the join with the landing page. Asserting it here means renaming the app cannot
    /// silently start sending users to the generic page.
    #[test]
    fn app_id_matches_the_landing_page_slug() {
        assert_eq!(APP_ID, "sagethumbs");
        assert!(config().link_base.contains("connections.icu"));
    }
}
