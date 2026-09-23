#![cfg(test)]

use super::{clamp_remembered_size, is_load_current, should_size_for_loading, source_capable};
#[cfg(feature = "html-preview")]
use super::{decide_load_route, parse_url_shortcut, LoadRoute};

/// 2026-09-05 audit, F10 adversarial review (P1): `resolve_load`'s worker-side `classify`
/// finds "html" already in `PREVIEW_TEXT_EXTS`, so if `load()` ever again ran the worker
/// (the text/classify route) for these extensions instead of deciding the web route on the
/// UI thread first, every release build (html-preview ships in every release EXE) would
/// preview HTML as raw source and a `.url`/`.webloc` shortcut as its raw INI/plist, never
/// the web card or the live render. This pins the routing gate directly.
#[cfg(feature = "html-preview")]
#[test]
fn html_and_url_extensions_are_gated_to_the_web_route() {
    for ext in ["html", "htm", "xhtml", "url", "webloc"] {
        assert_eq!(
            decide_load_route(ext, false),
            LoadRoute::Web,
            "{ext} must take the web route, not the worker/text route"
        );
    }
    // Unrelated extensions still take the worker's normal classify/read path.
    for ext in ["txt", "md", "png", ""] {
        assert_eq!(decide_load_route(ext, false), LoadRoute::Worker);
    }
}

/// View-source wins over the web route: matches the pre-audit ordering (view-source was
/// checked before `try_load_web`): the `{ }` toggle must still show raw HTML source, never
/// hand an html/url file to WebView2 while the user asked to see its source.
#[cfg(feature = "html-preview")]
#[test]
fn view_source_mode_overrides_the_web_route() {
    assert_eq!(decide_load_route("html", true), LoadRoute::Worker);
    assert_eq!(decide_load_route("url", true), LoadRoute::Worker);
}

/// Without the html-preview feature, `decide_load_route`/`LoadRoute` do not exist at all,
/// so `load()` has no web-route branch and falls straight through to the worker's
/// classify-based text/card path for every extension, exactly the pre-audit fall-through
/// for a non-html-preview build. `classify` itself is what decides at that point, and
/// "html" living in `PREVIEW_TEXT_EXTS` is what resolves it to Text (compared against the
/// live toggle, not assumed on, so this can't flake on a machine where it was changed).
#[test]
fn html_falls_back_to_worker_text_classification_without_the_web_route() {
    let kind = super::content::classify("nonexistent_probe.html");
    let want = if st2k_base::settings::preview_text() {
        super::ContentKind::Text
    } else {
        // Text toggle off and no other match: `classify`'s last resort for a nonexistent
        // path (no bytes to sniff) is the info card.
        super::ContentKind::InfoCard
    };
    assert!(
        kind == want,
        "html should classify as the worker's text/card fall-through"
    );
}

/// 2026-09-05 audit, F10 adversarial review (P2): an already-shown window (an arrow-key
/// step through a folder) must never be resized down to the Loading box only to snap back
/// once content lands: `client_size` sizes by `kind`, and `ensure_shown` resizes an
/// already-shown window to whatever that current kind's size is. Only the very-first-open
/// case (not shown yet) wants the full show-and-size call.
#[test]
fn loading_state_only_resizes_before_the_window_is_shown() {
    assert!(should_size_for_loading(false));
    assert!(!should_size_for_loading(true));
}

/// 2026-09-05 audit, F10 acceptance: "switching selections repeatedly never displays an
/// older selection's completion over the newest one". A completion is current for exactly
/// the load it was started for, never for an older OR a hypothetical later generation,
/// so a naive `gen >= current` (which would let a late completion win a race it lost) or
/// `gen <= current` (which would accept a completion for a load that hasn't started yet)
/// both fail this test; only strict equality passes.
#[test]
fn a_stale_generation_is_never_current() {
    // The load this completion was started for is still the one showing: apply it.
    assert!(is_load_current(5, 5));
    // The user already switched away (repeatedly, in the acceptance scenario) before this
    // slow completion landed: an OLDER generation must never paint over the current one.
    assert!(!is_load_current(3, 5));
    assert!(!is_load_current(1, 5));
    // Defensive: `decode_gen` only ever increases, so this can't happen in practice, but the
    // check must be exact equality, not merely "not older".
    assert!(!is_load_current(7, 5));
}

/// The concrete scenario audit E02 asks for: open a slow file, then switch (via
/// `CMD_SET_PATH`, which reaches `load()` the same way a direct open does) to a fast file
/// before the slow file's worker has finished. The `--shot` headless harness decodes
/// synchronously and cannot observe this live async race from outside the process (see
/// `tests/preview_async_load.rs`'s header comment), so this pins the exact fence that
/// makes it safe: a superseded generation (the slow file's) is never current once a newer
/// one (the fast file's) exists, whichever completion lands first.
#[test]
fn a_superseded_generation_is_never_current() {
    // The slow file's load starts at generation 1 (`reset_viewer_state` bumps and returns
    // the new generation for every load, slow or fast, identically).
    let slow_gen = 1;
    // Before the slow worker reports back, the user switches to a fast file: generation 2.
    let fast_gen = 2;
    // The fast file's own worker finishes first (that's the whole point of it being fast)
    // and its completion is for the CURRENT generation: it must apply.
    assert!(
        is_load_current(fast_gen, fast_gen),
        "the fast file's own completion must be allowed to paint"
    );
    // The slow file's worker finishes afterwards and posts its stale completion: it must
    // never be allowed to paint over the fast file now showing.
    assert!(
        !is_load_current(slow_gen, fast_gen),
        "a slow file's completion arriving after a newer selection must never apply"
    );
}

/// `abandon_pending_prepare` over a LIVE ticket must count it (`AbandonTicket::is_counted`),
/// and the ticket's own worker finishing must release it (audit E02, 2026-09-07: abandoned
/// prepare work must stay OBSERVABLE). Exercised through this module's actual
/// `PENDING_PREPARE` slot, the same one `load()`/`window::on_destroy` use. Asserted on the
/// ticket's own state directly, never on the shared process-wide count: other tests in this
/// binary run budgeted workers concurrently, so a before/after read of that count races them.
#[test]
fn abandon_pending_prepare_counts_a_live_ticket_and_releases_it_on_finish() {
    let ticket = st2k_base::safety::AbandonTicket::new();
    let worker = ticket.clone();
    *super::PENDING_PREPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ticket);

    super::abandon_pending_prepare();
    assert!(
        worker.is_counted(),
        "giving up a live ticket must count it against the abandoned-worker budget"
    );

    worker.worker_finished();
    assert!(
        !worker.is_counted(),
        "the worker finishing must release what it was counted for"
    );
}

/// A path that doesn't exist can't be hex-dumped (nothing to read), so `classify`'s
/// `InfoCard` verdict must still reach the ordinary info card — exactly as it did before
/// `resolve_hex_or_card` existed. This is the regression its "fallback, not the final
/// word" design must never break: `hex_markdown` declining is not allowed to leave the
/// load with no dispatch at all.
#[test]
fn resolve_by_content_kind_falls_back_to_the_info_card_when_hex_declines() {
    let resolved = super::resolve_by_content_kind("nonexistent_probe.unknownbinaryext");
    assert!(matches!(
        resolved,
        super::Resolved::Dispatch(super::ContentKind::InfoCard)
    ));
}

#[test]
fn source_capable_reaches_eml_but_not_msg() {
    // .eml is genuine RFC-822 text (same gate as any other text preview); .msg's raw
    // bytes are an OLE compound file with no text view to show.
    assert_eq!(source_capable("eml"), st2k_base::settings::preview_text());
    assert!(!source_capable("msg"));
}

/// Windows commonly writes a `.url` shortcut with a non-ASCII target as UTF-16 LE
/// with a BOM — a plain `read_to_string` (UTF-8 only) used to fail on these outright,
/// so the live-preview feature never engaged for them.
#[cfg(feature = "html-preview")]
#[test]
fn parse_url_shortcut_reads_a_utf16_le_target_with_bom() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_loader_url_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shortcut.url");

    let text = "[InternetShortcut]\r\nURL=https://example.com/caf\u{e9}\r\n";
    let mut bytes = vec![0xFFu8, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, &bytes).unwrap();

    let got = parse_url_shortcut(path.to_str().unwrap());
    assert_eq!(got.as_deref(), Some("https://example.com/caf\u{e9}"));

    // Past the size cap it is not a shortcut, whatever its text says.
    let big = dir.join("big.url");
    let mut text = String::from("[InternetShortcut]\r\nURL=https://example.com/\r\n");
    text.push_str(&" ".repeat(2 << 20));
    std::fs::write(&big, text).unwrap();
    assert_eq!(parse_url_shortcut(big.to_str().unwrap()), None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A literal `%` in a file name is escaped, or WebView2 decodes `a%2Fb.html` to `a/b.html`.
#[cfg(feature = "html-preview")]
#[test]
fn file_uri_escapes_a_percent_before_anything_else() {
    use super::web::file_uri;
    assert_eq!(
        file_uri(r"C:\pages\a%2Fb.html"),
        "file:///C:/pages/a%252Fb.html"
    );
    assert_eq!(
        file_uri(r"C:\my pages\x#1?.html"),
        "file:///C:/my%20pages/x%231%3F.html"
    );
}

#[test]
fn a_remembered_size_is_kept_when_it_fits() {
    let got = clamp_remembered_size((1200, 800), (400, 200), (1920, 1040));
    assert_eq!(got, (1200, 800));
}

#[test]
fn a_remembered_size_from_a_bigger_screen_shrinks_to_this_one() {
    let got = clamp_remembered_size((3800, 2000), (400, 200), (1920, 1040));
    assert_eq!(got, (1920, 1040));
}

#[test]
fn a_remembered_size_never_drops_below_the_window_minimum() {
    // Even a work area smaller than the minimum must not produce a sub-minimum size —
    // `WM_GETMINMAXINFO` would refuse it and the two would disagree every resize.
    assert_eq!(
        clamp_remembered_size((50, 50), (400, 200), (1920, 1040)),
        (400, 200)
    );
    assert_eq!(
        clamp_remembered_size((900, 700), (400, 200), (320, 160)),
        (400, 200)
    );
}
