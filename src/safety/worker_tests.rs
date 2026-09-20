#![cfg(test)]

use super::*;

/// A fresh baseline counter plus the caller/worker state word that every interleaving
/// scenario below starts from: a running worker, nothing abandoned yet.
fn fresh_abandoned_state() -> (AtomicU64, AtomicU8) {
    (AtomicU64::new(0), AtomicU8::new(WORKER_RUNNING))
}

/// The abandoned count must read exactly `expected`; `msg` names the interleaving under
/// test, so a failure still points at the ordering that broke.
fn assert_abandoned_count(count: &AtomicU64, expected: u64, msg: &str) {
    assert_eq!(count.load(Ordering::Acquire), expected, "{msg}");
}

/// The caller/worker handshake behind the abandoned-worker count: whichever side marks
/// second sees the other's mark, so an increment is always paired with exactly one
/// decrement, and a worker that finished before the caller gave up counts for nothing.
#[test]
fn abandoned_handshake_pairs_increment_with_decrement() {
    // Caller gives up first, worker finishes later: count, then uncount.
    let s = AtomicU8::new(WORKER_RUNNING);
    assert!(worker_abandoned(&s), "caller owns the increment");
    assert!(worker_finished(&s), "worker owns the decrement");

    // Worker finishes first: nothing to count on either side.
    let s = AtomicU8::new(WORKER_RUNNING);
    assert!(!worker_finished(&s));
    assert!(!worker_abandoned(&s));
}

/// Every interleaving of the worker's finish against the caller's two accounting steps
/// must leave the count at its baseline once both sides are done, and must count the
/// worker as abandoned for exactly the window in which it really is. Driven against a
/// LOCAL counter with the steps called by hand, so each ordering is exercised
/// deterministically rather than hoped for under a scheduler.
///
/// The middle case is the one that used to fail: the caller published ABANDONED, the
/// worker finished and decremented a count that was still zero (no-op), and the caller
/// then incremented. That phantom was never removed, and eight of them shut every
/// budgeted worker out of the host for good. The reproduction is `repros/
/// f02_worker_counter_repro.rs` in the 2026-09-05 audit; this pins the fix.
#[test]
fn abandoned_count_returns_to_baseline_on_every_interleaving() {
    // Worker finishes BEFORE the caller gives up: nothing is ever counted.
    let (count, s) = fresh_abandoned_state();
    finish_worker(&s, &count);
    reserve_abandoned(&count);
    publish_abandoned(&s, &count);
    assert_abandoned_count(&count, 0, "worker-first must count nothing");

    // Worker finishes BETWEEN the caller's reservation and its publication: the
    // reservation is undone, and the worker (which saw RUNNING) touched nothing.
    let (count, s) = fresh_abandoned_state();
    reserve_abandoned(&count);
    finish_worker(&s, &count);
    publish_abandoned(&s, &count);
    assert_abandoned_count(
        &count,
        0,
        "a worker finishing inside the caller's gap must leave no phantom",
    );

    // Worker finishes AFTER the caller gave up: counted while late, uncounted when done.
    let (count, s) = fresh_abandoned_state();
    reserve_abandoned(&count);
    publish_abandoned(&s, &count);
    assert_abandoned_count(&count, 1, "a late worker is counted");
    finish_worker(&s, &count);
    assert_abandoned_count(&count, 0, "and uncounted when it finishes");

    // Both sides repeated: a caller that times out and later drops its handle, a worker
    // path that reports twice. Neither may move the count a second time.
    let (count, s) = fresh_abandoned_state();
    reserve_abandoned(&count);
    publish_abandoned(&s, &count);
    reserve_abandoned(&count);
    publish_abandoned(&s, &count);
    assert_abandoned_count(&count, 1, "a second give-up is a no-op");
    finish_worker(&s, &count);
    finish_worker(&s, &count);
    assert_abandoned_count(&count, 0, "a second finish is a no-op");
}

/// The refusal predicate over the same local counter: eight workers abandoned in the
/// racy ordering used to leave eight phantoms and a permanent refusal. Now the same
/// eight leave zero, the ninth start is allowed, and the threshold trips only while
/// eight workers are GENUINELY still running late, recovering as they finish.
#[test]
fn eight_workers_finishing_inside_the_gap_do_not_exhaust_the_budget() {
    let count = AtomicU64::new(0);
    let exhausted = |c: &AtomicU64| c.load(Ordering::Acquire) >= MAX_ABANDONED_WORKERS;

    for _ in 0..MAX_ABANDONED_WORKERS {
        let s = AtomicU8::new(WORKER_RUNNING);
        reserve_abandoned(&count);
        finish_worker(&s, &count); // the worker slips in before the publication
        publish_abandoned(&s, &count);
    }
    assert_eq!(count.load(Ordering::Acquire), 0);
    assert!(
        !exhausted(&count),
        "no phantoms, so the budget must still be open"
    );

    // Eight genuinely late workers DO trip it, and finishing them reopens it.
    let states: Vec<AtomicU8> = (0..MAX_ABANDONED_WORKERS)
        .map(|_| AtomicU8::new(WORKER_RUNNING))
        .collect();
    for s in &states {
        reserve_abandoned(&count);
        publish_abandoned(s, &count);
    }
    assert!(
        exhausted(&count),
        "eight live late workers exhaust the budget"
    );
    finish_worker(&states[0], &count);
    assert!(!exhausted(&count), "one finishing reopens it");
    for s in &states[1..] {
        finish_worker(s, &count);
    }
    assert_eq!(count.load(Ordering::Acquire), 0, "back to baseline");
}

/// The public ticket wraps the same steps around the process-wide counter. The per-ticket
/// state is asserted exactly; the shared count only relatively, since other tests in this
/// binary run budgeted workers concurrently.
#[test]
fn abandon_ticket_counts_only_while_the_worker_is_genuinely_late() {
    let t = AbandonTicket::new();
    let w = t.clone();
    w.worker_finished();
    t.caller_gave_up();
    assert!(
        !t.is_counted(),
        "a worker that finished first must not be counted"
    );

    let t = AbandonTicket::new();
    let w = t.clone();
    t.caller_gave_up();
    assert!(t.is_counted(), "a late worker is counted");
    // Ours is live and counted, so the shared count is at least one whatever other tests'
    // workers do in the meantime.
    assert!(abandoned_workers() >= 1, "and the shared count saw it");
    w.worker_finished();
    assert!(!t.is_counted(), "and released on finish");
}

/// A worker that outlives its budget is counted while it runs and uncounted when it
/// finishes, through the real `spawn_budgeted` path. Only relative facts are asserted
/// (other tests in this binary may run budgeted workers concurrently).
#[test]
fn spawn_budgeted_counts_a_worker_that_outlives_its_budget() {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let r = spawn_budgeted(
        "st2k-test-abandoned",
        Duration::from_millis(20),
        move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
            7u8
        },
    );
    assert_eq!(r, None, "a blocked worker must time out");
    assert!(
        abandoned_workers() >= 1,
        "our still-blocked worker must be counted as abandoned"
    );
    let _ = release_tx.send(());
    assert!(
        done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "the abandoned worker must still run to completion on its own"
    );
}

/// A worker that finishes inside its budget hands back its result.
#[test]
fn spawn_budgeted_returns_a_prompt_result() {
    let r = spawn_budgeted("st2k-test-prompt", Duration::from_secs(10), || 42u32);
    assert_eq!(r, Some(42));
}

static POOL: LeasePool<2> = LeasePool::new(1_000);

/// The slots must be a LEASE, not a permanent claim. Two files whose reads hang forever
/// used to hold both property-probe slots for the life of the process, after which every
/// property query in that host returned nothing. Driven with an injected clock, so it
/// asserts the real policy without sleeping or spawning a thread.
#[test]
fn hung_holders_lose_their_slot_when_the_lease_expires() {
    let t0 = 1_000_000u64;
    let lease_ms = POOL.lease_ms;

    // Fill every slot, then confirm the cap actually holds.
    let first: Vec<Lease> = (0..2).map(|_| POOL.acquire_at(t0).expect("slot")).collect();
    assert!(
        POOL.acquire_at(t0).is_none(),
        "the cap must bound live holders"
    );

    // Still held part-way through the lease: a slow-but-progressing read keeps its slot.
    assert!(POOL.acquire_at(t0 + lease_ms - 1).is_none());

    // Past the lease, the slots are reclaimable even though the holders never finished.
    let second: Vec<Lease> = (0..2)
        .map(|_| {
            POOL.acquire_at(t0 + lease_ms + 1)
                .expect("an expired lease must be reclaimable")
        })
        .collect();

    // A late release from the FIRST generation must not free the slot its successor now
    // owns: the drop is keyed to the exact expiry it claimed.
    let held: Vec<u64> = POOL
        .slots
        .iter()
        .map(|s| s.load(Ordering::Acquire))
        .collect();
    drop(first);
    let after: Vec<u64> = POOL
        .slots
        .iter()
        .map(|s| s.load(Ordering::Acquire))
        .collect();
    assert_eq!(
        held, after,
        "a stale release must not steal the current holder's slot"
    );

    // A holder that finishes normally frees its slot immediately.
    drop(second);
    assert!(
        POOL.slots.iter().all(|s| s.load(Ordering::Acquire) == 0),
        "released slots must read as free"
    );
}
