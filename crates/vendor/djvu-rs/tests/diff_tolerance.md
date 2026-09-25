# Differential testing tolerances vs DjVuLibre

This document records the per-codec tolerance thresholds used by the
`diff_djvulibre` harness (`examples/diff_djvulibre.rs`) and the CI
workflow `.github/workflows/diff.yml`. The versioned corpus manifest and
machine-enforced coverage contract live in `conformance/corpus.json`; the
public history is published at
https://matyushkin.github.io/djvu-rs/dev/conformance/. Tracked under #192 and
#682. See `docs/conformance.md` for the complete local reproduction and
artifact contract.

## Harness

```
cargo run --release --features cli --example diff_djvulibre -- \
    [--width N] [--tolerance T] [--max-pages M] <file.djvu> [...]
```

For each page, both `djvu_rs` and `ddjvu` are asked to render at the
same target size; the resulting RGBA / RGB pixmaps are compared per
pixel. A pixel is "mismatched" if `max(|Δr|, |Δg|, |Δb|) > tolerance`.

## Why tolerance > 0 is acceptable

A small per-channel difference is **not** a decoder bug:

* **IW44**: lossy wavelet, both implementations carry intermediate
  rounding errors that diverge by ±1 LSB.
* **Resampling**: when target render size < native page size, both
  pipelines apply a downsampling filter. They are not identical
  (different kernels, different colour-space conversions). At small
  widths this dominates the diff.
* **YCbCr ↔ RGB**: the BT.601 / BT.709 matrices and rounding bias
  differ subtly between implementations.

Differences > a few LSB or > 5% of pixels are **decoder bugs** and
should be filed as separate issues.

## Per-codec ceilings (`PAGE_CEILING_PCT`, `MEAN_DELTA_CEILING`)

The CI gate enforces global ceilings; per-codec breakdown is
informational for triage.

| Codec / page kind | Tolerance | Page mismatch% | Mean abs Δ |
|-------------------|-----------|----------------|------------|
| **JB2 only (bilevel)**         | 0 | < 0.5% | < 0.5 |
| **IW44 only (photo)**          | 4 | < 5%   | < 1.5 |
| **Mixed JB2 + IW44 (scan)**    | 4 | < 5%   | < 1.5 |
| **Native-resolution render**   | 4 | < 0.5% | < 0.2 |
| **Downsampled render < 600px** | 8 | < 14%  | < 3.0 |

Empirical results, `--width 600 --tolerance 4`:

| File | Codec | Mismatch% | Max Δ | Mean Δ |
|------|-------|-----------|-------|--------|
| `boy.djvu`           | JB2+IW44 | 0.000% | 0  | 0.00 |
| `chicken.djvu`       | JB2+IW44 | 0.000% | 0  | 0.00 |
| `colorbook.djvu`     | IW44     | < 1%   | 22 | 0.20 |
| `watchmaker.djvu` †  | IW44     | 0.03%  | 22 | 0.05 |

† `watchmaker.djvu` not in fixtures; see `references/djvujs/library/assets/`.

## CI ceilings (currently enforced)

`.github/workflows/diff.yml` runs weekly + on dispatch and fails if any
page in the *bit-perfect-baseline* corpus exceeds:

* `PAGE_CEILING_PCT = 0.8`
* `MEAN_DELTA_CEILING = 0.2`

The CI corpus is the subset of `tests/fixtures/*.djvu` that is
bit-perfect or comfortably within the strict native-resolution ceilings today
(see the empirical table above). Any future regression above those ceilings
fails the gate.

The gate is fail-closed: every document/page declared in the manifest must
produce exactly one result. A missing fixture, missing `ddjvu`, parse/render
failure, malformed JSONL record, duplicate record, or silently skipped page
fails report generation. Each published `summary.json` records the Git commit,
pinned DjVuLibre identity, input SHA-256 digests, corpus digest, render policy,
and schema version so historical results remain attributable. The same gate
compares normalized text, annotation map-area counts, and bookmark trees with
`djvused`; a missing or divergent semantic plane also fails closed.

## Known divergences (excluded from CI gate, tracked separately)

The pinned Ubuntu DjVuLibre 3.5.28 baseline reports `colorbook.djvu` page 0 at
`0.7488%` mismatch with tolerance 4. The global ceiling is therefore `0.8%`;
the mean-delta ceiling remains the tighter `0.2` guard. The conformance
artifact records the exact DjVuLibre package identity so this baseline cannot
silently drift between tool builds.

Resolved under **#279**:

* `colorbook.djvu` page 0 — after centre-aligned BG sampling, integer FG/BG
  colour-cell pitch, and nearest-cell FG44 lookup, the native diff is within the
  strict gate. Reproducer:
  `cargo run --release --features cli --example diff_djvulibre -- --width 99999 --tolerance 4 --max-pages 1 tests/fixtures/colorbook.djvu`.
  The earlier local reference build measured `mismatch_pct = 0.2673%`, while
  the pinned Ubuntu DjVuLibre 3.5.28 package used by CI measures `0.7488%`.
  Both remain far below the pre-fix `3.4482%` / mean Δ `0.659`; the published
  dashboard keeps tool identities and results separate rather than flattening
  them into one baseline.
  Stage checks showed the JB2 mask matches ddjvu exactly and raw BG44 at
  754×1223 matches ddjvu exactly; the fixed drift was native-page
  compositing/upscaling, especially FG44 pixels. Rejected hypotheses:
  endpoint-only plane mapping improved only to 2.07%; old nearest-neighbour
  FG44 mapping without centre/integer-cell alignment regressed to 10.93%;
  ad-hoc per-layer subpixel offsets improved to ~1.60% but were not kept because
  they are not a clean-room format rule.

* `navm_fgbz.djvu` pages 3 + 4 — after the #279 sampling change they measure
  `0.0010%` / `0.0271%` mismatch at width 2550 with mean Δ `0.001` / `0.003`,
  within the strict native gate.

## Filing divergences

If the CI gate fails or local development surfaces a page with
`mismatch_pct > 5%` at native resolution:

1. Save the failing JSON line from `diff_results.jsonl`.
2. Reproduce locally with the same tolerance + width.
3. Attempt to localise the codec: re-run with the same args but a
   single `tests/fixtures/<file>.djvu` to isolate.
4. File a new issue under the `bug` label, link to #192, attach:
   - Source `.djvu`
   - djvu-rs render PNG
   - ddjvu render PPM
   - Diff visualisation (`magick compare`)
