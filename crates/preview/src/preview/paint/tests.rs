#![cfg(test)]

use super::*;
use std::cell::Cell;

/// Fabricate a distinct-but-harmless `HWND` for cache-key tests: [`cached_text_lang`] only
/// ever uses `.0` as a hashmap key and never dereferences it, so an arbitrary value is safe.
fn fake_hwnd(n: isize) -> HWND {
    HWND(n as *mut std::ffi::c_void)
}

/// The whole point of the fix (paint.rs used to re-derive the Text pane's syntax language
/// on every `WM_PAINT` — every scroll notch, every hover redraw): a repaint at the SAME
/// load generation must reuse the cached language, not recompute it, and a NEW load (the
/// generation bumping, e.g. arrow-nav to the next file in the same window) must invalidate
/// the cache rather than keep serving the previous file's language forever.
#[test]
fn cached_text_lang_recomputes_only_when_the_load_generation_changes() {
    let hwnd = fake_hwnd(0x1111);
    let calls = Cell::new(0);
    let compute_rust = || {
        calls.set(calls.get() + 1);
        highlight::Lang::Rust
    };

    assert!(cached_text_lang(hwnd, 1, compute_rust) == highlight::Lang::Rust);
    assert_eq!(calls.get(), 1);

    // Same generation, second repaint: must NOT recompute.
    assert!(cached_text_lang(hwnd, 1, compute_rust) == highlight::Lang::Rust);
    assert_eq!(
        calls.get(),
        1,
        "must not recompute while decode_gen is unchanged"
    );

    // New generation: must invalidate and recompute, and pick up the NEW answer.
    let compute_py = || {
        calls.set(calls.get() + 1);
        highlight::Lang::Py
    };
    assert!(cached_text_lang(hwnd, 2, compute_py) == highlight::Lang::Py);
    assert_eq!(calls.get(), 2);
}

/// Two different windows must not share a cache slot — an arbitrary HWND collision would
/// paint one preview window's file in another window's language.
#[test]
fn cached_text_lang_keys_are_per_window() {
    let a = fake_hwnd(0x2222);
    let b = fake_hwnd(0x3333);
    assert!(cached_text_lang(a, 1, || highlight::Lang::Rust) == highlight::Lang::Rust);
    assert!(cached_text_lang(b, 1, || highlight::Lang::Py) == highlight::Lang::Py);
    // `a`'s entry must still read back Rust, not have been clobbered by `b`'s insert.
    assert!(cached_text_lang(a, 1, || highlight::Lang::Py) == highlight::Lang::Rust);
}

/// A091: WM_PAINT used to `CreateCompatibleBitmap` a fresh full-client back buffer (tens of
/// MB at 4K) and delete it on EVERY repaint, instead of caching one sized to the client and
/// only rebuilding it when that size actually changes (WM_SIZE). Without the cache, this
/// predicate would need to return `true` unconditionally (every paint reallocates); with it,
/// a same-size repaint must reuse the buffer and only a real size change forces a rebuild.
#[test]
fn back_buffer_reused_across_repaints_reallocated_on_resize() {
    // Nothing cached yet (first paint, or just freed by WM_SIZE/WM_DESTROY) — must allocate.
    assert!(back_buffer_needs_alloc(None, (800, 600)));
    // Same client size as what's cached (a scroll notch, a hover redraw) — must NOT
    // reallocate. This is the fix: the old code had no such check at all.
    assert!(!back_buffer_needs_alloc(Some((800, 600)), (800, 600)));
    // The client size actually changed (WM_SIZE) — the cached bitmap no longer matches the
    // window and MUST be rebuilt, or the next paint would blit a stale-size buffer.
    assert!(back_buffer_needs_alloc(Some((800, 600)), (1024, 768)));
}

/// Audit F29 (2026-09-06): the outline sidebar's English header text is unchanged from the
/// pre-fix value, so the visible English UI never moved. That alone proves nothing about
/// localization - it only means something once paired with the source-contract test
/// (`tests/f29_screenshot_i18n_contract.rs`), which shows the old code shape feeding
/// straight into `encode_utf16` is gone from this file, and the locale-diff test showing
/// another shipped locale's translation of this key actually differs from English.
#[test]
fn outline_header_matches_the_locale_tables_english_value() {
    assert_eq!(st2k_appkit::win::t("preview_outline_header"), "CONTENTS");
}
