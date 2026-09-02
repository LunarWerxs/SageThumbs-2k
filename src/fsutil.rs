//! Small filesystem helpers shared across the verb / strip write paths.

use std::path::Path;
use std::time::Duration;

/// A fresh write or a move can briefly hit a transient Explorer / thumbnail-cache
/// lock on the destination (Windows os error 5/32). We retry a few times with a
/// short backoff before giving up. These consts are the retry POLICY in ONE place
/// — they used to be hand-copied as `0..5` / `from_millis(40)` in four loops.
const RENAME_RETRIES: u32 = 5;
/// `pub(crate)` because the three "survives a transient lock" tests hold their lock for
/// exactly one interval of it. Hard-coding a duration in those tests is what made them
/// flaky enough to block a release; see DEVELOPMENT_GOTCHAS.
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
    for _ in 0..RENAME_RETRIES {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = is_transient(&e);
                last = Err(e);
                if !transient {
                    return last;
                }
            }
        }
        std::thread::sleep(RENAME_BACKOFF);
    }
    last
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
