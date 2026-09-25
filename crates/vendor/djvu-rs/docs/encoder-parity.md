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

## Decision boundary

The scorecard is the measurement harness; the two IW44 fixes above were promoted
to the default bitstream only after this scorecard, byte-exact `ddjvu` interop,
and the full test suite confirmed them (recorded Kept in `PERF_EXPERIMENTS.md`).
IW44 photo now sits at 1.025–1.040× `c44` at matched-or-better fidelity; the
public direct JB2 lossless profile still ranges from 0.952× to 2.100×.

Same-size JB2 record-6 and lossy rec-7 remain explicit experimental options;
their real-byte, round-trip, and OCR evidence stays in `PERF_EXPERIMENTS.md`.
The IW44 forward-transform hypothesis is rejected there after
coefficient-identical production-vs-DjVuLibre DWT measurements (the gap was in
the coefficient coding and colour transform, not the DWT). No further candidate
is promoted to the default archival/lossless path by the scorecard alone.
