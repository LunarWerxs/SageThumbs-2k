#![cfg(test)]

use super::*;

/// The bug this guards: a `PostMessageW` failure used to be silently swallowed (`let _ =
/// ...`), so a boxed `Available(tag)` string was never freed — `WM_ABOUT_CHECKED`'s own
/// reclaim only runs for a message that actually reached the queue.
#[test]
fn a_failed_post_carrying_a_boxed_tag_must_reclaim() {
    assert!(post_failed_leaks_tag(false, 0x1000));
}

/// `UpToDate`/`Failed` results carry `lp == 0` (nothing boxed) — a failed post must not
/// try to reclaim a null pointer.
#[test]
fn a_failed_post_with_no_tag_needs_no_reclaim() {
    assert!(!post_failed_leaks_tag(false, 0));
}

/// A successful post means the message reached the queue, so `WM_ABOUT_CHECKED` now owns
/// the tag (either it consumes it, or its own null-check reclaims it) — the worker thread
/// must not double-free by also reclaiming here.
#[test]
fn a_successful_post_never_reclaims_on_the_worker_thread() {
    assert!(!post_failed_leaks_tag(true, 0x1000));
}
