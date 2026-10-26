#![cfg(test)]

use super::*;

fn mutes_here() -> u32 {
    super::logfile::MUTED.with(std::cell::Cell::get)
}

/// A muted thread writes no debug lines whatever the registry says, nested mutes stack, and a
/// panic inside the body still unmutes: the fuzz harness catches thousands of them.
#[test]
fn a_muted_thread_drops_debug_lines_until_the_body_ends_even_by_panic() {
    with_debug_log_muted(|| {
        with_debug_log_muted(|| {
            assert_eq!(mutes_here(), 2);
            assert!(!debug_logging_on());
        });
        assert_eq!(mutes_here(), 1, "the outer mute outlives the inner one");
        assert!(!debug_logging_on());
    });
    assert_eq!(mutes_here(), 0);
    let unwound = std::panic::catch_unwind(|| with_debug_log_muted(|| panic!("a broken input")));
    assert!(unwound.is_err());
    assert_eq!(
        mutes_here(),
        0,
        "a panic in the body must not leave the thread muted"
    );
}

/// Another thread is not muted by this one's call.
#[test]
fn a_mute_is_this_threads_alone() {
    with_debug_log_muted(|| {
        assert_eq!(std::thread::spawn(mutes_here).join().unwrap(), 0);
    });
}
