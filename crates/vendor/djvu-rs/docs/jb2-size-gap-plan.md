# Plan: closing the JB2 encoder size gap vs DjVuLibre

Status: **Both branches evaluated. Branch A: validated lossless −11.7% Sjbz on text (same-size rec-6, experimental). Branch B: B0 found the existing same-size `lossy_threshold` is a −22…−24%/SSIM≥0.999 text lever that ships off by default; B1 (cross-size lossy rec-7) implemented + measured + REVERTED (dominated by raising `lossy_threshold`). Shipped `Jb2EncodeOptions::lossy_text()` (opt-in 0.02 preset, ≈−22% text at SSIM 0.999) + enriched docs; NOT enabled by default (archival-safe). Plan complete.** Owner: perf.
See `PERF_EXPERIMENTS.md` for the measured history this plan builds on.

## 1. Where the gap actually is

The naive framing ("our JB2 files are bigger") is too coarse. The measured
picture (from `PERF_EXPERIMENTS.md`):

- **Library membership is not the lever.** Singleton pruning (emit page-unique
  glyphs as record-3, not added to the dict) *regressed* +0.07 % — the adaptive
  `symbol_index` context already pays almost nothing for unused dict slots, and
  adding a third `record_type` value raised that stream's entropy.
- **The mask is near parity (≈1.04×)** after shared-Djbz dedup (#446, #452).
- **The remaining JB2 lever is glyph *matching*.** DjVuLibre's `cjb2` shares and
  refines near-identical glyphs; our default path is exact-match only
  (record-7 exact copy or record-1 fresh symbol).
- **Scope caveat — where JB2 even matters.** JB2 (Sjbz) is 67 % of a *text* file
  but only 3–36 % of a *colour/photo* file, where **IW44 BG44 dominates
  (54–99 %) and is itself ~14 % larger than DjVuLibre**. So this JB2 work pays
  off on text documents; for colour archives the bigger size lever is the IW44
  encoder (tracked separately).

## 2. What has already been tried (and the lessons)

| Attempt | Result | Lesson |
|---------|--------|--------|
| #301 cross-size rec-6 byte-cost **estimator** | predicted **−54 %** / −26 % | **the estimator lies** — it costs a packed-Hamming proxy, not a real ZP-coded refinement bitstream, so it is wildly optimistic |
| #322 real cross-size rec-6 **emitter** | actual **+4.37 %** / +0.24 % | a cross-size reference must be nearest-neighbor **resampled**; the geometric misalignment makes the 11-bit refinement context mispredict, so the refinement bitmap costs nearly as much as a fresh symbol **plus** the rec-6 index + wdiff/hdiff overhead |
| #224 Phase 4 lossy rec-7 substitution | kept, **opt-in** (`lossy_threshold`) | treating a near-twin as a byte-exact copy works, but it is lossy and off by default |
| singleton → rec-3 pruning | reverted (+0.07 %) | dict membership is not the lever |

**Key code finding:** there is **no same-size lossless record-6 refinement** in the
encoder. `find_refinement_ref` exists only as a phantom doc-comment; the only rec-6
path wired up is the experimental **cross-size** one (`cross_size_rec6_probe`,
default `None`), which is exactly the path #322 proved loses. The refinement
emitter itself (`encode_bitmap_ref`) is written and round-trips correctly — it is
just only ever invoked cross-size.

## 3. The untried lever

**Same-size lossless record-6 refinement.** A fresh CC with the *same bounding box*
as a dictionary glyph and a small Hamming distance needs **no resampling** — the
reference aligns pixel-for-pixel, the refinement context stays synchronised, and the
refinement bitmap should cost bits only for the differing pixels. This is the case
that avoids the misalignment that killed #322, and it is lossless (unlike #224's
rec-7). It is what `cjb2 -lossless` does and what we lack.

This is *not guaranteed* to win — same-size near-twins may be rare on a given
corpus, and rec-6 is blit-only (does not extend the dict), so refining a glyph that
is later copied exact-many-times "poisons" those future cheap rec-7 hits. Phase A0
measures the population before any encoder change; the branch stops early on a
negative result, per the #322 discipline.

## 4. Work plan

### Branch A (primary, lossless) — same-size record-6 refinement

- **A0 — measurement stand (no encoder change).** For each fresh CC (no exact dict
  hit), enumerate same-size dict candidates and record the Hamming-distance
  distribution and the near-twin population at several thresholds (≤2 %, ≤5 %,
  ≤10 % flipped pixels). Report on `watchmaker` (text, Sjbz-heavy) and
  `pathogenic_bacteria_1896` (517 pages, scale). Answers "is there anything to
  refine?" *Selection only — proves nothing about bytes* (the #301 lesson).
- **A1 — emitter behind a flag.** Add `same_size_rec6` to `Jb2EncodeOptions`
  (default `None`, mirroring `cross_size_rec6_probe`). A fresh CC with a same-size
  dict twin within budget emits a lossless rec-6 (`wdiff = hdiff = 0`, refinement
  bitmap via the existing `encode_bitmap_ref`) instead of a rec-1. Ship a
  `same_size_rec6_off_is_byte_identical` test so every default encoder stays
  byte-identical.
- **A2 — measure REAL bytes + round-trip.** Emit on both corpora, diff the actual
  Sjbz byte totals against baseline, and require **pixel-exact** round-trip.
  **Stop condition:** if bytes grow or round-trip is not exact, record the negative
  result and stop — do not tune (the #322 discipline).
- **A3 — if it wins.** Sweep the Hamming budget; handle the shared-Djbz case
  (refining against a cross-page dict slot); add the "don't poison future rec-7"
  heuristic (only refine when the reference is not a frequently-exact-copied glyph).

### Branch B (secondary, lossy) — strengthen the #224 matcher toward cjb2's operating point

- Sweep `lossy_threshold` with **perceptual** validation via the D1 harness
  (PSNR/SSIM), not the arithmetic pixdiff.
- Evaluate **cross-size lossy rec-7**: substitute a different-size near-twin as an
  exact copy. Unlike cross-size *rec-6*, this emits **no refinement bitmap**, so it
  has none of the context-misalignment cost that sank #322 — only the index record.
  This is closest to what `cjb2` does by default.
- Define a documented "cjb2-like" quality preset. Requires a product decision on
  whether lossy-by-default is acceptable.

## 5. Validation methodology (hard rules, from #301 / #322)

1. **Never trust the estimator** — decide only on really-emitted bytes.
2. **Round-trip pixel-exact** for Branch A (lossless); **D1 PSNR/SSIM** for
   Branch B (lossy).
3. **Flag + `off_is_byte_identical`** — default encoders stay byte-identical until a
   win is proven.
4. **Corpus:** `watchmaker` (text, Sjbz 67 %) + `pathogenic_bacteria_1896`
   (517 pages).
5. **Account for rec-7 poisoning** — rec-6 is blit-only and never extends the dict.

## 6. Priority & expectations

- **Branch A first** — the only untried lossless lever, it sidesteps the proven
  cause of #322's loss, and the emitter machinery already exists. Begin strictly
  A0 → A2 and be ready to record a negative result.
- **Branch B next / in parallel** — closest to `cjb2`'s default, but needs a
  lossy-by-default product decision.
- **Remember the bigger picture** — for real colour archives the larger size lever
  is IW44 BG44 (~14 %), not JB2.
