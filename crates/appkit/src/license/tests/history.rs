#![cfg(test)]

//! The licence history lock: racing writers, and a lock that times out.

use super::*;

/// Two "sessions" (two OS threads, each opening its own `File` handle to the SAME lock
/// path - which is what actually distinguishes two logon sessions on Windows, not
/// which thread happens to run the code) both bump the breadcrumb through
/// [`update_history_at`] at the same time. If the lock is real mutual exclusion,
/// neither writer's read-modify-write critical section can overlap the other's, so
/// BOTH increments land - there is no interleaving in which one is lost, independent
/// of thread scheduling. Against the pre-fix `Local\` mutex this same shape would not
/// even prove anything (two threads in one test process share one logon session, so
/// that mutex serializes them too); what the pre-fix design could not survive is two
/// DIFFERENT sessions, which `without_a_shared_lock_two_racing_writers_can_lose_an_update`
/// below reproduces directly, since a real second session can't be created here.
#[test]
fn two_concurrent_sessions_through_the_lock_both_preserve_their_change() {
    let dir = temp_dir("lock_concurrent");
    let path = dir.join("license-history.json");
    assert!(write_history(&path, &History::default()));

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut joins = Vec::new();
    for who in 0..2u64 {
        let path = path.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        joins.push(std::thread::spawn(move || {
            barrier.wait(); // start both "sessions" as close together as possible
                            // `update_history_at` refuses to write when the lock is not free within
                            // its (test-shortened, ~150 ms) budget, and a slow CI runner can hold the
                            // other session's read-modify-write open longer than that: on 2026-09-08
                            // the GitHub windows runner did, and this test blamed the LOCK for an
                            // increment that was never attempted. So a session that lost the lock
                            // race tries again, as a real second session would on its next run. A lock
                            // that let the second writer THROUGH is still caught below: that write
                            // succeeds and clobbers, and the count comes out one short whatever the
                            // retries did.
            let mut tries = 0u32;
            loop {
                let wrote = update_history_at(&path, |h| {
                    h.nag_count += 1;
                    if who == 0 {
                        h.key_prefix = "esk_SESA".to_string();
                    } else {
                        h.last_status = "session-b".to_string();
                    }
                });
                if wrote {
                    break;
                }
                tries += 1;
                assert!(
                    tries < 200,
                    "session {who} could not take the history lock in 200 tries"
                );
            }
        }));
    }
    for j in joins {
        j.join().expect("writer thread must not panic");
    }

    let result = read_history(&path).expect("breadcrumb must still parse");
    assert_eq!(
        result.nag_count, 2,
        "both sessions' increments must land - a lost update means the lock let a \
         second writer through while the first's read-modify-write was in flight"
    );
    assert_eq!(result.key_prefix, "esk_SESA", "session A's field survives");
    assert_eq!(
        result.last_status, "session-b",
        "session B's field ALSO survives"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// What the OLD `Local\SageThumbs2K.LicenceHistory` mutex actually gave two DIFFERENT
/// Windows sessions: nothing. Each session's `CreateMutexW` opens or creates a mutex
/// object in ITS OWN session namespace, so from a second session's point of view there
/// was no lock in play at all - contending with your own session's other threads,
/// never with the other session's. Two real logon sessions can't be created in a test
/// (see the finding's Acceptance note), so this pins the exact interleaving a missing
/// cross-session lock permits directly, in program order rather than as a timing
/// gamble: both "sessions" read the SAME starting state before either writes, so the
/// second write has no idea about the first's change and destroys it.
///
/// This is what makes the fix's teeth visible: replace the two direct
/// `read_history`/`write_history` calls below with two calls to `update_history_at`
/// sharing one lock path, and this exact interleaving becomes impossible - the second
/// reader cannot observe the pre-first-write state, because the first writer's whole
/// read-modify-write section (including the write) completes, under the lock, before
/// the second's read-modify-write section is even allowed to start.
#[test]
fn without_a_shared_lock_two_racing_writers_can_lose_an_update() {
    let dir = temp_dir("racy_no_lock");
    let path = dir.join("license-history.json");
    assert!(write_history(&path, &History::default()));

    // "Session A" reads first...
    let mut a = read_history(&path).expect("seed must parse");
    a.nag_count = 1;
    // ...then "session B" reads the SAME pre-A-write state: no shared lock stops it,
    // exactly as two different sessions' own separate mutexes would not stop it.
    let mut b = read_history(&path).expect("seed must parse");
    b.last_status = "session-b".to_string();
    // B writes first...
    assert!(write_history(&path, &b));
    // ...and A's write, computed from data that predates B's, silently destroys it.
    assert!(write_history(&path, &a));

    let result = read_history(&path).expect("breadcrumb must still parse");
    assert_eq!(result.nag_count, 1, "A's change survives - it wrote last");
    assert_eq!(
        result.last_status, "",
        "B's change was silently lost: this is the F18 bug, reproduced without a \
         shared lock standing between the two writers"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A lock that cannot be taken must not fall back to writing unlocked - that fallback
/// is exactly the old bug's other half (see the module docs on `HistoryLock`). Hold
/// the lock file open in this thread for the whole test, so every retry
/// [`update_history_at`] makes is guaranteed to fail, then assert it gives up having
/// written nothing: the breadcrumb on disk is still byte-for-byte the seed, never the
/// mutation the caller asked for.
#[test]
fn a_lock_that_times_out_writes_nothing_rather_than_clobbering_newer_history() {
    let dir = temp_dir("lock_timeout");
    let path = dir.join("license-history.json");
    let seed = History {
        was_business: true,
        last_status: "active".to_string(),
        nag_count: 3,
        ..History::default()
    };
    assert!(write_history(&path, &seed));

    // Hold the lock ourselves for the whole test - every attempt inside
    // `update_history_at` below must see it already taken.
    let held = HistoryLock::acquire(&lock_path(&path));
    assert!(
        held.is_some(),
        "the test's own lock acquisition must succeed"
    );

    let wrote = update_history_at(&path, |h| {
        h.nag_count = 999;
        h.last_status = "this must never reach disk".to_string();
    });
    assert!(!wrote, "a timed-out lock must report no write happened");

    drop(held);

    let result = read_history(&path).expect("breadcrumb must still parse");
    assert_eq!(
        result, seed,
        "the file must be untouched: a lock timeout must never silently replace \
         history with a write that never actually held the lock"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
