# Encoder parity scorecard

Issue #684 uses a reproducible scorecard instead of a single size claim. The
harness compares the same raster input through the two archival-safe public
profiles and DjVuLibre's command-line encoders:

| Input | DjVuLibre | djvu-rs profile | Fidelity gate |
|-------|-----------|-----------------|---------------|
| P6 PPM | `c44` | `PageEncoder` + `EncodeQuality::Photo` | decoded PSNR/SSIM and dimensions |
| P4 PBM | `cjb2` | `PageEncoder` + `EncodeQuality::Lossless` | pixel-exact decoded bitmap |

The scorecard records encoded bytes, median wall time, peak RSS, tool versions,
repository SHA, dimensions, and the decoded-quality result. The optional OCR
probe runs Tesseract over the source and both decoded raster artifacts and records
character/word counts; it is a readability smoke signal, not a substitute for
OCR ground truth.

For photo cases the scorecard also runs `c44` a second time with our slice
schedule (`-slice 10,20,…,100`, from `Iw44EncodeOptions::default()`) and
records it as `matched_baseline` with `size_ratio_ours_over_matched`. The
default `c44` schedule is `74,89,99`: one slice fewer than ours, so the plain
size ratio compares two different quality points.

Run it from the repository root:

```sh
cargo run --release --example encoder_parity_scorecard -- \
  --ocr --repeats 3 --output target/encoder-parity.json
```

Requirements: `ddjvu`, `c44`, and `cjb2` from DjVuLibre on `PATH`.
`tesseract` is optional; use `--no-ocr` to skip the probe. The default
`--max-pixels 20000000` bound records large pages as `skipped` rather than
turning a benchmark into an accidental memory stress test. Select a subset
with repeated `--case NAME`; the example prints the available case names with
`--help`.

## 2026-07-13 snapshot

Platform: macOS Darwin 25.5 / Apple Silicon arm64, Rust 1.92.0,
djvu-rs `94636e5`, DjVuLibre 3.5.29, three measured repetitions after one
warm-up. RSS is KiB; times are milliseconds. The full JSON artifact is ignored
under `target/` and can be regenerated with the command above.

| Case | Mode | DjVuLibre B | djvu-rs B | Size ratio | DjVuLibre ms | djvu-rs ms | DjVuLibre RSS | djvu-rs RSS | Quality |
|------|------|------------:|----------:|-----------:|--------------:|------------:|--------------:|------------:|---------|
| watchmaker | IW44 photo / `c44` | 665,625 | 692,182 | 1.040× | 481.4 | 269.3 | 145,712 | 161,600 | PSNR 38.73 dB, SSIM 0.9899 |
| goody two-shoes | IW44 photo / `c44` | 327,798 | 440,864 | 1.345× | 367.3 | 375.0 | 135,648 | 265,408 | PSNR 26.39 dB, SSIM 0.9142 |
| cable | JB2 lossless / `cjb2` | 2,248 | 4,720 | 2.100× | 27.2 | 16.6 | 17,088 | 7,824 | pixel-exact |
| map atlas | JB2 lossless / `cjb2` | 145,592 | 138,672 | 0.952× | 348.6 | 29.6 | 33,792 | 6,736 | pixel-exact |
| Chinese cookbook | JB2 lossless / `cjb2` | 67 | 140 | 2.090× | 23.1 | 14.1 | 15,104 | 6,320 | pixel-exact |

The default `big-scanned-page` case is `6780×9148` (62,023,440 pixels) and is
recorded as skipped under the 20M bound. Increase `--max-pixels` deliberately
when that page is the subject of a run.

For the text-heavy JB2 cases, the optional Tesseract 5.5.2 probe produced the
same counts for source, DjVuLibre, and djvu-rs: cable `245 chars / 42 words`,
map atlas `1,399 / 681`, and the selected Chinese page `0 / 0`.

## 2026-07-16 snapshot (post IW44 fixes)

Two IW44 encoder bugs the 2026-07-13 snapshot exposed were fixed:
`IW44_LUMA_PLATEAU` (activation threshold `|V| > 11s/16` → `|V| >= s`) and
`IW44_PIGEON_COLOR` (encoder `rgb_to_ycbcr` switched to DjVuLibre's Pigeon
basis). Same platform/DjVuLibre version; decoded PSNR/SSIM vs source.

| Case | Mode | DjVuLibre B | djvu-rs B | Size ratio | djvu-rs PSNR / c44 | SSIM (ours) |
|------|------|------------:|----------:|-----------:|-------------------:|------------:|
| watchmaker | IW44 photo / `c44` | 665,625 | 682,598 | 1.025× | 45.96 / 45.28 dB | 0.9950 |
| goody two-shoes | IW44 photo / `c44` | 327,798 | 340,872 | 1.040× | 41.14 / 40.83 dB | 0.9811 |
| cable | JB2 lossless / `cjb2` | 2,248 | 4,720 | 2.100× | pixel-exact | — |
| map atlas | JB2 lossless / `cjb2` | 145,592 | 138,672 | 0.952× | pixel-exact | — |
| Chinese cookbook | JB2 lossless / `cjb2` | 67 | 140 | 2.090× | pixel-exact | — |

IW44 photo is now 1.025–1.040× `c44` at **matched-or-better fidelity** — decoded
PSNR and SSIM meet or exceed `c44` on both measured pages (previously up to
1.345× *and* far lower fidelity). The JB2 lossless path is unchanged by these
fixes.

## 2026-09-27 snapshot (dictionary JB2 for `Lossless`)

The bilevel `Lossless` profile used to write the page as direct tiles, with
no symbol dictionary. It now calls `encode_jb2_lossless`: a symbol dictionary
where a glyph that is close to an earlier one is coded as a refinement of it
(center-aligned records 4, ±2 px, 20 % Hamming budget). The page is also coded
as direct tiles, and the smaller stream wins — map atlas keeps its tiles.
Rust 1.98.0, djvu-rs `af7a850` plus this change, DjVuLibre 3.5.29, three
repetitions; `ddjvu` decodes every output pixel-exact.

| Case | Mode | DjVuLibre B | djvu-rs B | Size ratio | DjVuLibre ms | djvu-rs ms | DjVuLibre RSS | djvu-rs RSS | Quality |
|------|------|------------:|----------:|-----------:|--------------:|------------:|--------------:|------------:|---------|
| cable | JB2 lossless / `cjb2` | 2,248 | 2,272 | 1.011× | 24.1 | 21.9 | 17,104 | 15,136 | pixel-exact |
| map atlas | JB2 lossless / `cjb2` | 145,592 | 138,672 | 0.952× | 326.1 | 149.4 | 32,704 | 20,640 | pixel-exact |
| Chinese cookbook | JB2 lossless / `cjb2` | 67 | 66 | 0.985× | 20.8 | 18.0 | 15,152 | 13,344 | pixel-exact |

JB2 lossless is now **0.952–1.011×** `cjb2` (was 0.952–2.100×) and still
faster than `cjb2`. The cost is encode time on pages where tiles win: map
atlas 29.6 → 149.4 ms, because both encodings run.

## 2026-10-03 snapshot

Rerun on djvu-rs `3e64096`, Rust 1.98, DjVuLibre 3.5.29, Apple M1 Max, three
repetitions, `--no-ocr`. Sizes are unchanged from the snapshots above; djvu-rs
is faster than DjVuLibre on every case.

| Case | Mode | DjVuLibre B | djvu-rs B | Size ratio | DjVuLibre ms | djvu-rs ms | DjVuLibre RSS | djvu-rs RSS | Quality |
|------|------|------------:|----------:|-----------:|--------------:|------------:|--------------:|------------:|---------|
| watchmaker | IW44 photo / `c44` | 665,625 | 682,598 | 1.025× | 627.9 | 283.1 | 145,696 | 141,456 | PSNR 45.96 / 45.28 dB |
| goody two-shoes | IW44 photo / `c44` | 327,798 | 340,872 | 1.040× | 503.4 | 405.8 | 135,664 | 183,136 | PSNR 41.14 / 40.83 dB |
| cable | JB2 lossless / `cjb2` | 2,248 | 2,272 | 1.011× | 28.4 | 26.0 | 17,168 | 16,288 | pixel-exact |
| map atlas | JB2 lossless / `cjb2` | 145,592 | 138,672 | 0.952× | 358.7 | 161.5 | 35,360 | 22,368 | pixel-exact |
| Chinese cookbook | JB2 lossless / `cjb2` | 67 | 66 | 0.985× | 24.0 | 21.7 | 15,168 | 14,528 | pixel-exact |

## 2026-10-05 snapshot (equal slice count)

djvu-rs `b7481f3` plus the matched baseline, Rust 1.98, DjVuLibre 3.5.29,
Apple M1 Max, three repetitions, `--no-ocr`. "Equal slices" is `c44` with
our schedule; PSNR is decoded RGB against the source.

| Case | Mode | c44 default B | c44 equal slices B | djvu-rs B | Ratio (default) | Ratio (equal slices) | PSNR c44 equal / ours | DjVuLibre ms | djvu-rs ms |
|------|------|-------------:|-------------------:|----------:|----------------:|---------------------:|----------------------:|-------------:|-----------:|
| watchmaker | IW44 photo | 665,625 | 684,544 | 682,598 | 1.025× | **0.997×** | 46.00 / 45.96 dB | 493.7 | 252.4 |
| goody two-shoes | IW44 photo | 327,798 | 340,926 | 340,872 | 1.040× | **1.000×** | 41.15 / 41.14 dB | 398.2 | 311.9 |
| cable | JB2 lossless / `cjb2` | 2,248 | — | 2,272 | 1.011× | — | pixel-exact | 26.4 | 7.4 |
| map atlas | JB2 lossless / `cjb2` | 145,592 | — | 138,672 | 0.952× | — | pixel-exact | 353.7 | 121.6 |
| Chinese cookbook | JB2 lossless / `cjb2` | 67 | — | 66 | 0.985× | — | pixel-exact | 21.6 | 4.4 |

At an equal slice count the IW44 sizes and PSNR are at parity. The
1.025–1.040× ratio against default `c44` is the cost of our 100th slice
(+0.2–0.7 dB), not a coding gap. A nine-page check at 99 slices on both
sides agrees (`IW44_SIZE_PARITY` in `PERF_EXPERIMENTS.md`).

## Decision boundary

The scorecard is the measurement harness; the two IW44 fixes above were promoted
to the default bitstream only after this scorecard, byte-exact `ddjvu` interop,
and the full test suite confirmed them (recorded Kept in `PERF_EXPERIMENTS.md`).
IW44 photo now sits at 1.025–1.040× default `c44` at higher fidelity, and at
0.997–1.000× `c44` at an equal slice count; the
JB2 lossless profile, after the 2026-09-27 dictionary switch, sits at
0.952–1.011× `cjb2`.

Same-size JB2 record-6 and lossy rec-7 remain explicit experimental options;
their real-byte, round-trip, and OCR evidence stays in `PERF_EXPERIMENTS.md`.
The IW44 forward-transform hypothesis is rejected there after
coefficient-identical production-vs-DjVuLibre DWT measurements (the gap was in
the coefficient coding and colour transform, not the DWT). No further candidate
is promoted to the default archival/lossless path by the scorecard alone.
