# DjVu conformance dashboard

The conformance job compares native-resolution page renders with a pinned
DjVuLibre release and publishes both machine-readable results and a static
dashboard at <https://matyushkin.github.io/djvu-rs/dev/conformance/>.

The corpus and thresholds are versioned in `conformance/corpus.json`. Do not
add or remove a fixture only in the workflow: the manifest is the coverage
contract used to detect silently skipped pages. Accepted tolerances live in
`conformance/accepted_differences.json`. Classified differential-fuzz fixtures
under `fuzz/corpus-regressions/diff_fuzz/` are indexed into every published
summary.

## Reproduce locally

Install DjVuLibre so that `ddjvu` and `djvused` are on `PATH`, then run:

```sh
make conformance
```

This is the single entry point used by CI (`scripts/run_conformance.sh`). It
builds the harnesses, runs the manifest-driven render and semantic comparisons,
validates writer encode + `djvu_mut` mutation coverage, and writes
`conformance_site/` (override with `CONFORMANCE_OUT_DIR=...`).

Open `conformance_site/index.html` to inspect the generated dashboard. The
report command exits non-zero when coverage is incomplete, a result is
duplicated or malformed, writer interop fails, or any metric exceeds the
manifest threshold.

For directly comparable historical results use the DjVuLibre identity shown
in `summary.json`. A local run with a different DjVuLibre release remains a
useful diagnostic, but it is not the pinned CI baseline. Pass a previous
`history.json` with `CONFORMANCE_HISTORY_INPUT=...` to populate the baseline
delta section.

## Artifact contract

`summary.json` is schema-versioned and records:

- Git commit and DjVuLibre identity;
- SHA-256 for every input plus a digest for the complete corpus definition;
- render policy and thresholds;
- every per-page render result, semantic-plane result, and validation failure;
- writer interop counts (checked / rejected / dimension mismatches);
- accepted-difference registry entries and diff-fuzz category counts;
- `baseline_delta` versus the previous history entry (status change, Δ mismatch,
  new/resolved failures, regression/improvement flags).

Semantic planes covered per document/page:

- page: `text`, `text_hierarchy`, `annotations`
- document: `bookmarks`, `metadata`, `dirm`

`history.json` retains the latest 100 published run summaries. CI restores the
previous published history before appending the current run. Missing or
malformed current results always fail; inability to download old history does
not make the current conformance result invalid.
