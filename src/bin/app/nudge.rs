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
    }
}

fn cadence_from(name: &str) -> Cadence {
    match name {
        "monthly" => Cadence::Monthly,
        // `"never"` was a real cadence until 2026-08-27 and the engine has no such state now.
        // It never shipped, so the only copies are on our own dev machines - but somebody there
        // did click it, and reading their explicit "leave me alone" as the DAILY default would be
        // the rudest possible interpretation. Monthly is the quietest cadence that still exists,
        // so that is what an old opt-out becomes.
        "never" => Cadence::Monthly,
        // Anything else — a hand-edit, a value from a future build — falls back to the default.
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
        StopReason::LadderExhausted => "ladder-exhausted",
    }
}

/// Whether a stored `stopped` word came from a build where stopping was permanent.
///
/// Nothing has to be recognised here beyond "there was one". The engine used to record three
/// reasons; with this app's config it can now reach none of them (`repeat_ms` is `Some`, so the
/// ladder never runs out), so ANY stored value is legacy by definition.
///
/// The state it describes is a promise we no longer make, and there are only two honest ways to
/// treat it: keep them silenced forever, which the owner has explicitly ruled out, or resurrect
/// them into daily prompts, which is the rudest possible reading of somebody who pressed a button
/// that said "don't ask again". [`from_json`] does neither - it converts the stop into the MONTHLY
/// cadence, which is the quietest thing the engine can still do and about as close to the original
/// bargain as a design without "forever" allows.
fn was_stopped(v: &Value) -> bool {
    v.get("stopped").and_then(Value::as_str).is_some()
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
    let stored_cadence = cadence_from(v.get("cadence").and_then(Value::as_str).unwrap_or(""));
    // A legacy permanent stop becomes a monthly cadence rather than either extreme. See
    // [`was_stopped`] for why that is the only defensible reading.
    let cadence = if was_stopped(v) {
        Cadence::Monthly
    } else {
        stored_cadence
    };
    Some(NudgeState {
        version,
        installed_at,
        session_count: num("session_count") as u32,
        last_ask_at: v.get("last_ask_at").and_then(Value::as_u64),
        ask_count: num("ask_count") as u32,
        consecutive_declines: num("consecutive_declines") as u32,
        cadence,
        stopped: None,
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

/// A NAMED mutex so every process (the settings EXE, `st2k`, the shell extension host) shares
/// the one kernel object around [`with_state`]'s read-modify-write (issue #94) — without it, two
/// processes racing a load-edit-write can silently drop one edit (an ask recorded in one process
/// disappearing because another process's stale read overwrote it). `Local\` scopes it to this
/// logon session, mirroring `settings.rs`'s portable-ini `IniLock`.
struct NudgeLock(windows::Win32::Foundation::HANDLE);

impl NudgeLock {
    /// Best-effort: a lock that could not be created, or a wait that timed out, returns `None`
    /// and the caller proceeds unlocked rather than blocking a shell/host thread forever — a
    /// leaked/wedged mutex must never hang a settings write.
    fn acquire() -> Option<Self> {
        use windows::core::w;
        use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let h = unsafe { CreateMutexW(None, false, w!("Local\\SageThumbs2K.NudgeState")) }.ok()?;
        match unsafe { WaitForSingleObject(h, 2_000) } {
            // WAIT_ABANDONED means a previous holder died mid-edit without releasing; we still
            // got ownership, and `persist` only ever replaces the whole value in one write.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(NudgeLock(h)),
            _ => {
                let _ = unsafe { CloseHandle(h) };
                None
            }
        }
    }
}

impl Drop for NudgeLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn with_state<T>(f: impl FnOnce(&mut NudgeState) -> T) -> T {
    // Read-modify-write against the registry each time rather than caching in a static.
    //
    // This app is several processes — the settings EXE, the CLI, the shell extension host — and a
    // cached copy in one of them would be stale the moment another wrote. There are at most a
    // handful of these calls per run, so the read costs nothing worth optimizing away. The named
    // mutex (issue #94) is what keeps two of those processes from racing the load-edit-write
    // itself and dropping one edit.
    let _lock = NudgeLock::acquire();
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
    with_state(|s| s.consider(&cfg, trigger, signed_in, now_ms())).map(localize)
}

/// Swap the engine's English copy for the active locale's.
///
/// The engine's `copy_for` is English string literals by construction, and it is vendored
/// VERBATIM — so the only place this can happen without forking it is here, on the way out.
/// Both halves then get to be true at once: `nudge_engine.rs` stays byte-identical with the
/// copy QuickDictate carries, and a Turkish user gets a Turkish card instead of an English
/// advert sitting in an otherwise fully translated window.
///
/// ONLY the three copy fields are touched. `url` carries attribution, and `campaign` /
/// `trigger` / `ordinal` are what [`record`] settles the state machine with; rewriting any of
/// them here would make the engine and the app disagree about what was asked.
fn localize(mut ask: Ask) -> Ask {
    // Mirrors the engine's own key selection: a discover ask uses discover copy whatever
    // trigger fired it, because telling someone already signed in to sign in is the one
    // mistake the campaign split exists to prevent.
    let specific = ask.campaign != Campaign::Discover && ask.trigger == TRIGGER_SETTINGS;
    let (head, body) = if specific {
        ("nudge_head", "nudge_body")
    } else {
        // Everything else gets the GENERIC pair, translated. The engine has six copy variants
        // and SageThumbs can reach exactly one of them — `consider` is called once, from the
        // settings window, and `Config::discover` is off by default — so translating the other
        // five into 36 languages would be 360 strings no user can ever see. The cost of that
        // choice is bounded on purpose: a trigger added later renders translated generic copy,
        // never English, which is the failure this whole change exists to remove.
        ("nudge_head_generic", "nudge_body_generic")
    };
    ask.headline = crate::win::t(head).replace("{app}", APP_NAME);
    ask.body = crate::win::t(body).replace("{app}", APP_NAME);
    ask.action_label = crate::win::t("nudge_action").to_string();
    ask
}

/// The one trigger SageThumbs fires. Named so [`localize`]'s key choice and the call site in
/// `settings_dlg::nudge::decide` cannot drift apart into a silent fall back to generic copy.
pub(crate) const TRIGGER_SETTINGS: &str = "settings-changed";

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

    /// An ask carrying obviously-English engine copy, so a test can tell "the locale answered"
    /// apart from "the engine's string came through untouched".
    fn engine_ask(trigger: &str, campaign: Campaign) -> Ask {
        Ask {
            campaign,
            trigger: trigger.to_string(),
            ordinal: 1,
            headline: "ENGINE HEADLINE".into(),
            body: "ENGINE BODY".into(),
            action_label: "ENGINE ACTION".into(),
            can_snooze_month: false,
            url: "https://example.invalid/attribution".into(),
        }
    }

    /// The three copy fields come from the locale; everything else is the engine's and must
    /// arrive at [`record`] exactly as it left `consider`. Rewriting `trigger` or `campaign`
    /// here would make the state machine settle a different ask than the one on screen, and
    /// `url` is what ties a signup back to the app and the moment.
    #[test]
    fn localize_replaces_the_copy_and_nothing_else() {
        let before = engine_ask(TRIGGER_SETTINGS, Campaign::SignIn);
        let after = localize(before.clone());

        assert_ne!(after.headline, before.headline, "headline stayed English");
        assert_ne!(after.body, before.body, "body stayed English");
        assert_ne!(
            after.action_label, before.action_label,
            "action label stayed English"
        );

        assert_eq!(after.url, before.url);
        assert_eq!(after.trigger, before.trigger);
        assert_eq!(after.campaign, before.campaign);
        assert_eq!(after.ordinal, before.ordinal);
    }

    /// The whole point of the constant: if the trigger string and [`localize`]'s key choice
    /// ever drift apart, the banner silently drops to the generic copy and still looks fine.
    /// Compares against a trigger that is MEANT to be generic, so the test states the
    /// difference rather than pinning a translated string it would have to be updated with.
    #[test]
    fn the_settings_trigger_selects_its_own_copy() {
        let specific = localize(engine_ask(TRIGGER_SETTINGS, Campaign::SignIn));
        let generic = localize(engine_ask(
            "a-trigger-this-app-does-not-fire",
            Campaign::SignIn,
        ));
        assert_ne!(
            specific.headline, generic.headline,
            "the settings trigger fell through to the generic copy"
        );
    }

    /// Discover keys off the CAMPAIGN, mirroring the engine: someone already signed in must
    /// never be told to sign in, whichever trigger happened to fire.
    #[test]
    fn discover_ignores_the_trigger() {
        let discover = localize(engine_ask(TRIGGER_SETTINGS, Campaign::Discover));
        let generic = localize(engine_ask("whatever", Campaign::SignIn));
        assert_eq!(discover.headline, generic.headline);
        assert_eq!(discover.body, generic.body);
    }

    /// Every `{app}` slot must be filled. An unsubstituted one puts literal braces on screen,
    /// which is worse than the English string this change replaced. Runs across all three key
    /// paths and whatever language the test machine happens to be in.
    #[test]
    fn no_placeholder_reaches_the_screen() {
        for (trigger, campaign) in [
            (TRIGGER_SETTINGS, Campaign::SignIn),
            ("power-user", Campaign::SignIn),
            ("anything", Campaign::Discover),
        ] {
            let ask = localize(engine_ask(trigger, campaign));
            for (what, s) in [
                ("headline", &ask.headline),
                ("body", &ask.body),
                ("action_label", &ask.action_label),
            ] {
                assert!(
                    !s.contains('{') && !s.contains('}'),
                    "{trigger}: {what} kept a placeholder: {s}"
                );
                assert!(!s.is_empty(), "{trigger}: {what} is empty");
            }
        }
    }

    /// Every enum the stored shape carries must survive a round trip. A silent mismatch would
    /// reset the ladder on every launch, and nothing in the UI would show it.
    #[test]
    fn enum_names_round_trip() {
        for c in [Cadence::Default, Cadence::Monthly] {
            assert_eq!(cadence_from(cadence_name(c)), c);
        }
        for c in [Campaign::SignIn, Campaign::Discover] {
            assert_eq!(campaign_from(campaign_name(c)), Some(c));
        }
        // StopReason has one variant and this app can no longer reach it, so there is nothing
        // left to round-trip - `stop_name` is still exercised by the state round trip below.
    }

    /// A state stored by a build that still had a permanent opt-out must come back as the
    /// MONTHLY cadence: quiet, but not silenced forever. Both halves matter and neither is
    /// obvious, which is why this is asserted rather than left to the reader of `was_stopped`.
    #[test]
    fn a_legacy_permanent_stop_becomes_monthly() {
        for (cadence, stopped) in [
            ("never", "user-opted-out"),
            ("default", "declined"),
            ("default", "ladder-exhausted"),
        ] {
            let raw = json!({
                "v": 1,
                "installed_at": 1_000_u64,
                "session_count": 9,
                "cadence": cadence,
                "stopped": stopped,
            });
            let state = from_json(&raw).expect("parses");
            assert_eq!(
                state.cadence,
                Cadence::Monthly,
                "cadence={cadence} stopped={stopped}"
            );
            assert_eq!(
                state.stopped, None,
                "a stop that can no longer be earned must not be carried forward"
            );
        }
    }

    /// A plain `"never"` cadence with no stop recorded is the same promise by a different route,
    /// and gets the same answer.
    #[test]
    fn a_legacy_never_cadence_becomes_monthly() {
        let raw = json!({ "v": 1, "installed_at": 1_000_u64, "cadence": "never" });
        assert_eq!(from_json(&raw).expect("parses").cadence, Cadence::Monthly);
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
        // `stopped` is deliberately NOT round-tripped - see `was_stopped`. It is left `None` here
        // so this stays a test of the fields that DO survive; the conversion has its own test.
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

    /// Issue #94: the whole point of a NAMED (not process-local) mutex is that a second
    /// holder — standing in for a second PROCESS, since a named mutex is shared by name
    /// regardless of which process asks for it — must block until the first releases it,
    /// instead of both racing straight through to a read-modify-write.
    #[test]
    fn nudge_lock_serializes_concurrent_acquires() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let Some(first) = NudgeLock::acquire() else {
            eprintln!("nudge_lock_serializes_concurrent_acquires: mutex unavailable, skipping");
            return;
        };
        let released = Arc::new(AtomicBool::new(false));
        let released2 = released.clone();
        let handle = std::thread::spawn(move || {
            // Blocks until `first` is dropped below (WaitForSingleObject wakes on release).
            let _second = NudgeLock::acquire();
            assert!(
                released2.load(Ordering::SeqCst),
                "the second acquire returned before the first lock was released"
            );
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        released.store(true, Ordering::SeqCst);
        drop(first);
        handle.join().unwrap();
    }
}
