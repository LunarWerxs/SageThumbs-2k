//! Small filesystem helpers shared across the verb / strip write paths.

use std::path::Path;
use std::time::Duration;

/// A fresh write or a move can briefly hit a transient Explorer / thumbnail-cache
/// lock on the destination (Windows os error 5/32). We retry a few times with a
/// short backoff before giving up. These consts are the retry POLICY in ONE place
/// — they used to be hand-copied as `0..5` / `from_millis(40)` in four loops.
const RENAME_RETRIES: u32 = 5;
/// `pub(crate)` because `strip.rs`'s `ReplaceFileW` loop retries on the same policy and must
/// not grow a second copy of the number. It is NO LONGER part of any test's timing: the
/// "survives a transient lock" tests used to sleep one interval of it before releasing their
/// lock, and that guess is what made them flaky (see [`on_transient_failure`]).
pub(crate) const RENAME_BACKOFF: Duration = Duration::from_millis(40);

/// `ERROR_ACCESS_DENIED` — also returned for a rename onto a target another process
/// (Explorer, a thumbnail cache scan, an AV scanner) briefly holds open.
const ERROR_ACCESS_DENIED: i32 = 5;
/// `ERROR_SHARING_VIOLATION` — the target is open elsewhere with no sharing.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether `e` is one of the two documented transient lock codes (see the module
/// docs above) worth retrying. A permission failure on a genuinely read-only
/// destination, a cross-volume rename, or a path-too-long error all surface as
/// DIFFERENT codes and are permanent — sleeping `RENAME_RETRIES` times before
/// reporting them back just adds ~200 ms of dead time per file (~100 s across a
/// 500-file batch against, say, a read-only destination).
fn is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
    )
}

/// Rename `from` → `to`, retrying past a transient lock. Returns the final
/// `std::io::Result`: `Ok` on success, else the LAST error once the retries are
/// spent (or the first error, for a non-transient one — see [`is_transient`]).
/// Callers keep their own temp cleanup and error mapping.
pub(crate) fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for attempt in 1..=RENAME_RETRIES {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = is_transient(&e);
                last = Err(e);
                if !transient {
                    return last;
                }
                note_transient_failure(attempt);
            }
        }
        std::thread::sleep(RENAME_BACKOFF);
    }
    last
}

/// Compiled out of every shipping build — see [`on_transient_failure`] for what it is for.
#[cfg(not(test))]
#[inline(always)]
fn note_transient_failure(_attempt: u32) {}

/// The hook slot's type, named so `clippy::type_complexity` has something to read. `FnMut`
/// rather than `Fn` because the one real hook takes its handle out of an `Option` on first call.
#[cfg(test)]
type TransientFailureHook = std::cell::RefCell<Option<Box<dyn FnMut(u32)>>>;

#[cfg(test)]
thread_local! {
    /// Per-thread hook, so two tests running in parallel cannot see each other's.
    static AFTER_TRANSIENT_FAILURE: TransientFailureHook = const { std::cell::RefCell::new(None) };
}

/// Test seam: install a hook that runs after each FAILED transient attempt, on the retrying
/// thread, before the backoff sleep. Its argument is the 1-based attempt that just failed.
///
/// ⛔ It exists to DELETE a timing window, not to widen one. The three "survives a transient
/// lock" tests hold a real no-sharing handle on the rename destination and have to let go of
/// it mid-retry. They used to do that on a wall-clock guess — one `RENAME_BACKOFF` measured
/// from the moment a WATCHER THREAD happened to notice the staged temp file — and that guess
/// is wrong in both directions. Too eager and the lock is gone before the rename ever meets
/// it, so the test passes with the retry loop deleted (measured: it did). Too late — which is
/// all a loaded 900-test run has to do to a background thread — and the release lands after
/// the ~200 ms budget is spent, so a CORRECT build fails; that is the shape that blocked a
/// release once and failed twice more under `cargo test --workspace` on 2026-09-05.
///
/// Releasing from here is ordered by the retry loop itself: the lock is held for attempt 1
/// and gone before attempt 2, on every machine, at every load, with no clock anywhere. The
/// hook also makes the tests provably non-vacuous — it cannot fire unless the production path
/// really went through this function and really met the lock, so a test asserts it fired.
#[cfg(test)]
pub(crate) fn on_transient_failure(hook: impl FnMut(u32) + 'static) {
    AFTER_TRANSIENT_FAILURE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Drop this thread's hook. Rust's test harness gives each test its own thread, so this is
/// belt-and-braces rather than load-bearing.
#[cfg(test)]
pub(crate) fn clear_transient_failure_hook() {
    AFTER_TRANSIENT_FAILURE.with(|slot| *slot.borrow_mut() = None);
}

/// The hook is taken OUT of the cell for the duration of the call and put back after, so a
/// hook that itself touches the filesystem (and re-enters this function) cannot panic on an
/// already-borrowed `RefCell`.
#[cfg(test)]
fn note_transient_failure(attempt: u32) {
    let taken = AFTER_TRANSIENT_FAILURE.with(|slot| slot.borrow_mut().take());
    if let Some(mut hook) = taken {
        hook(attempt);
        AFTER_TRANSIENT_FAILURE.with(|slot| *slot.borrow_mut() = Some(hook));
    }
}

/// Hold `path` open with NO sharing — a real Windows lock, which a rename onto it reports as
/// os error 5/32 — and hand the handle to [`on_transient_failure`] so it is dropped the moment
/// the FIRST attempt fails. Returns the count of failed attempts, which the caller asserts is
/// non-zero: that is what proves the rename actually MET the lock rather than sailing past an
/// expired one.
///
/// Lives here, beside the loop that orders it, because three call sites in two modules
/// (`foldericon`'s two writers and `wallpaper`'s) need exactly this and used to carry a copy
/// each of a watcher thread and a hard-coded sleep.
#[cfg(test)]
pub(crate) fn lock_until_first_retry(path: &Path) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .expect("the test's own lock target must open");
    let mut handle = Some(held);
    let failures = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&failures);
    on_transient_failure(move |_attempt| {
        counter.fetch_add(1, Ordering::SeqCst);
        drop(handle.take());
    });
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-transient rename failure (here: the source doesn't exist, `ERROR_FILE_NOT_FOUND` =
    /// 2) must return on the FIRST attempt, not after `RENAME_RETRIES` sleeps. Timing the call
    /// pins the fix without depending on the exact error code Windows happens to raise for every
    /// non-transient case (cross-volume / path-too-long are awkward to reproduce deterministically
    /// in-process) — a genuinely transient case sleeps at least once before giving up, so a fast
    /// return proves the retry loop was skipped.
    #[test]
    fn rename_retrying_does_not_retry_a_non_transient_error() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_fsutil_nontransient_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing_src = dir.join("does-not-exist.bin");
        let dest = dir.join("dest.bin");

        let start = std::time::Instant::now();
        let result = rename_retrying(&missing_src, &dest);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "renaming a missing source must fail");
        assert!(
            elapsed < RENAME_BACKOFF,
            "a non-transient error must return before ever sleeping a retry backoff, \
             took {elapsed:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `is_transient` is the retry gate itself — pin its two positive codes and a
    /// representative negative one directly, since the behavioural test above can only
    /// prove ONE non-transient code returns immediately.
    #[test]
    fn is_transient_matches_only_the_documented_codes() {
        let make = |code: i32| std::io::Error::from_raw_os_error(code);
        assert!(is_transient(&make(ERROR_ACCESS_DENIED)));
        assert!(is_transient(&make(ERROR_SHARING_VIOLATION)));
        assert!(!is_transient(&make(17))); // ERROR_NOT_SAME_DEVICE (cross-volume)
        assert!(!is_transient(&make(206))); // ERROR_FILENAME_EXCED_RANGE (path too long)
        assert!(!is_transient(&make(2))); // ERROR_FILE_NOT_FOUND
    }
}
