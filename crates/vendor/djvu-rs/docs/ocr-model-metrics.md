# OCR model metrics — recorded baseline (#693)

Baseline quality of the pinned neural OCR models on the synthetic corpus
(`src/ocr_onnx/corpus.rs`). The corpus is project-owned and deterministic:
pages are rendered with the pinned PT Sans font (manifest key
`metrics-font-pt-sans`) from versioned layout parameters, so no images are
committed and no third-party benchmark license applies.

**Policy.** A model bump (any change to `docs/ocr-model-manifest.toml`
affecting the pipeline), a font bump, or a preprocessing change must
re-measure and update this table in the same PR. The corpus test
(`ocr_onnx::corpus::tests::corpus_baseline_holds_for_pinned_models`) asserts
thresholds = baseline plus a safety margin, so normal cross-platform float
jitter passes while a silent regression fails. CI runs it in the main-only
`OCR (onnx models)` job; locally it skips unless models and font are fetched
(`scripts/fetch_ocr_models.sh`).

## Baseline

Pinned pipeline: `ppocr-v4-mobile-det` (detection) +
`ppocr-v5-cyrillic-rec` with its pinned dictionary (recognition),
tract-onnx 0.22.3.

Measured 2026-08-12 on macOS arm64 (debug profile; metrics are
platform-stable by design — fixed-point preprocessing, deterministic
rasterization):

| Sample | CER | WER | Mean line IoU | Threshold (CER / WER / IoU) |
|--------|-----|-----|---------------|------------------------------|
| Cyrillic (4 lines, 2 pangram excerpts) | 0.000 | 0.000 | 0.937 | ≤ 0.05 / ≤ 0.10 / ≥ 0.80 |
| Latin + digits (4 lines) | 0.000 | 0.000 | 0.827 | ≤ 0.05 / ≤ 0.10 / ≥ 0.70 |

Notes:

- CER/WER of exactly 0.0 means the clean synthetic pages are recognized
  verbatim, including «ё», punctuation, and digits. Real scans will be
  worse; the corpus gates *regressions of the pinned artifacts*, not
  real-world accuracy.
- IoU compares ink-tight ground-truth line boxes against DBNet's unclipped
  boxes; values well below 1.0 are expected (the unclip ratio pads the
  detector's shrunken text cores). The Latin sample scores lower because its
  ascender/descender profile pads differently, not because detection is
  worse.
