# Neural OCR pipeline — design (issue #693, slice 1)

Issue [#693](https://github.com/matyushkin/djvu-rs/issues/693) asks for **one
supported, redistributable neural OCR model family** with deterministic
preprocessing/decoding, hierarchical zone output mapped to the DjVu
`TextLayer`, and versioned corpus metrics. This document is the slice-1
deliverable: the model-family decision, the provisioning/verification policy,
the pipeline contract, and the implementation slice plan. No code changes
ship with this slice.

Out of scope (per the issue): arbitrary user-supplied ONNX graphs, CTC
training, and replacing the Tesseract backend.

## Decision: PaddleOCR PP-OCRv5 mobile (det + rec), via ONNX under tract

The supported family is **PaddleOCR PP-OCR mobile** — the DBNet text
*detector* plus the CRNN/SVTR-style CTC text *recognizer* — consumed as ONNX
exports and executed by the already-present `tract-onnx` dependency
(pure Rust, CPU-only, no C bindings, no Python).

| Criterion | PP-OCR mobile |
|---|---|
| Code license | Apache-2.0 (SPDX: `Apache-2.0`), PaddlePaddle/PaddleOCR |
| Weight license | Apache-2.0 — same repo, no separate weight terms |
| Model size | det ~5 MB, rec ~11 MB (mobile tier; server tier excluded) |
| Decoding | CTC → **greedy decode is deterministic** and already implemented in pure Rust in `src/ocr_onnx.rs` (index-0 blank, matching PP-OCR dictionaries) |
| Scripts | Latin + dedicated Apache-2.0 Cyrillic recognizer (`cyrillic_PP-OCRv5_mobile_rec`, ~80% strict line accuracy per upstream) |
| tract evidence | The Kreuzberg/xberg project runs DBNet + CRNN + AngleNet under tract in Rust with documented parity vs ONNX Runtime (probability maps agree to 5.0e-5) |

Runner-up, kept as the documented fallback: **docTR** (Mindee, Apache-2.0,
ONNX via OnnxTR). It natively emits *word*-level boxes (structurally closer
to the DjVu hierarchy) but has no verified Cyrillic recognizer and no known
tract compatibility precedent. Excluded outright: TrOCR (autoregressive
attention decoder; ONNX export documented as unreliable upstream; no CTC),
EasyOCR/CRAFT (weight-license ambiguity, no mature ONNX packaging),
Kraken/Calamari (handwriting-focused, no ONNX path).

### Known compatibility risk — must be spiked before slice 2

The tract precedent above was established on **tract 0.23.4**; this crate is
capped at **tract-onnx 0.22.x** (0.23 needs Rust 1.91 > MSRV 1.88, see the
`Cargo.toml` comment). The 0.22→0.23 changelog shows only LLM-oriented
operator additions — nothing touching Conv/BatchNorm/GlobalAveragePool — so
0.22.x is *expected* to run these graphs, but nobody has published proof.
**Slice 2 therefore starts with a spike test**: load both mobile models under
tract-onnx 0.22 and run one fixture page end-to-end before any API is built.

The one documented model-specific pitfall (found and solved by Kreuzberg):
DBNet's backbone contains squeeze-and-excitation blocks with
`GlobalAveragePool`, which pools over the *whole* input canvas — padding
every page to one fixed canvas shifts probability maps enough to lose whole
text lines. The adopted mitigation is the same as theirs: **resize to the
page's actual extent** (rounded to a multiple of 32) instead of one fixed
canvas, and cache tract plans keyed by input shape (small LRU, a handful of
resident plans). The recognizer is unaffected (fixed height, dynamic width).

## Pipeline contract

```
Pixmap (RGB render of the page)
  → deterministic preprocessing (resize to ×32 extent, normalize; fixed
    documented constants, integer-exact where possible)
  → DBNet detection → line-level boxes (polygons → axis-aligned rects)
  → per-line crop → height-normalized recognizer input
  → CRNN inference → CTC greedy decode against the pinned dictionary
  → reading-order sort (top-to-bottom, left-to-right within bands)
  → TextLayer zone tree
```

Determinism guarantee: identical page bytes + identical model bytes +
identical options ⇒ byte-identical `TextLayer` output. No randomness, no
threading-order dependence in decode (per-line inference is independent;
results are ordered by geometry, not completion order).

### Zone hierarchy mapping

`TextZoneKind` is `Page → Column → Region → Para → Line → Word → Character`.
PP-OCR detection is **line-level** (word-level detection is an open upstream
feature request), so the neural backend emits the coarser tree the
`OcrBackend::recognize` contract already permits:

- `Page → Line` from the detector, with recognized text per line.
- `Word` boxes from a documented heuristic: split the recognized line at
  whitespace and apportion the line box by CTC frame positions (frame→pixel
  mapping is linear in the recognizer's width scaling). Word rects are
  therefore *approximate* and documented as such.
- `Column`/`Region`/`Para` reconstruction (layout analysis) is a later
  slice; Tesseract remains the full-hierarchy backend.

## Provisioning, checksum, and version policy

Model weights are **never** committed to the repository and **never**
downloaded implicitly at build or run time. Provisioning is explicit:

- A manifest checked in at `docs/ocr-model-manifest.toml` lists, per
  artifact: canonical source URL, upstream revision (an immutable commit
  hash, never a branch name), byte size, **SHA-256 of the exact ONNX
  bytes**, ONNX opset, and SPDX license id.
- The loader API takes a filesystem path and verifies the SHA-256 against
  the manifest entry before first use; a mismatch is a hard, typed error
  (no "best effort" loading of unverified weights).
- A helper script (slice 2) downloads and verifies the manifest set for
  local development and CI. CI jobs that need weights are non-blocking for
  merges, same policy as the existing Tesseract OCR job.
- Bumping a model version = a PR that changes the manifest (new hash, new
  metrics — see below), reviewable like any other dependency bump.

Feature naming: the pipeline lands behind the existing `ocr-onnx` feature
(tract is already its dependency); `src/ocr_onnx.rs`'s generic CTC helper is
superseded by the PP-OCR-shaped pipeline rather than kept as a parallel
contract. `src/ocr_neural.rs` (the Candle tombstone) stays untouched.

## Versioned corpus metrics

The standard OCR benchmark corpora are a licensing minefield (FUNSD and IAM
are non-commercial-only; ICDAR's redistribution terms are unverified against
the primary source). The metrics corpus is therefore **synthetic and
project-owned**:

- A deterministic generator (pinned fonts, sizes, DPI, text samples — Latin
  and Cyrillic) renders a small fixture set with exact ground truth;
  generation parameters are versioned in-repo, so the corpus is
  reproducible byte-for-byte.
- Tracked metrics: CER and WER per script, plus line-detection IoU. Metric
  values for the pinned model version are recorded next to the manifest;
  a model bump must update them in the same PR.
- Optionally, a few hand-picked public-domain scans may supplement the
  synthetic set later; any third-party corpus is non-blocking and only
  added after its license is verified at the primary source.

## Slice plan

1. **(this slice)** Design doc — model family, provisioning policy,
   contract.
2. **(done)** Spike + detection: both mobile models verified under
   tract-onnx 0.22; shipped as `ocr_onnx::{manifest, preprocess, detect}` —
   pinned manifest (`docs/ocr-model-manifest.toml`) + SHA-256 verification,
   `scripts/fetch_ocr_models.sh`, deterministic fixed-point preprocessing,
   DBNet detection with a shape-keyed plan LRU, and the main-only
   `OCR (onnx models)` CI job.
3. **(done)** Recognition + text layer: Cyrillic PP-OCRv5 CTC recognition
   against the pinned dictionary (`ocr_onnx::recognize`; the dictionary also
   covers Latin, digits, and punctuation — the separate Latin model stays
   pinned but unwired), detector-order line emission, `TextLayer` assembly
   with the proportional word-split heuristic (`ocr_onnx::pipeline`), and
   CLI wiring as `--backend onnx` through the existing seam
   (`docs/ocr-backend-seam.md`).
4. **(done)** Metrics corpus: deterministic synthetic generator (pinned
   PT Sans font as a manifest artifact), CER/WER/IoU harness
   (`ocr_onnx::{metrics, corpus}`), thresholds in the model-gated corpus
   test, recorded baseline in `docs/ocr-model-metrics.md`.
