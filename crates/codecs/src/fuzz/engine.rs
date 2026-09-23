#![cfg(test)]

//! The mutation engine: the PRNG, the mutators, the truncation sweep, and the budgets the always-on gate runs under.

use super::*;

/// xorshift64* — deterministic, dependency-free, seeded per run.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Rng(seed | 1) // never zero (xorshift's fixed point)
    }
    pub(super) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub(super) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    pub(super) fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
}

/// Ceiling on a mutant's size. Stacked mutations compound: the duplicate-a-slice case can
/// double the buffer, so eight of them could grow a seed 256×, and the run would spend its
/// budget memcpying instead of parsing.
pub(super) const MAX_MUTANT: usize = 2 * 1024 * 1024;

/// Apply `stack` mutations in sequence, optionally grafting in a run of a DIFFERENT seed
/// first. The always-on gate uses `stack = 1`; the deep session varies it.
///
/// Why both knobs exist: one mutation from a valid seed only ever explores that seed's
/// immediate neighbourhood, which is the right trade for a gate that must stay under ten
/// seconds but leaves anything needing TWO coordinated corruptions (a length field AND the
/// data it measures) unreachable. The crossover is how a parser meets a chunk header it would
/// never have generated for itself — Android's AXML and its resources.arsc share a framing,
/// so a pool chunk grafted from one into the other is a realistic hostile shape.
pub(super) fn mutate_stacked(
    rng: &mut Rng,
    seed: &[u8],
    others: &[&[u8]],
    stack: usize,
) -> Vec<u8> {
    let mut b = seed.to_vec();
    if !others.is_empty() && !b.is_empty() && rng.below(4) == 0 {
        let src = others[rng.below(others.len())];
        if !src.is_empty() {
            let len = 1 + rng.below(src.len().min(64));
            let from = rng.below(src.len() - len + 1);
            let at = rng.below(b.len());
            b.splice(at..at, src[from..from + len].iter().copied());
        }
    }
    for _ in 0..stack.max(1) {
        b = mutate(rng, &b);
        if b.len() > MAX_MUTANT {
            b.truncate(MAX_MUTANT);
        }
    }
    b
}

/// Apply one random structure-aware mutation to `seed`. Kept localized (a few bytes, one
/// length field, one truncation) so mutated inputs stay close enough to valid to reach deep
/// code rather than bouncing off the magic check.
pub(super) fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    match rng.below(7) {
        _ if b.is_empty() => {
            b.push(rng.byte());
        }
        0 => {
            // 1..=8 single-bit flips.
            for _ in 0..=rng.below(8) {
                let i = rng.below(b.len());
                b[i] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            // Set a byte to a boundary value.
            let i = rng.below(b.len());
            b[i] = [0x00u8, 0xFF, 0x7F, 0x80, 0x01][rng.below(5)];
        }
        2 => {
            // Truncate to a random prefix (classic short-read panic finder).
            let n = rng.below(b.len());
            b.truncate(n);
        }
        3 => {
            // Blow up a would-be length field: fill 1/2/4/8 bytes with 0xFF.
            let width = [1usize, 2, 4, 8][rng.below(4)];
            let i = rng.below(b.len());
            for j in i..(i + width).min(b.len()) {
                b[j] = 0xFF;
            }
        }
        4 => {
            // Zero a region (a size going to 0, an id vanishing).
            let i = rng.below(b.len());
            let width = 1 + rng.below(8);
            for j in i..(i + width).min(b.len()) {
                b[j] = 0;
            }
        }
        5 => {
            // Duplicate a slice (grow without changing the head).
            let i = rng.below(b.len());
            let len = 1 + rng.below(b.len() - i);
            let chunk = b[i..i + len].to_vec();
            let at = rng.below(b.len());
            b.splice(at..at, chunk);
        }
        _ => {
            // Overwrite a run with random bytes.
            let i = rng.below(b.len());
            let len = 1 + rng.below(16);
            for j in i..(i + len).min(b.len()) {
                b[j] = rng.byte();
            }
        }
    }
    b
}

/// Run `iters` mutations of `seed` (named `label`) through `target`, capturing any panic.
/// Returns a human-readable failure string on the FIRST panic, else `None`.
pub(super) fn hammer(
    target: Target,
    label: &str,
    seed: &[u8],
    iters: usize,
    rng: &mut Rng,
) -> Option<String> {
    hammer_n(target, label, seed, &[], iters, 1, rng, PAIR_BUDGET, &mut 0)
}

/// [`hammer`] with the deep session's extra knobs: `stack` mutations per iteration, `others`
/// to graft from, an explicit time `budget`, and a counter so a run can report how many
/// inputs it actually got through rather than how many it hoped to.
#[allow(
    clippy::too_many_arguments,
    reason = "one fuzz driver, all knobs explicit"
)]
pub(super) fn hammer_n(
    target: Target,
    label: &str,
    seed: &[u8],
    others: &[&[u8]],
    iters: usize,
    stack: usize,
    rng: &mut Rng,
    budget: std::time::Duration,
    done: &mut u64,
) -> Option<String> {
    let (name, f) = target;
    let deadline = std::time::Instant::now() + budget;
    for it in 0..iters {
        // Checked every 32 iterations: `Instant::now()` per iteration would itself be a
        // measurable share of the cost for the parsers that reject in nanoseconds.
        if it % 32 == 0 && std::time::Instant::now() > deadline {
            break;
        }
        let input = mutate_stacked(rng, seed, others, stack);
        *done += 1;
        let res = catch_unwind(AssertUnwindSafe(|| f(&input)));
        if let Err(e) = res {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".into());
            let head: String = input
                .iter()
                .take(48)
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            return Some(format!(
                "PANIC in {name} on seed '{label}' iter {it}: {msg} at {}\n  input[{}] head: {head}",
                last_panic_site(),
                input.len()
            ));
        }
    }
    None
}

/// Truncate the pristine seed and feed every prefix (no PRNG) — the single most productive
/// class for short-read / off-by-one panics.
///
/// Exhaustive over the first `trunc_exhaustive` bytes, which is where the headers, length
/// fields and index structures these parsers walk actually live, then strided over the rest.
/// Exhaustive everywhere is not affordable and buys nothing: a 96 KB seed would be 96,000
/// invocations for ONE (seed, target) pair, and the tail bytes are payload, not structure.
pub(super) fn truncation_sweep(
    target: Target,
    label: &str,
    seed: &[u8],
    trunc_exhaustive: usize,
) -> Option<String> {
    const TRUNC_STRIDE: usize = 257; // prime: never aligns with a power-of-two field width
    let (name, f) = target;
    let mut n = 0usize;
    while n <= seed.len() {
        let input = &seed[..n];
        if catch_unwind(AssertUnwindSafe(|| f(input))).is_err() {
            return Some(format!(
                "PANIC in {name} on seed '{label}' truncated to {n}/{} bytes at {}",
                seed.len(),
                last_panic_site()
            ));
        }
        n += if n < trunc_exhaustive {
            1
        } else {
            TRUNC_STRIDE
        };
    }
    None
}

thread_local! {
    /// `file:line` of the most recent caught panic on this thread, recorded by the quiet
    /// hook so a report can name the site; the payload alone ("index out of bounds") cannot.
    static LAST_PANIC_SITE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Where the last caught panic happened, as `file:line`, or `?` when the hook saw none.
pub(super) fn last_panic_site() -> String {
    LAST_PANIC_SITE.with(|c| c.borrow().clone().unwrap_or_else(|| "?".into()))
}

thread_local! {
    /// How many [`with_quiet_panics`] calls this thread is inside.
    static QUIET: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Silence panics on THIS thread for the duration of `body` so the thousands of intentionally
/// caught panics (in a failing run) don't flood stderr with backtraces. The quiet hook still
/// records each panic's location for [`last_panic_site`].
///
/// One hook, installed once, that is quiet only on a thread inside this call and passes every
/// other panic to the hook it replaced. It used to swap the process-wide hook in and out per
/// call, and two fuzz tests running at once (the default under `cargo test`) restored each
/// other's quiet hook: the process stayed silent, and a failing session's own report - which
/// names the parser, the seed and the input - printed nothing (Dredd, 2026-09-23).
pub(super) fn with_quiet_panics<T>(body: impl FnOnce() -> T) -> T {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if QUIET.with(std::cell::Cell::get) == 0 {
                return prev(info);
            }
            let site = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()));
            LAST_PANIC_SITE.with(|c| *c.borrow_mut() = site);
        }));
    });
    struct Loud;
    impl Drop for Loud {
        fn drop(&mut self) {
            QUIET.with(|q| q.set(q.get() - 1));
        }
    }
    QUIET.with(|q| q.set(q.get() + 1));
    let _loud = Loud;
    body()
}

/// How deep the truncation sweep walks EVERY prefix before switching to a stride.
///
/// Two values, because this is the whole cost of the gate and the two runs want different
/// answers. Measured 2026-08-20 over the 78 targets: the always-on gate spends ~76% of its
/// time here, not on mutation, because the sweep is O(seed length) per (seed, target) pair
/// while mutation is a fixed count.
///
/// The always-on value keeps the property this sweep exists for. Short-read and off-by-one
/// panics live in HEADER parsing - magic, chunk lengths, offset tables - which is the first
/// few hundred bytes; past that a prefix walk is mostly re-testing the same "payload ran out"
/// branch. The deep session then walks further than the single old value ever did, so nothing
/// is lost overall, only moved off every `cargo test`.
pub(super) const TRUNC_ALWAYS_ON: usize = 256;

/// The deep session's depth. HIGHER than the 2048 both runs used to share.
pub(super) const TRUNC_DEEP: usize = 4096;

/// Mutations per (seed, target) on every `cargo test`, and in the opt-in full-depth pass.
///
/// The always-on number buys breadth: every seed still meets every target. Depth is what
/// moved out, and the deep figure is five times what the gate used to do rather than equal
/// to it, so the split adds coverage overall instead of trading it away.
pub(super) const FUZZ_ITERS_ALWAYS_ON: usize = 200;

pub(super) const FUZZ_ITERS_DEEP: usize = 3000;

/// Per-target wall-clock budget for one (seed, target) pair. Mutation fuzzing is only
/// useful if it stays fast enough to run on every `cargo test`, and a few targets can be
/// pushed into genuinely expensive work by a mutation (an archive parser handed a plausible
/// central directory, WIC on a mutated raster). Stop early rather than let one pair dominate
/// the run; the iteration count is a target, not a contract.
pub(super) const PAIR_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);

pub(super) fn run_all(
    seeds: &[(&str, Vec<u8>)],
    iters_per: usize,
    base_seed: u64,
    trunc_exhaustive: usize,
) -> Vec<String> {
    let targets = all_targets();
    let mut failures = Vec::new();
    with_quiet_panics(|| {
        for (si, (label, seed)) in seeds.iter().enumerate() {
            for (ti, &target) in targets.iter().enumerate() {
                // Distinct stream per (seed,target) so a fix to one doesn't shift others.
                let mut rng = Rng::new(
                    base_seed ^ ((si as u64) << 32) ^ (ti as u64).wrapping_mul(0x9E37_79B9),
                );
                if let Some(f) = truncation_sweep(target, label, seed, trunc_exhaustive) {
                    failures.push(f);
                }
                if let Some(f) = hammer(target, label, seed, iters_per, &mut rng) {
                    failures.push(f);
                }
            }
        }
    });
    failures
}
