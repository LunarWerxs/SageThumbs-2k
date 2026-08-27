//! The sign-in nudge decision engine, ported to Rust.
//!
//! A line-for-line port of `src/nudge.ts`. The TypeScript file is the specification and
//! `src/nudge.spec.ts` is its executable form; the tests at the bottom of this file are the same
//! assertions, so a behavioural drift between the two shows up as a failing test on one side.
//!
//! WHY A PORT AND NOT A SERVICE: the decision is a pure state machine over one small blob, and the
//! apps that need it here (SageThumbs 2K, QuickDictate) are native, offline-capable, and have no
//! business asking a server whether to draw a banner. Every rule below is local, deterministic and
//! cheap.
//!
//! ## Vendoring this file
//!
//! Drop it in as a module. It is `#![no_std]`-friendly in spirit - pure `std` collections, no
//! external crates - so it compiles anywhere without touching your dependency tree.
//!
//! Two things the host app owns:
//!
//!   1. **Persistence.** `NudgeState` is plain public fields. Serialize it however you already
//!      serialize settings; if you use serde, add `#[derive(Serialize, Deserialize)]` to the
//!      state, cadence and stop-reason types when you vendor. It is deliberately not derived here
//!      so this file pulls in nothing.
//!   2. **Drawing.** `consider()` returns an `Ask` - headline, body, button label, URL. Render it
//!      however your app renders things, and call `record()` with what the user did. Reporting
//!      nothing is handled: the next `start_session()` counts an unanswered ask as a decline, so
//!      forgetting a callback makes the app ask LESS, never more.
//!
//! ## The rules, in one place
//!
//!   - Nothing is asked before the app has been installed a week AND used a few times.
//!   - Asks fire on a MOMENT (`consider()` at a value event), never on a timer. There is no clock
//!     in here that goes off by itself.
//!   - Once the gate opens it asks at most once a DAY, and it keeps asking. There is no lifetime
//!     cap and no permanent opt-out: a dismissal buys a day, and it comes back tomorrow.
//!   - From the FOURTH ask on, a dismissal is worth a MONTH instead of a day, and every ask after
//!     that is worth a month too. `Ask::can_snooze_month` tells the UI when to offer it, so every
//!     app draws that button at the same moment without re-deriving the rule.
//!   - Ignoring an ask counts as a dismissal. It never stops the engine; nothing does.
//!   - Neither campaign ever asks for money.
//!
//! ## The trade this file used to make, and no longer does
//!
//! Until 2026-08-27 the rules above read the other way round: three asks in a lifetime 30 and 90
//! days apart, a permanent stop after two ignored in a row, and a "Never" button offered on the
//! FIRST ask. That design optimised for never being resented; this one optimises for being seen,
//! and it is a deliberate owner decision (Michael, 2026-08-27: *"I don't want that option. I want
//! them to have to dismiss it. At least once a day. After the third time. We can allow them to
//! dismiss... For a month. Not forever."*).
//!
//! **Do not "restore" the old behaviour as a cleanup.** The cost is real and is accepted knowingly:
//! a promotional prompt with no permanent off is, by the plain meaning of the word, nagware, and
//! this file's own comments used to say so. Two guardrails survive that decision and are the reason
//! it is defensible rather than merely aggressive: the WEEK-AND-SEVERAL-SESSIONS gate is untouched,
//! so nobody meets it early; and the month escape is unlimited, so a user who never wants it can
//! spend one click a month forever and never see it otherwise. If either of those is ever weakened,
//! this stops being a nudge.
//!
//! Nothing had shipped when this changed - QuickDictate's released tags carry no nudge files, the
//! web banner was not wired to a live surface, and SageThumbs 2.5.0 was not cut - so no user was
//! ever shown a "Never" button and no promise was withdrawn. That is why `Cadence::Never` and the
//! decline-stop are DELETED rather than merely hidden: there is no persisted state in the wild that
//! needs them, and an engine that cannot express "forever" cannot regress into it.

use std::collections::BTreeSet;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// How often this user has agreed to hear about it. Only ever changed by the user choosing.
///
/// There are exactly two, and there is deliberately no third. A `Never` variant existed until
/// 2026-08-27; it is gone rather than hidden, so that no future UI can reintroduce a permanent
/// opt-out by simply drawing a button for a state the engine still understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Daily, indefinitely.
    Default,
    /// A dismissal taken from the fourth ask on. Buys a month, and every ask after it is monthly
    /// too - so a user who never wants this spends one click a month and is otherwise left alone.
    Monthly,
}

/// Which pitch is being made. Deliberately never combined in one message: they are aimed at people
/// in different states, and bundled each one dilutes the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Campaign {
    /// For someone who has an account waiting and does not know it.
    SignIn,
    /// For someone already signed in, about what else the account reaches.
    Discover,
}

/// Why the engine will never ask again. Recorded so support can answer "why did it stop?".
///
/// Conversion is deliberately absent: signing in finishes the sign-in CAMPAIGN, not the engine.
///
/// **With the shipped defaults this is unreachable, and that is the point** - `Config::repeat_ms`
/// is `Some`, so the ladder never runs out. It survives for an app that deliberately opts into a
/// finite ladder by setting `repeat_ms: None`. `UserOptedOut` and `Declined` are gone: a user
/// choice and an ignored prompt must not be able to end the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    LadderExhausted,
}

/// What the user did with an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Took the action. Retires that campaign.
    Accepted,
    /// "Later" - engagement rather than refusal. It buys the same interval a decline does; the
    /// difference is only in the counter support reads.
    Snoozed,
    /// Closed, dismissed, or ignored. Two in a row stops the engine.
    Declined,
    /// Picked a cadence. In practice that is the month-long dismissal, offered from the fourth
    /// ask on; nothing here can stop the engine.
    SetCadence(Cadence),
}

/// An ask, handed to the app to render however it likes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub campaign: Campaign,
    pub trigger: String,
    /// Which ask this is, 1-based.
    pub ordinal: u32,
    pub headline: String,
    pub body: String,
    pub action_label: String,
    /// Whether the UI should offer the month-long dismissal beside the ordinary one.
    ///
    /// True from `Config::snooze_after` asks onward (the fourth, by default). Decided HERE rather
    /// than in each app so every surface grows the button at the same moment - the alternative is
    /// three UIs each re-deriving "is this the fourth one" and one of them getting it wrong, which
    /// nothing would catch because the wrong answer still renders a valid banner.
    pub can_snooze_month: bool,
    /// Carries attribution, so a signup can be traced to the app and the moment that produced it.
    pub url: String,
}

/// Identity and tunables. Everything here has a default that matches the TypeScript engine.
#[derive(Debug, Clone)]
pub struct Config {
    /// Slug as it appears in the URL. Must match the app's `appId` on the JS side.
    pub app_id: String,
    /// Display name, used in copy.
    pub app_name: String,
    /// Shipping version, passed through to attribution.
    pub app_version: Option<String>,
    /// Days installed before the first ask. Default 7 days.
    pub min_age_ms: u64,
    /// Sessions before the first ask. Default 3.
    pub min_sessions: u32,
    /// Gaps before each of the OPENING asks. Default `[0]` - ask as soon as the gate opens.
    ///
    /// It no longer bounds the lifetime; `repeat_ms` decides what happens once it is spent.
    pub ladder_ms: Vec<u64>,
    /// Gap for every ask after `ladder_ms` is spent. `Some(1 day)` by default, so it asks daily
    /// and forever. `None` restores the old finite ladder, and is the ONLY way to reach
    /// [`StopReason::LadderExhausted`].
    pub repeat_ms: Option<u64>,
    /// The 1-based ask ordinal from which the month-long dismissal is offered. Default 3, so the
    /// first three asks can only be dismissed for a day and the fourth onward for a month.
    ///
    /// Read as "after this many asks", which is why the comparison is `ordinal > snooze_after`.
    pub snooze_after: u32,
    /// Run the discover campaign at signed-in users. Off by default.
    pub discover: bool,
    /// Base for the attribution link. The app slug is appended as a path segment.
    pub link_base: String,
}

impl Config {
    pub fn new(app_id: impl Into<String>, app_name: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_name: app_name.into(),
            app_version: None,
            min_age_ms: 7 * DAY_MS,
            min_sessions: 3,
            ladder_ms: vec![0],
            repeat_ms: Some(DAY_MS),
            snooze_after: 3,
            discover: false,
            link_base: "https://connections.icu/link".to_string(),
        }
    }
}

/// An ask that was shown and whose outcome was never reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub at: u64,
    pub trigger: String,
    pub campaign: Campaign,
}

/// Everything the engine knows. Plain fields - the host app persists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NudgeState {
    pub version: u32,
    /// epoch ms of first run.
    pub installed_at: u64,
    pub session_count: u32,
    pub last_ask_at: Option<u64>,
    pub ask_count: u32,
    pub consecutive_declines: u32,
    pub cadence: Cadence,
    /// Its presence IS the stop; the value is the why.
    pub stopped: Option<StopReason>,
    pub pending_ask: Option<PendingAsk>,
    /// Campaigns already carried to acceptance, so they are never re-run.
    pub converted: BTreeSet<Campaign>,
}

/// Current persisted-shape version. State claiming a newer one is discarded, not guessed at.
pub const STATE_VERSION: u32 = 1;

impl NudgeState {
    pub fn new(now: u64) -> Self {
        Self {
            version: STATE_VERSION,
            installed_at: now,
            session_count: 0,
            last_ask_at: None,
            ask_count: 0,
            consecutive_declines: 0,
            cadence: Cadence::Default,
            stopped: None,
            pending_ask: None,
            converted: BTreeSet::new(),
        }
    }

    /// Repair state that a crash, a hand-edit, a rolled-back release, or a moving clock left
    /// inconsistent. Call it once after loading, before anything else.
    ///
    /// Every branch degrades toward asking LESS. A clock that jumped backwards - a timezone fix, a
    /// VM snapshot restore, a dead CMOS battery - would otherwise leave `now - installed_at`
    /// underflowing and hold the gate shut forever, or make a ladder gap look already satisfied.
    /// Both stamps are pulled back to now, so the user simply waits the interval again.
    pub fn sanitize(&mut self, now: u64) {
        if self.version > STATE_VERSION {
            *self = Self::new(now);
            return;
        }
        if self.installed_at > now {
            self.installed_at = now;
        }
        // `is_some_and` rather than a nested `if let`: clippy's `collapsible_if` rejects the
        // nested form and its suggested fix is a let-chain, which does not compile on edition
        // 2021 - and this file is vendored into apps whose edition is theirs, not ours.
        if self.last_ask_at.is_some_and(|last| last > now) {
            self.last_ask_at = Some(now);
        }
    }

    /// Count a session and settle any ask the last one left unanswered.
    ///
    /// The app quitting on a visible prompt is the most common outcome there is, and it is counted
    /// as a dismissal - the same as pressing the ordinary dismiss button, no better and no worse.
    /// It buys the same day (or month) and it does NOT end anything. The counter it keeps is for
    /// support to read; nothing branches on it.
    ///
    /// `config` is unused now that declines cannot stop the engine. It stays in the signature
    /// because every vendored copy and both spec suites call it this way, and because an app that
    /// sets `repeat_ms: None` still needs the shape to be identical across the port and the
    /// TypeScript original.
    pub fn start_session(&mut self, _config: &Config, now: u64) {
        self.sanitize(now);
        self.session_count = self.session_count.saturating_add(1);
        if self.pending_ask.take().is_some() {
            self.consecutive_declines = self.consecutive_declines.saturating_add(1);
        }
    }

    /// Decide whether to ask right now. Returns `None` far more often than not - that is the point.
    pub fn consider(
        &mut self,
        config: &Config,
        trigger: &str,
        signed_in: bool,
        now: u64,
    ) -> Option<Ask> {
        self.sanitize(now);
        if self.stopped.is_some() || self.pending_ask.is_some() {
            return None;
        }

        let campaign = if signed_in {
            Campaign::Discover
        } else {
            Campaign::SignIn
        };
        if campaign == Campaign::Discover && !config.discover {
            return None;
        }
        if self.converted.contains(&campaign) {
            return None;
        }

        // The gate. Both halves matter: age alone would ask someone who installed the app and
        // never opened it again, sessions alone a burst user on their first afternoon.
        if self.session_count < config.min_sessions
            || now.saturating_sub(self.installed_at) < config.min_age_ms
        {
            return None;
        }

        let gap = match self.next_gap(config) {
            Some(gap) => gap,
            None => {
                // The ladder is spent. Persist it so later sessions skip all of the above.
                self.stop(StopReason::LadderExhausted);
                return None;
            }
        };
        if self
            .last_ask_at
            .is_some_and(|last| now.saturating_sub(last) < gap)
        {
            return None;
        }

        let ordinal = self.ask_count + 1;
        self.ask_count = ordinal;
        self.last_ask_at = Some(now);
        self.pending_ask = Some(PendingAsk {
            at: now,
            trigger: trigger.to_string(),
            campaign,
        });

        let (headline, body, action_label) = copy_for(trigger, campaign, &config.app_name);
        Some(Ask {
            campaign,
            trigger: trigger.to_string(),
            ordinal,
            headline,
            body,
            action_label,
            // `>` not `>=`: `snooze_after` counts asks already spent, so the default 3 means the
            // first three offer a day and the FOURTH is the first to offer a month.
            can_snooze_month: ordinal > config.snooze_after,
            url: build_link(config, trigger, campaign, ordinal),
        })
    }

    /// Report what the user did. A second report for the same ask is ignored, so a UI where
    /// "close" also fires after the action cannot corrupt the ladder.
    ///
    /// `config` is unused now that no outcome can stop the engine; it stays in the signature so
    /// the port and the TypeScript original keep the same shape across every vendored copy.
    pub fn record(&mut self, _config: &Config, outcome: Outcome) {
        // Take the pending ask and KEEP it: acceptance retires the campaign that was actually on
        // screen, and the only record of which one that was is the ask itself. Assuming SignIn
        // here would silently retire the wrong campaign whenever a discover ask was accepted.
        let pending = match self.pending_ask.take() {
            Some(pending) => pending,
            None => return,
        };
        match outcome {
            Outcome::Accepted => {
                self.consecutive_declines = 0;
                self.converted.insert(pending.campaign);
            }
            Outcome::Snoozed => self.consecutive_declines = 0,
            Outcome::Declined => {
                self.consecutive_declines = self.consecutive_declines.saturating_add(1);
            }
            Outcome::SetCadence(cadence) => {
                self.cadence = cadence;
                self.consecutive_declines = 0;
            }
        }
    }

    /// Set the cadence from a settings screen, with no ask on screen.
    ///
    /// It also UN-stops a stopped engine: a user who goes looking for the control has changed
    /// their mind, and continuing to stay silent would be the wrong answer. With the shipped
    /// defaults nothing can be stopped in the first place, so that branch only matters to an app
    /// that chose a finite ladder.
    pub fn set_cadence(&mut self, cadence: Cadence) {
        self.cadence = cadence;
        self.consecutive_declines = 0;
        self.stopped = None;
    }

    /// Record that the user signed in, however they got there - including through the app's own
    /// settings with no ask involved. Ends the sign-in campaign only.
    pub fn mark_signed_in(&mut self) {
        self.converted.insert(Campaign::SignIn);
        self.pending_ask = None;
    }

    /// The gap that must have elapsed since the last ask, or `None` if there is no next rung.
    ///
    /// `None` is only reachable when an app sets `repeat_ms: None`; with the shipped defaults the
    /// ladder is spent after the first ask and every one after it uses `repeat_ms`.
    fn next_gap(&self, config: &Config) -> Option<u64> {
        if self.cadence == Cadence::Monthly {
            return Some(30 * DAY_MS);
        }
        config
            .ladder_ms
            .get(self.ask_count as usize)
            .copied()
            .or(config.repeat_ms)
    }

    /// Idempotent: the FIRST reason is kept, since it is the true one.
    fn stop(&mut self, reason: StopReason) {
        if self.stopped.is_none() {
            self.stopped = Some(reason);
        }
        self.pending_ask = None;
    }
}

/// Percent-encode a query value. Hand-rolled so this file needs no crates; the character set is
/// RFC 3986 unreserved, which is the conservative choice and correct for every slug and version
/// string an app will actually pass.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The attribution link. The slug rides the PATH so the landing page can pitch the right app
/// before any JavaScript runs, and the query as well, because a link that gets shortened, proxied
/// or hand-copied loses its path long before it loses its parameters.
fn build_link(config: &Config, trigger: &str, campaign: Campaign, ordinal: u32) -> String {
    let campaign = match campaign {
        Campaign::SignIn => "sign-in",
        Campaign::Discover => "discover",
    };
    let base = config.link_base.trim_end_matches('/');
    let mut url = format!(
        "{base}/{app}?app={app}&src=nudge&campaign={campaign}&trigger={trigger}&ask={ordinal}",
        app = encode(&config.app_id),
        trigger = encode(trigger),
    );
    if let Some(version) = &config.app_version {
        url.push_str(&format!("&v={}", encode(version)));
    }
    url
}

/// Copy leads with what the user gets, at the moment it is true.
///
/// Deliberately absent: any threat about losing data the app has not actually lost, and any ask
/// for money or sponsorship. The account is free and the app already supports signing into it, so
/// the entire pitch is that it exists.
fn copy_for(trigger: &str, campaign: Campaign, app_name: &str) -> (String, String, String) {
    // The discover campaign keys off the CAMPAIGN, not the trigger it happened to fire on. Keying
    // it off the trigger would tell someone already signed in to sign in.
    let key = if campaign == Campaign::Discover {
        "discover"
    } else {
        trigger
    };
    match key {
        "settings-changed" => (
            "Keep these settings everywhere".into(),
            format!("Sign in with Connections and your {app_name} settings follow you to every machine you use."),
            "Sign in".into(),
        ),
        "config-lost" => (
            "Set that up again?".into(),
            format!("That update reset your {app_name} settings. Signed in with Connections, they come back on their own next time."),
            "Sign in".into(),
        ),
        "power-user" => (
            format!("You use {app_name} a lot"),
            "Sign in with Connections to carry your setup to your other machines - and to every other app we make.".into(),
            "Sign in".into(),
        ),
        "second-device" => (
            "Already set this up?".into(),
            format!("If you use {app_name} on another machine, sign in with Connections and pull that setup across."),
            "Sign in".into(),
        ),
        "discover" => (
            "Your account does more than this".into(),
            format!("The Connections account {app_name} signs into works across every app we make, and a fair bit besides."),
            "See what else".into(),
        ),
        _ => (
            "Sign in with Connections".into(),
            format!("Keep your {app_name} settings across every machine you use."),
            "Sign in".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_760_000_000_000;

    fn config() -> Config {
        Config::new("testapp", "TestApp")
    }

    /// Walks the state to the far side of its gate: three sessions and eight days.
    fn open_gate(state: &mut NudgeState, config: &Config) -> u64 {
        state.start_session(config, START);
        state.start_session(config, START);
        let now = START + 8 * DAY_MS;
        state.start_session(config, now);
        now
    }

    #[test]
    fn stays_silent_on_first_run() {
        let config = config();
        let mut state = NudgeState::new(START);
        state.start_session(&config, START);
        let decision = state.consider(&config, "settings-changed", false, START);
        assert!(decision.is_none());
    }

    #[test]
    fn stays_silent_for_an_app_installed_long_ago_but_barely_used() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = START + 400 * DAY_MS;
        state.start_session(&config, now);
        let decision = state.consider(&config, "settings-changed", false, now);
        assert!(decision.is_none());
    }

    #[test]
    fn stays_silent_for_heavy_use_on_the_first_afternoon() {
        let config = config();
        let mut state = NudgeState::new(START);
        for _ in 0..20 {
            state.start_session(&config, START);
        }
        let decision = state.consider(&config, "power-user", false, START);
        assert!(decision.is_none());
    }

    #[test]
    fn asks_once_both_halves_of_the_gate_are_satisfied() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        let ask = state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        assert_eq!(ask.ordinal, 1);
        assert_eq!(ask.campaign, Campaign::SignIn);
        assert_eq!(ask.headline, "Keep these settings everywhere");
    }

    #[test]
    fn never_asks_from_a_timer_alone() {
        let config = config();
        let mut state = NudgeState::new(START);
        // A year of daily sessions and no consider() call produces no ask: there is no clock in
        // here that goes off by itself. This is the whole difference from a day-counter.
        for day in 0..365 {
            state.start_session(&config, START + day * DAY_MS);
        }
        assert_eq!(state.ask_count, 0);
    }

    #[test]
    fn asks_once_a_day_and_the_ladder_never_runs_out() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("first");
        state.record(&config, Outcome::Snoozed);

        // Same day: dismissing bought the rest of it, however many sessions happen meanwhile.
        now += 23 * 60 * 60 * 1000;
        state.start_session(&config, now);
        assert!(
            state
                .consider(&config, "settings-changed", false, now)
                .is_none(),
            "an hour short of a day"
        );

        // A hundred days, one ask each. The old engine stopped after three; this one does not
        // stop, which is the entire behavioural change and the thing most likely to be
        // "simplified" back by someone reading the old comments.
        now -= 23 * 60 * 60 * 1000;
        for expected in 2..=100u32 {
            now += DAY_MS + 1;
            state.start_session(&config, now);
            let ask = state
                .consider(&config, "settings-changed", false, now)
                .unwrap_or_else(|| panic!("ask {expected} should have fired"));
            assert_eq!(ask.ordinal, expected);
            state.record(&config, Outcome::Snoozed);
        }
        assert_eq!(state.stopped, None);
    }

    #[test]
    fn nothing_stops_it_however_many_asks_are_closed() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);

        // Twenty in a row closed without answering - the old engine died on the second.
        for _ in 0..20 {
            state
                .consider(&config, "settings-changed", false, now)
                .expect("an ask");
            state.record(&config, Outcome::Declined);
            now += DAY_MS + 1;
            state.start_session(&config, now);
        }
        assert_eq!(state.stopped, None);
        assert!(state
            .consider(&config, "settings-changed", false, now)
            .is_some());
    }

    #[test]
    fn the_month_dismissal_appears_only_after_the_third_ask() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        for ordinal in 1..=6u32 {
            if ordinal > 1 {
                now += DAY_MS + 1;
                state.start_session(&config, now);
            }
            let ask = state
                .consider(&config, "settings-changed", false, now)
                .unwrap_or_else(|| panic!("ask {ordinal}"));
            assert_eq!(ask.ordinal, ordinal);
            assert_eq!(
                ask.can_snooze_month,
                ordinal > 3,
                "ask {ordinal} offered the month option: {}",
                ask.can_snooze_month
            );
            state.record(&config, Outcome::Snoozed);
        }
    }

    #[test]
    fn the_month_dismissal_buys_a_month_and_then_asks_again() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        state.record(&config, Outcome::SetCadence(Cadence::Monthly));

        now += 29 * DAY_MS;
        state.start_session(&config, now);
        assert!(
            state
                .consider(&config, "settings-changed", false, now)
                .is_none(),
            "a day short of the month"
        );

        now += 2 * DAY_MS;
        state.start_session(&config, now);
        assert!(
            state
                .consider(&config, "settings-changed", false, now)
                .is_some(),
            "the month is a snooze, not an opt-out"
        );
        assert_eq!(state.stopped, None);
    }

    #[test]
    fn counts_an_unanswered_ask_as_a_decline_at_the_next_session() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("shown");
        // App quits with the prompt on screen. Nothing reported.
        state.start_session(&config, now + DAY_MS);
        assert_eq!(state.consecutive_declines, 1);
        assert!(state.pending_ask.is_none());
    }

    #[test]
    fn later_is_not_a_decline() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("first");
        state.record(&config, Outcome::Snoozed);

        now += 31 * DAY_MS;
        state.start_session(&config, now);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("second");
        state.record(&config, Outcome::Snoozed);

        assert_eq!(state.consecutive_declines, 0);
        assert_eq!(state.stopped, None);
    }

    #[test]
    fn ignores_a_second_outcome_for_the_same_ask() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        state.record(&config, Outcome::Accepted);
        state.record(&config, Outcome::Declined);
        state.record(&config, Outcome::Declined);
        assert_eq!(state.consecutive_declines, 0);
    }

    /// There is no permanent off, and this test exists so that adding one back fails loudly
    /// rather than passing quietly. Two and a half years of monthly dismissals still leave the
    /// engine willing to ask - which is the deal: unlimited escapes, none of them final.
    #[test]
    fn there_is_no_way_to_switch_it_off_for_good() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        for _ in 0..30 {
            state
                .consider(&config, "settings-changed", false, now)
                .expect("an ask");
            state.record(&config, Outcome::SetCadence(Cadence::Monthly));
            now += 31 * DAY_MS;
            state.start_session(&config, now);
        }
        assert_eq!(state.stopped, None);
        assert!(state
            .consider(&config, "settings-changed", false, now)
            .is_some());
    }

    #[test]
    fn monthly_keeps_asking_with_no_lifetime_cap() {
        let config = config();
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        state.record(&config, Outcome::SetCadence(Cadence::Monthly));

        // Well past the three-rung ladder: a user who asked for monthly gets monthly.
        for _ in 0..8 {
            now += 31 * DAY_MS;
            state.start_session(&config, now);
            let decision = state.consider(&config, "settings-changed", false, now);
            assert!(decision.is_some());
            state.record(&config, Outcome::Snoozed);
        }
        assert_eq!(state.stopped, None);
    }

    #[test]
    fn monthly_still_respects_its_gap() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        state.record(&config, Outcome::SetCadence(Cadence::Monthly));

        let soon = now + 20 * DAY_MS;
        state.start_session(&config, soon);
        let decision = state.consider(&config, "settings-changed", false, soon);
        assert!(decision.is_none());
    }

    #[test]
    fn a_settings_screen_reopens_an_exhausted_engine() {
        // Only an app that opted into a finite ladder can be stopped at all, so that is the one
        // this has to be written against now.
        let mut config = config();
        config.ladder_ms = vec![0];
        config.repeat_ms = None;
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("first");
        state.record(&config, Outcome::Snoozed);
        now += 31 * DAY_MS;
        state.start_session(&config, now);
        assert!(state
            .consider(&config, "settings-changed", false, now)
            .is_none());
        assert_eq!(state.stopped, Some(StopReason::LadderExhausted));

        state.set_cadence(Cadence::Monthly);
        assert_eq!(state.stopped, None);
        now += 31 * DAY_MS;
        state.start_session(&config, now);
        let decision = state.consider(&config, "settings-changed", false, now);
        assert!(decision.is_some());
    }

    #[test]
    fn never_runs_the_sign_in_campaign_at_someone_already_signed_in() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        let decision = state.consider(&config, "settings-changed", true, now);
        assert!(decision.is_none());
    }

    #[test]
    fn discover_is_off_unless_the_app_opts_in() {
        let mut config = config();
        config.discover = false;
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        assert!(state.consider(&config, "power-user", true, now).is_none());

        config.discover = true;
        let ask = state
            .consider(&config, "power-user", true, now)
            .expect("an ask");
        assert_eq!(ask.campaign, Campaign::Discover);
        // The trigger was a sign-in moment; the copy must follow the CAMPAIGN.
        assert_eq!(ask.headline, "Your account does more than this");
    }

    #[test]
    fn mark_signed_in_retires_only_the_sign_in_campaign() {
        let mut config = config();
        config.discover = true;
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        state.mark_signed_in();

        let decision = state.consider(&config, "settings-changed", false, now);
        assert!(decision.is_none());
        assert_eq!(
            state
                .consider(&config, "power-user", true, now)
                .map(|ask| ask.campaign),
            Some(Campaign::Discover),
        );
    }

    #[test]
    fn neither_campaign_ever_asks_for_money() {
        // The account is free and these apps already support signing into it, so the entire pitch
        // is that it exists. Copy reaching for a donation would be a different product decision.
        let forbidden = [
            "donate",
            "sponsor",
            "support development",
            "pay",
            "upgrade",
            "subscribe",
            "$",
        ];
        for campaign in [Campaign::SignIn, Campaign::Discover] {
            for trigger in [
                "settings-changed",
                "config-lost",
                "power-user",
                "second-device",
                "anything-else",
            ] {
                let (headline, body, action) = copy_for(trigger, campaign, "TestApp");
                let words = format!("{headline} {body} {action}").to_lowercase();
                for word in forbidden {
                    assert!(
                        !words.contains(word),
                        "{campaign:?}/{trigger} contains {word:?}: {words}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_link_carries_attribution_with_the_slug_in_the_path() {
        let mut config = config();
        config.app_version = Some("2.1.0".into());
        let url = build_link(&config, "settings-changed", Campaign::SignIn, 1);
        assert!(
            url.starts_with("https://connections.icu/link/testapp?"),
            "{url}"
        );
        for part in [
            "app=testapp",
            "src=nudge",
            "campaign=sign-in",
            "trigger=settings-changed",
            "ask=1",
            "v=2.1.0",
        ] {
            assert!(url.contains(part), "{url} is missing {part}");
        }
    }

    #[test]
    fn a_custom_link_base_tolerates_a_trailing_slash() {
        let mut config = config();
        config.link_base = "https://example.test/go/".into();
        let url = build_link(&config, "power-user", Campaign::SignIn, 1);
        assert!(url.starts_with("https://example.test/go/testapp?"), "{url}");
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_jam_the_gate() {
        let config = config();
        let mut state = NudgeState::new(START);
        let now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("an ask");
        state.record(&config, Outcome::Snoozed);

        // A timezone fix, a VM snapshot restore, a dead CMOS battery.
        let rewound = START - 500 * DAY_MS;
        state.start_session(&config, rewound);
        let decision = state.consider(&config, "settings-changed", false, rewound);
        assert!(decision.is_none());
        assert!(state.installed_at <= rewound);
        assert!(state.last_ask_at.unwrap() <= rewound);
    }

    #[test]
    fn state_from_a_future_version_is_discarded_rather_than_guessed_at() {
        let mut state = NudgeState::new(START);
        state.version = 99;
        state.ask_count = 7;
        state.cadence = Cadence::Monthly;
        state.sanitize(START);
        assert_eq!(state.cadence, Cadence::Default);
        assert_eq!(state.ask_count, 0);
    }

    #[test]
    fn an_app_can_choose_its_own_gate_and_cadence() {
        let mut config = config();
        config.min_sessions = 1;
        config.min_age_ms = 0;
        config.repeat_ms = Some(7 * DAY_MS);
        config.snooze_after = 1;
        let mut state = NudgeState::new(START);

        state.start_session(&config, START);
        let first = state
            .consider(&config, "power-user", false, START)
            .expect("first");
        assert_eq!(first.ordinal, 1);
        assert!(!first.can_snooze_month, "snooze_after counts asks SPENT");
        state.record(&config, Outcome::Snoozed);

        let mut now = START + 2 * DAY_MS;
        state.start_session(&config, now);
        assert!(
            state.consider(&config, "power-user", false, now).is_none(),
            "a weekly cadence is not a daily one"
        );

        now += 6 * DAY_MS;
        state.start_session(&config, now);
        let second = state
            .consider(&config, "power-user", false, now)
            .expect("second");
        assert_eq!(second.ordinal, 2);
        assert!(second.can_snooze_month);
    }

    /// The finite ladder is still reachable, for an app that wants one - it is just no longer the
    /// default, and `repeat_ms: None` is the only way to ask for it.
    #[test]
    fn an_app_can_still_choose_a_ladder_that_ends() {
        let mut config = config();
        config.ladder_ms = vec![0, DAY_MS];
        config.repeat_ms = None;
        let mut state = NudgeState::new(START);
        let mut now = open_gate(&mut state, &config);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("first");
        state.record(&config, Outcome::Snoozed);

        now += 2 * DAY_MS;
        state.start_session(&config, now);
        state
            .consider(&config, "settings-changed", false, now)
            .expect("second");
        state.record(&config, Outcome::Snoozed);

        now += 400 * DAY_MS;
        state.start_session(&config, now);
        assert!(state
            .consider(&config, "settings-changed", false, now)
            .is_none());
        assert_eq!(state.stopped, Some(StopReason::LadderExhausted));
    }
}
