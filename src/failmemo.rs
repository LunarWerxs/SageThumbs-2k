//! Per-file failure memory (a circuit breaker) for the thumbnail provider.
//!
//! Without this, a hostile or broken file that fails or times out to decode is
//! re-decoded at full cost on every Explorer redraw. This keeps a small,
//! bounded, process-local memory of files whose decode FAILED, keyed on the
//! identity the shell's `IStream` reports for it (name, size, modified time -
//! see [`Identity`]). A later hit on the same identity skips the decode and
//! returns a failure immediately; a file whose size or modified time changed
//! is a different identity and is always retried. Only failures are ever
//! remembered - a success is never recorded here.
//!
//! `GetThumbnail` in [`crate::thumbprovider`] is the only production caller.

use std::collections::VecDeque;
use std::sync::{Mutex, TryLockError};

/// How long a remembered failure is honored before the file is retried anyway. Short on
/// purpose: the provider cannot tell a file that will never decode from one that failed
/// for a passing reason (a locked file, a wedged Media Foundation still inside its grace
/// period), and the cost being avoided is the re-decode on every redraw of the SAME
/// Explorer view, which happens within seconds, not minutes.
const COOLDOWN_MS: u64 = 2 * 60 * 1000;

/// Most failure entries kept at once; the oldest is evicted to make room for a new one.
const MAX_ENTRIES: usize = 256;

/// The identity the provider has for a stream: its reported name, size, and modified
/// time, from one `IStream::Stat` call (see `thumbprovider::stream_identity`). A file
/// whose size or modified time changed is a different identity, so a past failure
/// recorded against the old one never matches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) mtime: u64,
}

/// One remembered failure and when the memory of it expires.
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    size: u64,
    mtime: u64,
    expires_at_ms: u64,
}

impl Entry {
    fn matches(&self, id: &Identity) -> bool {
        self.name == id.name && self.size == id.size && self.mtime == id.mtime
    }
}

/// The failure memory itself. A plain type (not just a bare static) so tests can build
/// their own isolated instance instead of sharing the one process-wide table - `cargo
/// test` runs this crate's tests concurrently, and a single shared table would make the
/// eviction/expiry tests race each other.
struct FailMemo {
    entries: Mutex<VecDeque<Entry>>,
}

impl FailMemo {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Whether `id` is a remembered, not-yet-expired failure at `now_ms`. A contended
    /// lock reports `false` (not remembered) rather than blocking the caller - this
    /// runs on the COM thread the shell is waiting on. A poisoned lock (some other
    /// caller panicked while holding it) is recovered and used rather than propagated:
    /// the entries themselves are still valid data, a panic elsewhere does not corrupt
    /// them.
    fn is_remembered_failure_at(&self, id: &Identity, now_ms: u64) -> bool {
        self.with_entries(false, |entries| {
            entries
                .iter()
                .any(|e| e.matches(id) && e.expires_at_ms > now_ms)
        })
    }

    /// Record `id` as a failure as of `now_ms`, evicting the oldest entry first if the
    /// table is already at capacity. Contention or a poisoned lock is handled the same
    /// way as [`Self::is_remembered_failure_at`]; on contention the record is simply
    /// dropped (a missed circuit-breaker entry costs one extra re-decode next time, not
    /// correctness).
    fn record_failure_at(&self, id: Identity, now_ms: u64) {
        self.with_entries((), |entries| {
            if entries.len() >= MAX_ENTRIES {
                entries.pop_front();
            }
            entries.push_back(Entry {
                name: id.name,
                size: id.size,
                mtime: id.mtime,
                expires_at_ms: now_ms.saturating_add(COOLDOWN_MS),
            });
        });
    }

    fn with_entries<R>(&self, default: R, f: impl FnOnce(&mut VecDeque<Entry>) -> R) -> R {
        match self.entries.try_lock() {
            Ok(mut guard) => f(&mut guard),
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut guard = poisoned.into_inner();
                f(&mut guard)
            }
            Err(TryLockError::WouldBlock) => default,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.with_entries(0, |entries| entries.len())
    }
}

/// The one process-wide failure memory.
static MEMORY: FailMemo = FailMemo::new();

/// Whether `id` is a remembered failure right now. See
/// [`FailMemo::is_remembered_failure_at`].
pub(crate) fn is_remembered_failure(id: &Identity) -> bool {
    MEMORY.is_remembered_failure_at(id, crate::safety::elapsed_ms())
}

/// Record `id` as a failure right now. See [`FailMemo::record_failure_at`].
pub(crate) fn record_failure(id: Identity) {
    MEMORY.record_failure_at(id, crate::safety::elapsed_ms());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str, size: u64, mtime: u64) -> Identity {
        Identity {
            name: name.to_string(),
            size,
            mtime,
        }
    }

    /// A recorded failure is a hit on the same identity, and a miss before it is
    /// recorded.
    #[test]
    fn insert_then_hit() {
        let memo = FailMemo::new();
        let a = id("a.png", 100, 1);
        assert!(!memo.is_remembered_failure_at(&a, 0));
        memo.record_failure_at(a.clone(), 0);
        assert!(memo.is_remembered_failure_at(&a, 0));
    }

    /// A remembered failure stops being a hit once its cooldown has elapsed. Driven with
    /// an injected clock, so the test asserts the real policy without sleeping.
    #[test]
    fn expiry_without_sleeping() {
        let memo = FailMemo::new();
        let a = id("a.png", 100, 1);
        memo.record_failure_at(a.clone(), 1_000);
        assert!(memo.is_remembered_failure_at(&a, 1_000 + COOLDOWN_MS - 1));
        assert!(!memo.is_remembered_failure_at(&a, 1_000 + COOLDOWN_MS));
    }

    /// Past capacity, the oldest entry is evicted to make room for the newest one, and
    /// the table never grows past the cap.
    #[test]
    fn eviction_at_capacity() {
        let memo = FailMemo::new();
        for i in 0..MAX_ENTRIES {
            memo.record_failure_at(id(&format!("f{i}.png"), 1, 1), 0);
        }
        assert_eq!(memo.len(), MAX_ENTRIES);
        let oldest = id("f0.png", 1, 1);
        assert!(memo.is_remembered_failure_at(&oldest, 0));

        memo.record_failure_at(id("new.png", 1, 1), 0);
        assert_eq!(
            memo.len(),
            MAX_ENTRIES,
            "the table must never grow past the cap"
        );
        assert!(
            !memo.is_remembered_failure_at(&oldest, 0),
            "the oldest entry must be evicted first"
        );
        assert!(memo.is_remembered_failure_at(&id("new.png", 1, 1), 0));
    }

    /// A file that changed size since the recorded failure is a different identity, so
    /// it is a miss (retried), not a hit.
    #[test]
    fn changed_size_is_a_miss() {
        let memo = FailMemo::new();
        memo.record_failure_at(id("a.png", 100, 1), 0);
        assert!(!memo.is_remembered_failure_at(&id("a.png", 200, 1), 0));
    }

    /// Same, for a changed modified time.
    #[test]
    fn changed_mtime_is_a_miss() {
        let memo = FailMemo::new();
        memo.record_failure_at(id("a.png", 100, 1), 0);
        assert!(!memo.is_remembered_failure_at(&id("a.png", 100, 2), 0));
    }

    /// A lock poisoned by a panic elsewhere must be recovered, not propagated or
    /// unwrapped - the entries it guards are still valid data.
    #[test]
    fn poisoned_lock_is_recovered() {
        let memo = FailMemo::new();
        let a = id("a.png", 100, 1);
        memo.record_failure_at(a.clone(), 0);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = memo.entries.lock().unwrap();
            panic!("simulated panic while holding the lock");
        }));
        assert!(poisoned.is_err());
        assert!(memo.entries.is_poisoned());

        // The entry recorded before the panic must still be found, and the memo must
        // still accept new records afterward.
        assert!(memo.is_remembered_failure_at(&a, 0));
        memo.record_failure_at(id("b.png", 1, 1), 0);
        assert!(memo.is_remembered_failure_at(&id("b.png", 1, 1), 0));
    }
}
