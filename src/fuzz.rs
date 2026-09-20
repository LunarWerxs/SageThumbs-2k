//! Structure-aware mutation fuzzing of the pure-Rust binary parsers, run as ordinary
//! `cargo test` regression tests.
//!
//! Motivation, concretely: the MKV codec work shipped a real defect — an 8-byte EBML size
//! vint made `0xFFu8 >> 8` overflow, which panicked in debug and silently mis-parsed in
//! release. Every hand-written unit test passed; nothing fed the parser that one byte shape.
//! The class (a shift/index/arithmetic that a crafted or corrupt header pushes out of range)
//! is exactly what a mutation fuzzer finds cheaply and a fixed example suite does not.
//!
//! What this does: takes small VALID seeds (synthetic ones built here, plus real files from
//! the dev `test-corpus/` when present), applies deterministic structure-aware mutations
//! (bit flips, length-field blowups, truncations, region zeroing/filling), and runs each
//! through every pure-Rust parser entry point inside `catch_unwind`. A debug build has
//! overflow-checks and bounds-check panics ON, so "no panic across N mutations" is a real
//! assertion about the whole overflow/slice class, not just the inputs someone thought to
//! write down. Any panic is reported with the target, seed, and the mutated bytes' head so
//! it reproduces.
//!
//! Deliberately NOT fuzzed here: the tiers that shell out (ImageMagick) or call the OS
//! (Media Foundation, WinRT PDF/OCR) — those aren't ours to harden, and ImageMagick's
//! ~20 s kill-timeout alone would turn a run into hours. This targets the code WE parse
//! untrusted headers with. The corpus pass (dev-only) additionally drives
//! `decode::decode_menu_preview`, the same cascade minus those out-of-process tiers, so our
//! native JXL/DDS/container decoders get mutated real files too.
//!
//! Determinism: a fixed-seed xorshift PRNG (no `rand`, and `Date`/`Instant`-free per repo
//! rules), so a failure is always reproducible and CI is stable.

#![cfg(test)]

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
mod targets;
use targets::*;
mod seeds;
use seeds::*;
mod engine;
use engine::*;
mod surfaces;
use surfaces::*;
// The corpus session is self-contained (its tests live in the child), so nothing is imported
// from it here.
mod corpus;

/// The always-on, self-contained fuzz pass: synthetic seeds + random buffers, no external
/// files. Runs in CI and on every `cargo test`.
fn synthetic_seed_set() -> Vec<(&'static str, Vec<u8>)> {
    let mut seeds: Vec<(&'static str, Vec<u8>)> = vec![
        ("mkv", synthetic_mkv()),
        ("mp4", synthetic_mp4()),
        ("flv", synthetic_flv()),
        ("mkv-largesize-bomb", synthetic_mkv_largesize_bomb()),
    ];
    // One structurally VALID file per container format. Without these the only container input
    // this gate ever saw was a handful-of-bytes magic stub, so a mutation had essentially
    // nothing to corrupt — and on CI, which has no `test-corpus` to fall back on, that was the
    // whole of the coverage. See `container::fuzzseed`.
    seeds.extend(crate::container::fuzzseed::seeds());
    seeds.extend(new_surface_seeds());
    for (i, stub) in header_stubs().into_iter().enumerate() {
        seeds.push((Box::leak(format!("stub{i}").into_boxed_str()), stub));
    }
    // A few pure-random buffers of assorted sizes — covers the shallow reject paths.
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);
    for (i, &sz) in [0usize, 1, 2, 3, 4, 8, 16, 64, 200, 999].iter().enumerate() {
        let buf: Vec<u8> = (0..sz).map(|_| rng.byte()).collect();
        seeds.push((Box::leak(format!("rand{i}_{sz}").into_boxed_str()), buf));
    }
    seeds
}

/// The always-on, self-contained fuzz pass: synthetic seeds + random buffers, no external
/// files. Runs in CI and on every `cargo test`.
#[test]
fn parsers_survive_mutation_of_synthetic_seeds() {
    let seeds = synthetic_seed_set();

    // 3000 mutations per (seed, target) was right when there were 16 targets and 27 mostly-tiny
    // seeds. With the container seeds and their parsers added the matrix is ~5x bigger, and at
    // 3000 this gate cost 22.6 s of every `cargo test`; 1000 held it near 10 s. The 2.0 parsers
    // (VP9 container walk, Flash tag walk, the APK/H.264 sub-parsers) grew it again — 8 seeds
    // and 7 targets, which put 1000 back up at 16.2 s — so the count drops again to hold the
    // budget. Breadth is what this gate is FOR; depth moved to
    // `deep_session_over_the_new_parsers`, which runs the same targets for minutes rather than
    // milliseconds. What must NOT be traded away for time is the exhaustive truncation sweep:
    // short-read and off-by-one panics are what a prefix walk finds, and it is deterministic
    // rather than sampled.
    //
    // ⚠ RE-MEASURED 2026-08-20, AND THE 2026-08-19 NOTE HERE WAS WRONG IN A WAY WORTH
    // KEEPING. It said this gate costs ~255 s. That number was real but it was a DEBUG-build
    // reading quoted as if it described the gate, and debug fuzzing is ~7x slower than
    // release: the same gate is 36 s with --release. Quote the profile or the number means
    // nothing.
    //
    // Where the time actually goes (release, 78 targets, `fuzz::where_the_gate_spends_its_time`):
    //   container::fuzzseed   26 seeds   15.4 s
    //   new-surface           16 seeds   15.5 s
    //   header-stubs          16 seeds    1.1 s
    //   base-containers        4 seeds    0.3 s
    // and the two sibling always-on tests are 0.04 s between them. So it is two seed families,
    // and within them the TRUNCATION SWEEP rather than mutation: the sweep is O(seed length)
    // per (seed, target) pair while mutation is a fixed count, which put it at ~76% of the run.
    //
    // The split is therefore in the sweep DEPTH, not in the seed list: every seed and every
    // target still runs here, so nothing stops being covered on a push. See TRUNC_ALWAYS_ON.
    // Dropping whole seed families instead was the obvious move and the wrong one, because the
    // two expensive families are exactly the ones added to close a real CI coverage hole.
    let failures = run_all(
        &seeds,
        FUZZ_ITERS_ALWAYS_ON,
        0x5A5A_1234_9E37_79B9,
        TRUNC_ALWAYS_ON,
    );
    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by mutation fuzzing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// **The full-depth half of [`parsers_survive_mutation_of_synthetic_seeds`], moved off every
/// `cargo test`.**
///
/// Same seeds and same targets as the always-on gate, but with the deep truncation depth and
/// several times the mutations. Nothing was deleted when the gate was split on 2026-08-20: the
/// work lives here, and here it walks FURTHER than the single shared depth ever did.
///
/// ```text
/// cargo test --release --lib fuzz::full_depth -- --ignored --nocapture
/// ```
///
/// Run it before a release, after touching any parser, and after adding a seed or a target.
/// `--release` matters: debug fuzzing is ~7x slower and this is the expensive one.
#[test]
#[ignore = "full-depth synthetic sweep (minutes); run with --ignored"]
fn full_depth_sweep_over_the_synthetic_seeds() {
    let seeds = synthetic_seed_set();
    eprintln!(
        "full-depth sweep: {} seeds x {} targets, truncation to {TRUNC_DEEP}, {FUZZ_ITERS_DEEP} mutations per pair",
        seeds.len(),
        all_targets().len()
    );
    let failures = run_all(&seeds, FUZZ_ITERS_DEEP, 0x5A5A_1234_9E37_79B9, TRUNC_DEEP);
    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by the full-depth sweep:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Where the always-on gate's ~255 s actually goes, by seed family and by target.
///
/// ```text
/// cargo test --release --lib fuzz::where_the_gate_spends_its_time -- --ignored --nocapture
/// ```
///
/// Written before splitting the gate, because "move the slow half out" is only meaningful once
/// you know which half is slow. Guessing here is easy and wrong: the obvious suspect is the big
/// seeds, but the truncation sweep is already capped at `TRUNC_EXHAUSTIVE`, so a 8 KB seed costs
/// barely more than a 2 KB one.
#[test]
#[ignore = "cost report; run with --ignored"]
fn where_the_gate_spends_its_time() {
    use std::time::Instant;

    /// A named group of seeds, so the cost report can attribute time to one.
    type SeedFamily = (&'static str, Vec<(&'static str, Vec<u8>)>);

    let families: Vec<SeedFamily> = vec![
        (
            "base-containers",
            vec![
                ("mkv", synthetic_mkv()),
                ("mp4", synthetic_mp4()),
                ("flv", synthetic_flv()),
                ("mkv-largesize-bomb", synthetic_mkv_largesize_bomb()),
            ],
        ),
        (
            "container::fuzzseed",
            crate::container::fuzzseed::seeds().into_iter().collect(),
        ),
        ("new-surface", new_surface_seeds()),
        (
            "header-stubs",
            header_stubs()
                .into_iter()
                .enumerate()
                .map(|(i, s)| (Box::leak(format!("stub{i}").into_boxed_str()) as &str, s))
                .collect(),
        ),
    ];

    let targets = all_targets();
    println!("  targets: {}", targets.len());
    let mut total = 0f64;
    for (name, seeds) in &families {
        let bytes: usize = seeds.iter().map(|(_, s)| s.len()).sum();
        let t = Instant::now();
        let failures = run_all(
            seeds,
            FUZZ_ITERS_ALWAYS_ON,
            0x5A5A_1234_9E37_79B9,
            TRUNC_ALWAYS_ON,
        );
        let secs = t.elapsed().as_secs_f64();
        total += secs;
        println!(
            "  {name:<22} {:>3} seeds, {:>7} bytes -> {secs:>7.1} s   ({} failures)",
            seeds.len(),
            bytes,
            failures.len()
        );
    }
    println!("  {:<22} {total:>26.1} s", "TOTAL (families)");

    // Per-target, using one mid-sized seed, to see whether any single parser dominates.
    let probe = synthetic_mp4();
    let mut per: Vec<(f64, &str)> = targets
        .iter()
        .map(|&target| {
            let t = Instant::now();
            let mut rng = Rng::new(1);
            let _ = truncation_sweep(target, "probe", &probe, TRUNC_ALWAYS_ON);
            let _ = hammer(target, "probe", &probe, 600, &mut rng);
            (t.elapsed().as_secs_f64(), target.0)
        })
        .collect();
    per.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!("  --- slowest targets on one seed ---");
    for (secs, name) in per.iter().take(8) {
        println!("  {name:<40} {secs:>7.3} s");
    }
}
