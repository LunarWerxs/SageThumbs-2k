#![cfg(test)]

//! The opt-in corpus session: pick real samples from the test corpus and mutate them through the same targets.

use super::*;

/// One candidate for [`select_corpus_seeds`]: enough to decide whether and when to pick it,
/// without ever reading its content. `root` is an opaque index into whichever list of root
/// directories the caller scanned, the selector doesn't know or care what a root MEANS, only
/// that seeds from different roots should mix rather than one root exhausting the budget
/// before a second is even consulted (2026-09-05 audit, F23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeedCandidate {
    pub(super) root: usize,
    pub(super) path: PathBuf,
    pub(super) size: u64,
}

impl SeedCandidate {
    pub(super) fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string()
    }

    pub(super) fn extension(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    }
}

/// True for a corpus entry that is the harness's own bookkeeping, not a sample to fuzz: its
/// manifests (anything underscore-prefixed: `_expected-fail.txt`, `_pixls-index.json`,
/// `_render-sanity-allow.txt`, ...), notes, and generated review output (`contact.png`). None
/// of these exercise a parser; before this fix they were eligible seeds competing with real
/// samples for the (single, global) budget (2026-09-05 audit, F23).
pub(super) fn is_bookkeeping_seed(c: &SeedCandidate) -> bool {
    let name = c.name();
    if name.starts_with('_') || name == "contact.png" {
        return true;
    }
    matches!(c.extension().as_str(), "txt" | "json" | "md" | "csv")
}

/// Picks up to `budget` seeds from `candidates`, stratified by root AND by format
/// (extension): every (root, extension) group present contributes one seed before any group
/// contributes a second, round-robin in a fixed sort order, as far as the budget allows: a
/// single root with at least `budget` distinct extensions can still take every seat.
///
/// This replaces a selector that filled the WHOLE 60-file budget from one sorted,
/// unstratified `test-corpus` listing (2026-09-05 audit, F23): with `test-corpus`'s 382 files
/// sorting first, `test-corpus-real`'s 229 files never contributed a single seed, and
/// bookkeeping files (excluded here by [`is_bookkeeping_seed`]) were eligible seeds that could
/// sort ahead of a format nothing else in the budget covered.
///
/// Deterministic: buckets and the candidates within them are sorted, never shuffled, so the
/// same candidate list always yields the same seed list regardless of scan order.
pub(super) fn select_corpus_seeds(
    candidates: &[SeedCandidate],
    budget: usize,
) -> Vec<&SeedCandidate> {
    let mut buckets: Vec<((usize, String), Vec<&SeedCandidate>)> = Vec::new();
    for c in candidates {
        if is_bookkeeping_seed(c) {
            continue;
        }
        let key = (c.root, c.extension());
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(c),
            None => buckets.push((key, vec![c])),
        }
    }
    buckets.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, v) in &mut buckets {
        v.sort_by_key(|a| a.name());
    }

    // Round-robin: one seed from each non-empty bucket per pass, in the fixed sort order
    // above, until the budget is met or every bucket is drained.
    let mut out = Vec::new();
    let mut cursor = vec![0usize; buckets.len()];
    while out.len() < budget {
        let mut took_any = false;
        for (bi, (_, v)) in buckets.iter().enumerate() {
            if out.len() >= budget {
                break;
            }
            if cursor[bi] < v.len() {
                out.push(v[cursor[bi]]);
                cursor[bi] += 1;
                took_any = true;
            }
        }
        if !took_any {
            break;
        }
    }
    out
}

/// Reads a seed's bytes for the corpus fuzz pass, refusing anything over `cap` instead of
/// reading it whole and truncating afterward: it asks the reader for at most `cap + 1` bytes,
/// so an oversize file is caught without the I/O cost of reading it to completion (2026-09-05
/// audit, F23. The old code did `std::fs::read(&p)` first and truncated the `Vec` after, so a
/// stray multi-hundred-MB file sitting in the corpus was read in full for nothing before the
/// cap even applied).
pub(super) fn read_seed_bounded<R: std::io::Read>(src: R, cap: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let limit = (cap as u64).saturating_add(1);
    src.take(limit).read_to_end(&mut buf).ok()?;
    (buf.len() <= cap).then_some(buf)
}

/// Deep pass over REAL files from the dev `test-corpus/` + `test-corpus-real/` (which live
/// outside the repo and are not committed). Mutates a stratified, size-bounded sample of each
/// and drives the header parsers plus the hermetic decode cascade.
///
/// `#[ignore]` because it is minutes, not seconds: real seeds are far larger and more varied
/// than the synthetic ones, which is exactly what makes it worth running — just not on every
/// inner-loop `cargo test`. Run it deliberately, with `--nocapture` to see the seed manifest
/// and the per-seed, per-target progress this run prints as it goes (2026-09-05 audit, F23:
/// a 20-minute run used to finish with no evidence of what it had actually covered):
///
/// ```text
/// cargo test --lib fuzz:: -- --ignored --nocapture
/// ```
///
/// The always-on [`parsers_survive_mutation_of_synthetic_seeds`] is the CI gate; this is the
/// deeper sweep for a dev machine that has the corpus. It skips cleanly if the corpus is absent.
/// Scans each root directory (non-recursively) for candidate seed files: enough metadata
/// (root index, path, size) for [`select_corpus_seeds`] to choose from without reading any
/// content. Split out of [`parsers_survive_mutation_of_corpus_samples`] to keep that test's
/// own complexity below the repo's per-function gate (2026-09-05 audit, F23).
pub(super) fn scan_corpus_candidates(roots: &[PathBuf]) -> Vec<SeedCandidate> {
    let mut candidates = Vec::new();
    for (root_idx, root) in roots.iter().enumerate() {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            candidates.push(SeedCandidate {
                root: root_idx,
                path,
                size,
            });
        }
    }
    candidates
}

/// Reads the bytes for each already-selected candidate, bounded by `cap` via
/// [`read_seed_bounded`], printing the seed manifest line for each as it goes so
/// `--nocapture` shows what will be fuzzed before the run itself starts. Split out of
/// [`parsers_survive_mutation_of_corpus_samples`] to keep that test's own complexity below
/// the repo's per-function gate (2026-09-05 audit, F23).
pub(super) fn load_selected_seeds(
    selected: &[&SeedCandidate],
    cap: usize,
) -> Vec<(String, Vec<u8>)> {
    let mut seeds = Vec::new();
    for c in selected {
        let name = c.name();
        let ext = c.extension();
        let Ok(file) = std::fs::File::open(&c.path) else {
            eprintln!(
                "  skip (unreadable)          root={} .{ext:<5} {name}",
                c.root
            );
            continue;
        };
        match read_seed_bounded(file, cap) {
            Some(bytes) => {
                eprintln!(
                    "  root={}  .{ext:<5}  {name:<40}  {} bytes",
                    c.root,
                    bytes.len()
                );
                seeds.push((name, bytes));
            }
            None => eprintln!(
                "  skip (over {cap}-byte cap)  root={} .{ext:<5} {name} ({} bytes on disk)",
                c.root, c.size
            ),
        }
    }
    seeds
}

/// Runs the truncation sweep plus mutation hammer for every (seed, target) pair, printing a
/// per-seed, per-target progress line with elapsed time. Split out of
/// [`parsers_survive_mutation_of_corpus_samples`] to keep that test's own complexity below the
/// repo's per-function gate (2026-09-05 audit, F23).
pub(super) fn fuzz_seeds_over_header_parsers(
    seeds: &[(String, Vec<u8>)],
    targets: &[Target],
    overall: &std::time::Instant,
) -> Vec<String> {
    let mut failures = Vec::new();
    with_quiet_panics(|| {
        for (si, (label, seed)) in seeds.iter().enumerate() {
            for (ti, &target) in targets.iter().enumerate() {
                let started = std::time::Instant::now();
                let mut rng = Rng::new(
                    0x00C0_FFEE_1234_5678
                        ^ ((si as u64) << 32)
                        ^ (ti as u64).wrapping_mul(0x9E37_79B9),
                );
                if let Some(f) = truncation_sweep(target, label, seed, TRUNC_DEEP) {
                    failures.push(f);
                }
                if let Some(f) = hammer(target, label, seed, 400, &mut rng) {
                    failures.push(f);
                }
                eprintln!(
                    "  [{:>6.1}s] {label:<28} x {:<34} {:>6.1} ms",
                    overall.elapsed().as_secs_f64(),
                    target.0,
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
        }
    });
    failures
}

/// Runs the hermetic decode cascade (`decode::decode_menu_preview`) over every seed, with the
/// same per-seed progress reporting. Split out of
/// [`parsers_survive_mutation_of_corpus_samples`] to keep that test's own complexity below the
/// repo's per-function gate (2026-09-05 audit, F23).
pub(super) fn fuzz_seeds_over_decode_cascade(
    seeds: &[(String, Vec<u8>)],
    overall: &std::time::Instant,
) -> Vec<String> {
    let preview: Target = ("decode::decode_menu_preview", |b| {
        let _ = crate::decode::decode_menu_preview(b);
    });
    let mut failures = Vec::new();
    with_quiet_panics(|| {
        let mut rng = Rng::new(0x1122_3344_5566_7788);
        for (label, seed) in seeds {
            let started = std::time::Instant::now();
            if let Some(f) = hammer(preview, label, seed, 120, &mut rng) {
                failures.push(f);
            }
            eprintln!(
                "  [{:>6.1}s] {label:<28} x {:<34} {:>6.1} ms",
                overall.elapsed().as_secs_f64(),
                preview.0,
                started.elapsed().as_secs_f64() * 1000.0,
            );
        }
    });
    failures
}

#[test]
#[ignore = "deep corpus fuzz (minutes); run with --ignored"]
pub(super) fn parsers_survive_mutation_of_corpus_samples() {
    let roots = [crate::testcorpus::dir(), crate::testcorpus::real_dir()];
    // Cap per-file bytes so a multi-MB RAW doesn't make the mutation loop crawl; the header
    // parsers only ever look near the start, and decode caps its own input anyway.
    const CAP: usize = 96 * 1024;
    const BUDGET: usize = 60;

    let candidates = scan_corpus_candidates(&roots);
    let selected = select_corpus_seeds(&candidates, BUDGET);
    if selected.is_empty() {
        eprintln!("corpus fuzz: no test-corpus present, skipping");
        return;
    }

    eprintln!(
        "corpus fuzz: seed manifest, {} of {} eligible candidates (budget {BUDGET}):",
        selected.len(),
        candidates.len(),
    );
    let seeds = load_selected_seeds(&selected, CAP);
    if seeds.is_empty() {
        eprintln!("corpus fuzz: every selected candidate was unreadable or over the cap, skipping");
        return;
    }

    // Header parsers on the real files (fewer iters, there are many seeds), with a per-seed,
    // per-target progress line and elapsed time so a run states its own coverage rather than
    // running silently for minutes. Same targets and deep truncation depth (TRUNC_DEEP) as
    // `run_all`, but its own 400-iteration budget and a distinct per-(seed,target) base seed
    // 0x00C0_FFEE_1234_5678.
    // This is deliberately its own loop (not a `run_all` change) so the always-on gate and
    // the full-depth sweep, which share `run_all`, are untouched.
    let targets = all_targets();
    let overall = std::time::Instant::now();
    let mut failures = fuzz_seeds_over_header_parsers(&seeds, &targets, &overall);

    // Plus the DECODE cascade, via `decode_menu_preview`, which is the same tier stack minus
    // the three that leave this process: the ImageMagick subprocess, Media Foundation, and the
    // WinRT PDF rasterizer. That keeps the fuzz hermetic and fast (magick alone has an 18-20 s
    // kill-timeout, so a full `decode_preview` fuzz would take hours and would be testing
    // someone else's parser) while still driving OUR pure-Rust JXL, DDS, container and image
    // tiers on mutated real files.
    failures.extend(fuzz_seeds_over_decode_cascade(&seeds, &overall));

    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by corpus mutation fuzzing:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "corpus fuzz: {} seed files, no panics, {:.1}s total",
        seeds.len(),
        overall.elapsed().as_secs_f64()
    );
}

/// Unit tests for [`select_corpus_seeds`] and [`read_seed_bounded`] over synthetic candidate
/// lists, the pure half of the F23 fix, checkable in milliseconds without a `test-corpus`
/// checkout. See `parsers_survive_mutation_of_corpus_samples` for the real-corpus pass these
/// back.
#[cfg(test)]
pub(super) mod corpus_seed_selection_tests {
    use super::*;

    fn cand(root: usize, name: &str, size: u64) -> SeedCandidate {
        SeedCandidate {
            root,
            path: PathBuf::from(name),
            size,
        }
    }

    #[test]
    fn both_roots_contribute_when_both_have_files() {
        let candidates = vec![
            cand(0, "a.png", 100),
            cand(0, "b.png", 100),
            cand(1, "c.jpg", 100),
        ];
        let picked = select_corpus_seeds(&candidates, 2);
        let roots: std::collections::BTreeSet<usize> = picked.iter().map(|c| c.root).collect();
        assert_eq!(
            roots,
            [0, 1].into_iter().collect(),
            "a 2-seed budget over two non-empty roots must draw from both, not fill from the \
             first root alone"
        );
    }

    #[test]
    fn bookkeeping_files_are_excluded() {
        let candidates = vec![
            cand(0, "_expected-fail.txt", 10),
            cand(0, "_pixls-index.json", 10),
            cand(0, "_render-sanity-allow.txt", 10),
            cand(0, "notes.txt", 10),
            cand(0, "readme.md", 10),
            cand(0, "data.csv", 10),
            cand(0, "contact.png", 10),
            cand(0, "sample.png", 10),
        ];
        let picked = select_corpus_seeds(&candidates, 60);
        assert_eq!(
            picked.iter().map(|c| c.name()).collect::<Vec<_>>(),
            vec!["sample.png"],
            "every bookkeeping name/extension must be excluded, leaving only the real sample"
        );
    }

    #[test]
    fn a_rare_extension_is_not_crowded_out_by_a_common_one() {
        let mut candidates = vec![cand(0, "only.mpg", 10)];
        for i in 0..50 {
            candidates.push(cand(0, &format!("many_{i:03}.png"), 10));
        }
        let picked = select_corpus_seeds(&candidates, 5);
        assert!(
            picked.iter().any(|c| c.extension() == "mpg"),
            "a single-file extension must get a seat within a small budget, not lose every \
             slot to the 50-file extension"
        );
    }

    #[test]
    fn the_budget_is_honoured_exactly() {
        let candidates: Vec<_> = (0..100)
            .map(|i| cand(0, &format!("f{i:03}.png"), 10))
            .collect();
        assert_eq!(select_corpus_seeds(&candidates, 60).len(), 60);
        assert_eq!(select_corpus_seeds(&candidates, 5).len(), 5);
        // Fewer eligible candidates than the budget: take them all, no padding.
        assert_eq!(select_corpus_seeds(&candidates[..3], 60).len(), 3);
    }

    #[test]
    fn selection_is_deterministic_regardless_of_scan_order() {
        let candidates = vec![
            cand(1, "z.jpg", 5),
            cand(0, "b.png", 5),
            cand(0, "a.png", 5),
            cand(1, "y.jpg", 5),
        ];
        let mut shuffled = candidates.clone();
        shuffled.reverse();
        let a: Vec<PathBuf> = select_corpus_seeds(&candidates, 10)
            .iter()
            .map(|c| c.path.clone())
            .collect();
        let b: Vec<PathBuf> = select_corpus_seeds(&shuffled, 10)
            .iter()
            .map(|c| c.path.clone())
            .collect();
        assert_eq!(
            a, b,
            "selection must not depend on the candidate list's input order"
        );
    }

    /// Records the largest cumulative number of bytes ever demanded from it, so a test can
    /// prove a bounded reader stopped asking rather than reading this (infinite) source to
    /// completion.
    struct Endless(std::rc::Rc<std::cell::Cell<u64>>);
    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.set(self.0.get() + buf.len() as u64);
            buf.fill(b'q');
            Ok(buf.len())
        }
    }

    #[test]
    fn an_oversize_seed_is_skipped_without_being_read_in_full() {
        let served = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let cap = 64usize;
        let result = read_seed_bounded(Endless(served.clone()), cap);
        assert!(
            result.is_none(),
            "a source with no end must be treated as over the cap, not read to EOF"
        );
        assert!(
            served.get() <= cap as u64 + 1,
            "must never demand more than cap+1 bytes from the reader, demanded {}",
            served.get()
        );
    }

    #[test]
    fn a_seed_exactly_at_the_cap_is_kept() {
        let at_cap = vec![b'x'; 64];
        let result = read_seed_bounded(&at_cap[..], 64);
        assert_eq!(result, Some(at_cap));
    }
}
