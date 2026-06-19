//! Small filesystem helpers shared across the verb / strip write paths.

use std::path::Path;
use std::time::Duration;

/// A fresh write or a move can briefly hit a transient Explorer / thumbnail-cache
/// lock on the destination (Windows os error 5/32). We retry a few times with a
/// short backoff before giving up. These consts are the retry POLICY in ONE place
/// — they used to be hand-copied as `0..5` / `from_millis(40)` in four loops.
const RENAME_RETRIES: u32 = 5;
const RENAME_BACKOFF: Duration = Duration::from_millis(40);

/// Rename `from` → `to`, retrying past a transient lock. Returns the final
/// `std::io::Result`: `Ok` on success, else the LAST error once the retries are
/// spent. Callers keep their own temp cleanup and error mapping.
pub(crate) fn rename_retrying(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..RENAME_RETRIES {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last = Err(e),
        }
        std::thread::sleep(RENAME_BACKOFF);
    }
    last
}
