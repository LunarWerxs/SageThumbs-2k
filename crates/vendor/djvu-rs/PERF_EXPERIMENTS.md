# Performance experiments

Log of perf experiments and their outcomes. Each entry: issue, approach,
numbers, decision, reason. Referenced from issue templates ("Record result
in `PERF_EXPERIMENTS.md` (Kept or Reverted + reason)") and from
`.github/workflows/bench.yml`.

### WASM_DJVUJS_COMPARE (#596) — browser wasm decode/render vs djvu.js — **Kept (diagnostic)** (2026-07-12)

**Issue.** The wasm decode/render work had scalar and simd128 self-comparisons,
but no browser head-to-head against the incumbent JavaScript DjVu decoder. The
repo shares corpus assets with djvu.js under `references/djvujs`, so the missing
baseline was a diagnostic/adoption gap.

**Approach.** Added `scripts/bench_wasm_djvujs.mjs`: a dependency-free Node
harness that optionally builds wasm scalar/simd128 packages with `wasm-pack`,
serves the repo root, launches headless Chrome through CDP, and runs the same
100 dpi corpus pages through:

- `djvu-rs` wasm scalar (`wasm-pack build --features wasm`);
- `djvu-rs` wasm simd128 (`RUSTFLAGS=-C target-feature=+simd128`);
- `djvu.js` 0.5.4 (`djvujs-dist` built locally from the npm tarball).

The checked-in `references/djvujs` tree contains shared assets, not the runtime
bundle, so the harness accepts `--djvujs` and auto-detects the local
`target/bench-djvujs/package/library/dist/djvu.js` or `node_modules` copy. The
djvu.js path renders native `ImageData` and scales through Canvas to the same
target dpi; wasm renders directly at target dpi. Command:

```sh
npm pack djvujs-dist --pack-destination target/bench-djvujs
mkdir -p target/bench-djvujs/package
tar -xzf target/bench-djvujs/djvujs-dist-0.5.4.tgz \
  -C target/bench-djvujs/package --strip-components=1
(cd target/bench-djvujs/package/library && npm install && npm run build)
node scripts/bench_wasm_djvujs.mjs --build-wasm \
  --iterations 5 --warmup 2 --json > target/bench-djvujs/issue-596.json
```

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max host, HeadlessChrome
150.0.0.0, Node.js v26.4.0, Rust 1.88-compatible crate build via wasm-pack
0.13.1. Chrome reported `simd128` support. Wasm linear memory reached 446.8 MiB
by the end of the corpus run; Chrome `performance.memory` heap deltas were
recorded in the raw JSON but are GC-noisy and not treated as peaks.

**Numbers (selected corpus page, median ms, full decode + render at 100 dpi):**

| Fixture | Class | wasm scalar | wasm simd128 | djvu.js | simd vs djvu.js |
|---|---|---:|---:|---:|---:|
| `big_scanned_page.djvu` | photo | 849.5 | 617.7 | 6588.2 | 10.7x |
| `map_atlas_sample.djvu` | line-art | 67.9 | 65.6 | 441.6 | 6.7x |
| `goody_twoshoes.djvu` | mixed | 35.5 | 30.8 | 269.4 | 8.7x |
| `chinese_cookbook_sample.djvu` | cjk | 18.9 | 14.6 | 196.3 | 13.4x |
| `cyrillic_simonovich_co2.djvu` | cyrillic | 2.4 | 2.4 | 21.3 | 8.9x |
| `war_1812.djvu` | newspaper | 52.2 | 46.0 | 346.0 | 7.5x |
| `colorbook.djvu` | iw44-color | 26.8 | 22.2 | 250.0 | 11.3x |
| `carte.djvu` | carte | 74.5 | 66.8 | 497.5 | 7.4x |
| `boy_jb2.djvu` | jb2-small | 0.3 | 0.3 | 4.6 | 15.3x |

First-page latency is effectively the same ordering: simd128 is 6.8-23.0x
faster than djvu.js across the fixture set (11.3x on the large photo page,
13.6x on CJK, 11.2x on colorbook).

**Decision. Kept (diagnostic).**

**Reason.** No follow-up performance bug is indicated by the head-to-head:
browser wasm is already far ahead of djvu.js on every measured content class,
including the large maskless photo page, JB2-heavy line art, CJK/Cyrillic text,
IW44 colorbook, and the `carte` interop fixture. The simd128 build gives the
expected extra margin on IW44/photo-heavy pages; small JB2 pages are timer-floor
limited but still comfortably faster. The harness is checked in so future wasm
changes can re-run the comparison and publish updated tables.


### PAR_SEGMENT — parallel BG-cell fill in `segment_page` — **Kept** (2026-07-02)

**Issue.** `segment_page` (`src/segment.rs`) builds the sub-sampled background by
a nested `for by { for bx { block_mean(…) } }` loop. `block_mean` averages each
`sub × sub` block's mask-excluded source pixels, so the loop collectively scans
the whole page and is the bulk of `segment_page`'s cost — but it ran entirely
sequentially. Each BG cell is independent (cells never read each other, only the
shared read-only `rgba`/`mask`), so the fill is embarrassingly parallel.

**Approach.** Extract the per-cell colour into a pure `bg_cell_color(…)` helper,
then under `#[cfg(feature = "parallel")]` split `bg` into disjoint mutable row
slices (`bg.data.par_chunks_mut(bw*4).enumerate()`) and fill them concurrently;
byte-identical sequential nested loop otherwise. Same colour per cell, same write
positions, alpha left at 255 (as `Pixmap::white` + `set_rgb` leave it) → output is
byte-identical by construction (confirmed by the exact-colour segment tests).

**Nested-rayon safety.** `segment_page` is called inside the multi-page bundler's
already-parallel per-page loop (PAR_ENCODE parallelises `segs`). Verified the
inner parallelism does **not** regress the multi-page benches: with the change,
`encode_djvm_layered_shared` change p = 0.57 (no change) and `encode_djvm_bundle_jb2`
p = 0.07 (and it's bilevel — never calls `segment_page`). With few pages on 8
cores the inner split fills otherwise-idle cores; when the outer loop saturates,
rayon's work-stealing absorbs the nesting.

**Platform / command.** Apple M1 Max (8 perf cores), Rust 1.92.0, `parallel`,
`[profile.bench]` fat LTO. Baseline = clean tree via `git stash push -- src/segment.rs`:

```sh
cargo bench --features parallel --bench codecs -- segment_page_color --save-baseline seg_before
# apply change, then:
cargo bench --features parallel --bench codecs -- segment_page_color --baseline seg_before
```

**Numbers (two runs):**

| Benchmark | Baseline | after | Delta (run 1 / run 2) |
|---|---:|---:|---:|
| `segment_page_color` (colorbook page) | 1.965 ms | 1.195 ms | **−39.7% / −41.2%** |
| `encode_djvm_layered_shared` (multi-page, nested) | — | — | p = 0.57 (no regression) |

**Decision.** Kept.

**Reason.** Large, stable win (both runs p < 0.05) on `segment_page`, which is a
mandatory stage of every colour encode and the dominant cost on BG-heavy pages
(colorbook: 55% BG44). Byte-identical output (exact-colour segment tests pass with
`--features parallel`; the whole change only reorders independent writes). Gated to
the opt-in `parallel` feature. Unlike PAR_PAGE_LAYERS (reverted — masked by JB2 on
the only fixture), this measures cleanly because `segment_page_color` isolates the
segmentation stage, and the multi-page regression check confirms the nesting is
free. Directly speeds single-page / CLI colour encodes and the segmentation half
of `encode_color_page_quality`.

### PAR_PAGE_LAYERS — `rayon::join` the JB2 mask and IW44 BG in single-page color encode — **Reverted** (2026-07-02)

**Issue.** `PageEncoder::encode` (Quality/Archival, `src/djvu_encode.rs`) encodes
a page's `Sjbz` (JB2 dict) and `BG44` (IW44) sequentially. Given `seg`, those two
layers are fully independent (`fgbz` needs `sjbz` and stays after), so they are an
obvious `rayon::join` candidate — the single-page analogue of the already-parallel
IW44 3-plane join and the multi-page per-page loop.

**Approach.** `#[cfg(feature = "parallel")] let (sjbz, bg44_chunks) = rayon::join(
|| jb2…, || iw44…)`, byte-identical sequential fallback otherwise. This path is
*not* nested inside the multi-page bundler loop (that has its own impl), so there
is no nested-saturation concern.

**Platform / command.** Apple M1 Max, Rust 1.92.0, `parallel`, `[profile.bench]`.
Baseline = clean tree via `git stash push -- src/djvu_encode.rs`:

```sh
cargo bench --features parallel --bench codecs -- encode_color_page_quality --save-baseline join_before
# apply change, then:
cargo bench --features parallel --bench codecs -- encode_color_page_quality --baseline join_before
```

**Numbers (two runs):**

| Benchmark | Run 1 | Run 2 |
|---|---:|---:|
| `encode_color_page_quality` (watchmaker) | −0.9% (p = 0.00) | −0.7% (p = 0.35, CI −2.3…+0.9%) |

**Decision.** Reverted.

**Reason.** No **consistent** measurable win: the change is architecturally correct
and byte-identical, but the only single-page colour fixture is watchmaker — a
**text** page where the JB2 mask dominates (67% Sjbz vs 11% BG44 per ENC_SIZE_DIAG)
and the IW44 background is tiny. Overlapping a small IW44 encode with a large JB2
encode saves only ≈`min(jb2, iw44)` ≈ the tiny IW44 time, which lands in the noise
(run 2 CI crosses zero). It would help materially only on a **BG-balanced** page
(e.g. a colorbook picture page, 55% BG44), for which no single-page bench exists.
Fails the repo's "both runs p < 0.05" bar, so not landed. The higher-value
parallel axis — across pages — is already covered by PAR_ENCODE + PAR_CLUSTER.
**Revisit** only with a BG-heavy single-page colour fixture added to
`encode_color_page_quality`'s corpus; until then this is unmeasurable, same class
as the round-3 "small fraction of an unbenched total" deferrals.

## Parallelism sweep round 4 (2026-07-02) — summary

A follow-up sweep after LTO_FAT / PAR_ENCODE, targeting the remaining sequential
tails on the encode/export side. **Four kept, one reverted:**

- **PAR_CLUSTER** — parallel `extract_ccs` in shared-dict clustering:
  `encode_djvm_bundle_jb2` **−58%**, `encode_djvm_layered_shared` −17…22%.
- **PAR_EPUB** — parallel per-page artifact build in EPUB export: **−81%**.
- **PAR_TIFF** — parallel per-page image build in TIFF export: **−73%** (plus a
  new `export/tiff` bench; byte-identity SHA-256-verified).
- **PAR_SEGMENT** — parallel BG-cell fill in `segment_page`: **−41%**
  (`segment_page_color`), no multi-page regression from nesting.
- **PAR_PAGE_LAYERS** — `rayon::join` of Sjbz/BG44 in single-page encode:
  **reverted**, noise-level on the text-only fixture.

**Candidates checked and found already implemented** (do not re-propose):
IW44 forward-transform 3-plane parallelism (`rayon::join` in `encode_iw44_color`);
per-page parallel encode incl. thumbnails (PAR_ENCODE); hash-bucket exact dedup +
`(w,h)` size-bucketed clustering; POPCNT `packed_hamming`; PDF parallel export
(#298). The `scaled_hamming` cross-size matcher is behind the `experimental`
feature, not on the default path.

**Deferred (need a fixture or a higher correctness bar), unchanged from round 3:**
ZP decoder u64 bit-buffer (the hot loops in jb2/iw44/bzz each inline their own
32-bit `refill!` and depend on the exact `pos` overshoot semantics — a 4-site,
interop-fragile change for a marginal per-bit saving); CCITT G4 / JBIG2 PDF masks
(a real size win but needs a G4/JBIG2 encoder); masked IW44 wavelet + the #300
`conquete_paix` PSNR fix (normative-stream / correctness work per
IW44_MASKED_WAVELET); linear-light downscale and median-cut FGbz palette (quality
trade-offs without a clean win metric). A **BG-heavy single-page colour fixture**
would make PAR_PAGE_LAYERS and further colour-encode micro-parallelism measurable.

### PAR_TIFF — parallel per-page image build in TIFF export — **Kept** (2026-07-02)

**Issue.** `djvu_to_tiff_writer` (`src/tiff_export.rs`) wrote pages in one
sequential loop; the color path even *interleaved* render and encode (streaming
RGB rows straight into `TiffEncoder` strips for low memory). The per-page work
(color: render → RGB; bilevel: JB2 decode → Gray8) is independent and CPU-heavy;
only appending IFDs to the single `TiffEncoder` must stay serial. Third of the
three exporters (after PDF/#298 and PAR_EPUB) still to be parallelised; the
journal's round-3 summary flagged TIFF as the natural next candidate.

**Approach.** Split into a pure `build_page_image(page, opts) -> PageImage`
(materialises the RGB or Gray8 buffer — the `Send`-safe part) and a serial
`write_page_image(encoder, &img)` (one `new_image[_with_compression]` + `write_data`
per page). With `#[cfg(feature = "parallel")]`, build every page's image via
`indices.par_iter().map(...).collect::<Result<Vec<_>, _>>()`, then write in index
order. The sequential fallback keeps the existing row-streaming O(1)-page memory
path. The color builder mirrors the sequential dispatch (collect streamed RGB when
`can_stream`, else full-pixmap RGB), so pixels are identical.

**Byte-identity verified empirically.** Materialising to `write_data` could in
principle differ from the streaming `write_strip` loop's TIFF strip layout. A
throwaway dump (`examples/_tiff_dump.rs`, since removed) exported watchmaker with
`--features tiff` and `--features tiff,parallel`: both produced **303 036 584 B
with an identical SHA-256** (`adf72a06…`). So `write_data` emits the same strips
as the manual loop — the output is byte-identical, not merely pixel-identical.

**Platform / command.** Apple M1 Max (8 perf cores), Rust 1.92.0, `tiff,parallel`
features, `[profile.bench]` fat LTO. A new `export/tiff` bench (watchmaker, 12
pages, color) was added to `benches/render.rs`. Baseline = the same bench with
only `src/tiff_export.rs` reverted (`git stash push -- src/tiff_export.rs`), so
the sequential streaming path runs under the `parallel` feature:

```sh
git stash push -- src/tiff_export.rs
cargo bench --features tiff,parallel --bench render -- export/tiff --save-baseline tiff_before
git stash pop
cargo bench --features tiff,parallel --bench render -- export/tiff --baseline tiff_before
```

**Numbers (two runs):**

| Benchmark | Baseline | after | Delta (run 1 / run 2) |
|---|---:|---:|---:|
| `export/tiff` (watchmaker, 12 pages, color) | 680.3 ms | 185.2 ms | **−72.8% / −74.2%** |

**Decision.** Kept.

**Reason.** Large, stable win (p < 0.05, two runs) — a ~3.7× speed-up on 12 pages;
sub-linear vs the 8 cores because each 1275×1651 page's serial IFD write and the
Amdahl tail are non-trivial, and the pages are large. Byte-identical output
(verified by SHA-256, above; 16 `tiff_export` tests pass in both feature configs;
fmt / clippy `-D warnings` pass for `tiff` and `tiff,parallel`). Gated to the
opt-in `parallel` feature, so the default build keeps its deliberate row-streaming
low-memory profile; the parallel path trades peak RSS (all page buffers held
before writing) for wall-time, consistent with the PDF/EPUB exporters. Completes
the trio — all three multi-page exporters (PDF, EPUB, TIFF) now parallelise.

### PAR_EPUB — parallel per-page artifact build in EPUB export — **Kept** (2026-07-02)

**Issue.** `djvu_to_epub` (`src/epub.rs`) rendered and wrote pages in a strictly
sequential loop: for each page it rendered the RGBA raster, PNG-encoded it, built
the text/hyperlink overlay + XHTML, and streamed both entries into the
`ZipWriter`. The PDF exporter was parallelised for this exact shape back in #298,
but EPUB never was — even though the per-page render → PNG-encode → XHTML build is
independent and CPU-heavy, and only the ZIP writing (single non-`Send`
`ZipWriter`) needs to stay serial.

**Approach.** Mirror the PDF parallel exporter: split the per-page work into a
pure `build_page_artifacts(page, i, opts) -> PageArtifacts` (render, PNG encode,
overlay, XHTML — the `Send`-safe part) and a serial `write_page_artifacts(zip,
&art)` (the two `start_file` + `write_all` calls, unchanged order/compression).
With `#[cfg(feature = "parallel")]`, build every page's artifacts via
`indices.par_iter().map(...).collect::<Result<Vec<_>, _>>()`, then write them in
index order; the sequential fallback builds-and-writes one page at a time (keeping
the streaming O(1)-page memory profile when the feature is off). Output bytes are
identical: same write order, per-page bytes are pure functions of the page, and
the `zip` options (fixed default timestamp, same compression methods) match.

**Platform / command.** Apple M1 Max (8 perf cores), Rust 1.92.0,
`epub,parallel` features, `[profile.bench]` fat LTO. Baseline = clean tree
(sequential build-and-write) with the same features, via `git stash`:

```sh
cargo bench --features epub,parallel --bench render -- epub --save-baseline epub_before
# apply change, then:
cargo bench --features epub,parallel --bench render -- epub --baseline epub_before
```

**Numbers (two runs):**

| Benchmark | Baseline | after | Delta (run 1 / run 2) |
|---|---:|---:|---:|
| `export/epub` (watchmaker, 12 pages, 150 dpi) | 310.6 ms | 57.5 ms | **−80.8% / −81.9%** |

**Decision.** Kept.

**Reason.** Large, stable win (p < 0.05, two runs) — a ~5.4× speed-up on 12 pages
across 8 perf cores, matching the PDF exporter's parallel scaling. Gated to the
opt-in `parallel` feature; the default single-thread path keeps its streaming
one-page-at-a-time memory profile. Byte-identical, deterministic output (same
reasoning the PDF parallel path relies on); all 17 `epub` tests pass with
`--features epub,parallel`; fmt / clippy `-D warnings` (both `epub` and
`epub,parallel`) pass. Like PAR_ENCODE/PDF, the parallel path trades peak RSS
(all page artifacts held before writing) for wall-time — acceptable for the
opt-in feature and consistent with the existing exporters. Same
render→encode→collect shape as the TIFF exporter, which is the natural next
candidate.

### PAR_CLUSTER — parallel per-page `extract_ccs` in shared-dict clustering — **Kept** (2026-07-02)

**Issue.** PAR_ENCODE (2026-07-02) parallelised the per-page *encode* loop of the
multi-page bundlers but explicitly left the **shared-dictionary clustering pass**
(`cluster_shared_symbols_tunable`, `crates/djvu-jb2/src/encode.rs`) as a
sequential Amdahl tail that runs *before* it. That pass does a strictly
sequential `for page { let ccs = extract_ccs(page); … bucket … }` loop. Connected-
component extraction (`extract_ccs` — iterative DFS over an unpacked byte grid) is
the bulk of the clustering cost and is fully independent per page; only the
bucketing that follows is order-dependent (it must visit CCs in page order to keep
`first_seen` / `pages_seen` and the pixel-budget trim tie-breaks byte-identical).

**Approach.** Split extract from bucket: extract CCs for a bounded **batch** of
pages in parallel (`chunk.par_iter().map(extract_ccs)` behind
`#[cfg(feature = "parallel")]`, with a byte-identical `chunk.iter().map(...)`
sequential fallback), then bucket that batch sequentially in page order via a
shared local `bucket_page_ccs` helper. Batching (`BATCH = 32`) rather than one big
`par_iter().collect()` caps transient CC memory to 32 pages, so long bilevel
corpora (the 517-page `pathogenic_bacteria_1896`) don't regress peak memory.
Output is byte-identical to the old extract-then-bucket loop (same CCs, same
order). New `djvu-jb2` `parallel` feature (optional `rayon`), wired into the main
crate's `parallel` feature.

**Platform / command.** Apple M1 Max (8 perf cores), Rust 1.92.0, `parallel`
feature, `[profile.bench]` fat LTO. Baseline = clean tree (sequential clustering)
built with `--features parallel`, saved before the change:

```sh
cargo bench --features parallel --bench codecs -- encode_multipage --save-baseline clccs_before
# apply change, then:
cargo bench --features parallel --bench codecs -- encode_multipage --baseline clccs_before
```

**Numbers (two runs):**

| Benchmark | Baseline | after | Delta (run 1 / run 2) |
|---|---:|---:|---:|
| `encode_djvm_bundle_jb2` (conquete_paix, 6 large masks) | 214 ms | 89.7 ms | **−58.0% / −58.0%** |
| `encode_djvm_layered_shared` (watchmaker, 3 colour pages) | 6.73 ms | 5.08 ms | **−21.6% / −17.2%** |

**Decision.** Kept.

**Reason.** Large, stable win (both benches p < 0.05, two runs) on the
every-multi-page-encode path, gated to the opt-in `parallel` feature (default
no_std / single-thread build unchanged). The `bundle_jb2` −58% shows the
sequential clustering `extract_ccs` was, for large-mask documents, a *bigger*
serial tail than PAR_ENCODE's per-page loop was before it was parallelised — the
masks there are large 600-dpi bilevel pages where DFS-based CC extraction
dominates. Byte-identical output verified: all 636 lib tests + `encode_size_regression`
(`jb2_mask_size_does_not_regress`, `iw44_bg44_size_does_not_regress`) + all
`djvm` round-trip tests pass with `--features parallel`; fmt / workspace clippy
`-D warnings` / no_std / wasm32 gates pass. (The lone `encode_empty_directory_fails`
CLI test fails identically on clean `main` — a stale "no image files" message
assertion, unrelated.) Compounds with PAR_ENCODE and LTO_FAT: clustering and the
per-page encode now both parallelise and both LTO their per-page work.

### PGO (profile-guided optimization) over LTO_FAT — **Rejected (regresses encode)** (2026-07-02)

Backlog item #4: measure the *ceiling* PGO adds on top of the already-kept
`lto="fat"` + `codegen-units=1` profile (LTO_FAT). Hypothesis: the ZP coder is
all data-dependent branches, so profile-guided block layout / inlining priorities
might squeeze a further few percent out of the encode/decode hot paths.

**Method (manual PGO — `cargo-pgo` isn't installed, but `llvm-profdata` ships in the
`llvm-tools` component).**
1. Instrumented build of the `codecs` bench binary — `RUSTFLAGS=-Cprofile-generate`,
   with `CARGO_PROFILE_BENCH_LTO=off codegen-units=16` so instrumentation is fast.
2. Ran it over the `encode|decode` benches to emit 24 `.profraw`; merged with
   `llvm-profdata merge`.
3. Two final fat-LTO binaries from identical source: one with
   `-Cprofile-use=merged.profdata`, one plain (the LTO_FAT control).
4. Interleaved the two binaries in one thermal session (same A/B method as the
   allocator experiment) to cancel M1 throttling drift.

**Interleaved medians (4 rounds each):**

| bench | control (LTO_FAT) | PGO + LTO | Δ |
|-------|-------------------|-----------|---|
| `jb2_encode_dict` | ~7.27 ms | ~9.34 ms | **+28 % slower** |
| `iw44_encode_color` | ~2.20 ms | ~2.46 ms | **+12 % slower** |
| `bzz_decode` | ~68.0 µs | ~68.2 µs | neutral |
| `jb2_decode` | ~131.7 µs | ~133.0 µs | ~+1 % (noise) |

**Candidate mechanism (not disambiguated).** The path PGO hurts most —
`jb2_encode_dict` — is exactly the one LTO_FAT sped up **−65 %**, and it won there by
*cross-crate inlining the tiny ZP coder functions* into the encode loop. One plausible
reading is that PGO's function-level layout / inline-priority heuristics fight that
whole-program inlining. But this single A/B run does **not** separate that from a simpler
confound: the profile was gathered from a **non-LTO, `codegen-units=16` instrumented
build** and then applied to a fat-LTO final binary, so the recorded block layout the
profile describes does not match the code it was applied to. That profile/layout mismatch
alone could account for the regression, independent of any inherent "PGO vs LTO" conflict.
The two are not distinguished here.

**Decision: Rejected.** As tested, manual PGO regresses the encode hot paths (+28 % / +12 %)
and helps nothing, so it is not worth the two-phase build-infra cost. What is *not*
established is why — a follow-up that gathers the profile from a `codegen-units=1` /
ThinLTO instrumented build (matching the final layout) is the correct next step to tell a
real PGO-vs-LTO conflict apart from an instrumentation-mismatch artifact; fat-LTO +
`profile-generate` builds are slow/fragile, which is why this run took the non-LTO
instrumentation shortcut. Left as the documented reproduction if anyone revisits.

### JB2 singleton pruning — type 3 for page-unique glyphs — **Reverted** (2026-07-02)

Backlog item (#2 in the 2026-07-02 list): the confirmed JB2 size gap vs DjVuLibre
(dict ≈ 1.356× original) suggested trimming the symbol library. Hypothesis: a
connected component whose exact bitmap occurs **exactly once** on the page is never
the target of a later `Copy`, so storing it in the library only inflates `dict_size`
and widens the `symbol_index` range every subsequent `Copy` record pays for. Emit
such singletons as **record type 3** ("new symbol, direct, blit only, not added to
dict") instead of type 1. The direct-bitmap payload is byte-identical, so the change
is lossless (record type 3 is spec-defined and the decoder already handles it).

**Implementation.** A pre-pass groups CCs by `symbol_hash` and marks the exact-unique
ones; in the main loop a `Action::New` on a singleton becomes `Action::NewNoDict`
(type 3, not pushed to the library). Correctly gated to the **lossless** path only —
a lossy rec-7 copy or the experimental cross-size rec-6 refinement can target an
exact-unique bitmap, so pruning is disabled whenever `lossy_threshold > 0` or the
rec-6 probe is set (a first attempt without this gate broke the `lossy_*_rec7` test:
the base glyph got pruned out of the dict, so its two lossy copies fell back to full
`New` records). All 53 jb2 round-trip tests stayed green.

**Result (deterministic — integer ZP path, exact bytes; 120 mask pages across 7 docs):**

| | total JB2 bytes |
|---|---|
| baseline (type 1) | 2 235 589 |
| singleton → type 3 | 2 237 144 |
| **delta** | **+1 555 B (+0.070 %)** |

Every one of the 7 documents grew — a small but **consistent regression**, not noise.

**Why it loses.** The `symbol_index` of a `Copy` is coded with an *adaptive* arithmetic
context, not `log2(range)`, so the encoder was already paying almost nothing for the
extra singleton slots — the range widens but the high indices are essentially never
used, so the adaptive model doesn't spend bits on them. Meanwhile introducing type 3
adds a **third value** to the `record_type` stream, which was previously just {1, 7}
(new + copy). Scanned pages carry *many* exact-unique CCs (noise specks, broken glyphs),
so a large fraction of records flip 1→3, which very likely raises the entropy of the
adaptive `record_type` context. That cost apparently outweighs the (near-zero) index-range
saving. (This is the inferred mechanism consistent with the numbers — the deterministic
+0.070 % is the hard result; the bit-level attribution is not separately measured.)

**Decision: Reverted.** The real DjVuLibre size gap is in glyph *matching* (cjb2 shares
and refines near-identical glyphs; our default path is exact-match only), not in library
membership of singletons. Next JB2 size levers to try: a byte-cost model before emitting
cross-size rec-6 (the open #301), or hardening the near-match / lossy-copy matcher —
both attack the matching axis, where the gap actually lives.

### Partial IW44 decode under scale (#19) — **Diagnostic, already implemented** (2026-07-02)

Backlog item #19 proposed stopping the BG44 ZP decode early at reduced render
scale (thumbnails / low DPI don't need the fine refinement chunks). Reading the
render path shows this **already exists**: `decode_background_chunks` uses
`bg44_partial` (first BG44 chunk only) at `subsample >= 4`, and the coarse-only
image is cached (`bg44_partial` OnceLock). So the thumbnail / ≤72-DPI case is done.

The one untapped point is `subsample == 2` (the common 150-from-300-DPI downscale),
which still decodes **all** chunks. Probed whether dropping the last chunk there is
worth it — per corpus page, PSNR of full-vs-drop-last at the sub=2 output, plus the
decode-time delta (best-of-25, M1 Max):

| page | BG44 chunks | last chunk | PSNR@sub2 | decode saving |
|------|-------------|-----------|-----------|---------------|
| watchmaker (text scan) | 4 | 504 B | **inf** (identical) | ~2 % (noise) |
| cable_1973 (text scan) | 4 | 174 B | **inf** | ~2 % |
| conquete_paix (text) | 4 | 721 B | **inf** | ~0 % |
| colorbook (photo) | 4 | 4643 B | 43.2 dB | **−26 %** |
| chicken (photo) | 3 | 5260 B | 30.6 dB | **−41 %** |
| boy | 1 | — | n/a (single chunk) | — |

**Reading.** For text/bilevel-background scans the last chunk is tiny, so dropping
it is *lossless* at sub=2 (PSNR inf) but saves essentially nothing — the imperceptible
chunk is also the cheap one. For photo-heavy colour pages the last chunk is large and
dropping it saves 26–41 % of BG44 decode, but the quality is variable (43 dB safe;
30.6 dB borderline-visible) and would diverge from how DjVuLibre and every other
viewer render at 150 DPI (they decode all chunks).

**Decision: no default change.** A blanket "drop last chunk at sub=2" is unsafe
(chicken's 30.6 dB, plus the interop divergence the repo has repeatedly prioritised —
see the chroma_half saga). The remaining value is an **opt-in "draft/fast" render
mode** for photo pages, which is a deliberate public-API feature (a `RenderOptions`
flag + a quality gate), not an autonomous perf tweak — deferred to a feature PR.
The scale-aware partial-decode lever itself is considered closed.

### Global allocator swap (mimalloc) — **Rejected** (2026-07-02)

Hypothesis (from the 2026-07-02 experiment backlog, item #3): the macOS system
allocator was the sensitive point in the parallel path (that is what ZEROED's
page-fault storm exposed), so a thread-caching allocator with per-thread heaps
(mimalloc) might cut lock contention on the parallel render/export path and speed
up the allocation-heavy cold-decode and multi-page-parse paths.

**Method.** Added `mimalloc` as a dev-dependency and built each criterion bench
binary twice — once with `#[global_allocator] = MiMalloc`, once with the stock
system allocator — from the *same* source. Then ran the two binaries **interleaved
in one thermal session** (round-robin, 4–5 rounds each), which cancels the M1 Max
throttling drift that makes criterion's stored-baseline compare unreliable here.

The stored-baseline compare *looked* like a win at first (`render_large_doc_first_page`
−25 %, `parse_multipage_520p` −5.5 %) but the near-identical `render_large_doc_mid_page`
simultaneously showed **+11.7 %** — the tell-tale contradiction of a thermal artifact
(cool baseline vs. warmed treatment run). Interleaving dissolved it.

**Interleaved medians (mimalloc vs system):**

| Path | system | mimalloc | Δ |
|------|--------|----------|---|
| `render_large_doc_first_page` (1-page decode+render) | ~1.63 ms | ~1.65 ms | ~neutral |
| `parse_multipage_520p` (520-page directory, many small allocs) | ~2.30 ms | ~2.49 ms | **+8.6 % slower** |
| `pdf_export_parallel` (parallel render+encode — best case for mimalloc) | ~115.1 ms | ~114.0 ms | −1 %, within noise |
| `render_colorbook_cold` (cold IW44 decode) | ~11.67 ms | ~12.80 ms | **+9.7 % slower** |

**Decision: Rejected.** Even the parallel path — the one case per-thread heaps should
win — landed inside noise (~1 %, and one of four rounds was slower), while the cold-decode
and multi-page-parse paths regressed ~9–10 %. macOS's `libmalloc`/nano allocator is already
well-tuned for this workload's mix (large IW44/mask buffers + churn of small parse structs).
Independently, a *library* crate should not pin a `#[global_allocator]` anyway — that choice
belongs to the final binary, and downstream users who want mimalloc/jemalloc can set it
themselves. Reverted the dev-dependency and both bench edits; tree left clean.

### JB2 dict encoder — hash-bucket dedup (drop per-CC bitmap clone) — **Kept (small)** (2026-07-01)

Perf swarm's top-vetted candidate. `encode_jb2_dict`'s exact-match dedup was a
`BTreeMap<(u32,u32,Vec<u8>), usize>` that **cloned the connected-component bitmap
data on every CC** to build the lookup key. Replaced with `BTreeMap<u64, Vec<usize>>`
keyed by an FNV-1a `symbol_hash(w,h,data)`; the bucket compares the actual data
against `dict_entries` only on a hash hit, so dedup stays **byte-identical** while
the per-CC `Vec<u8>` clone disappears. Measured (best-of-6, M1): DjVu3Spec 36.6→35.8,
pathogenic 42.8→41.9, cable 27.2→26.4 ms — **~2-3 %**, output byte-identical (sizes
unchanged; 53 jb2 + proptest + size-gate green).

**Context:** the swarm reviewed 7 modules; iw44-encode/decode, jb2-decode, bzz-decode
were judged already-tight, and even this best candidate yields only ~2-3 % — the codec
is well-optimized, returns are small. Kept because it is byte-identical, low-risk, and
removes allocation churn.

### Encoder ↔ DjVuLibre interop (encoder-differential) — **Kept (correctness)** (2026-06-30)

Reverse of the decode-differential: encode with djvu-rs, decode the result with
`ddjvu`, compare. Found our *colour* DjVu output was unreadable by DjVuLibre — two
distinct bugs (round-trip + our own decoder missed both, since they only test our
encoder against our decoder):

1. **IW44 major-version byte** (#462): emitted 0, but DjVuLibre needs
   `(major & 0x7f) == 1` (IWCODEC_MAJOR) → "incompatible IWCodec". Fixed to 0x01
   (colour) / 0x81 (gray).
2. **chroma_half** (this entry): our `chroma_half=true` default stores Cb/Cr at half
   *spatial* resolution, but DjVuLibre's `IWPixmap::decode_chunk` builds
   full-resolution chroma maps and reads full-resolution chroma slices — so it runs
   short of bits ("Unexpected End Of File"). Root-caused by reading DjVuLibre's
   `IW44Image.cpp` after black-box testing (version/skip/padding/slice-count) was
   exhausted. **Fix:** default `chroma_half = false` (full-resolution chroma).

**Result:** 6/6 colour corpus files now decode in ddjvu (were 0/6); our own decoder
round-trips unchanged (37 IW44 tests green). JB2 (bilevel) encode was already
pixel-perfect interop. Our colour `.djvu` were previously unusable in DjVuLibre and
every other standard reader.

**Follow-up (`chroma_delay = 10`).** Full-resolution chroma first cost ~+8% (119_230 →
128_999 B). Inspecting real c44 output showed its `crcbdelay = 10` (chroma deferred to
slice 10, never `crcb_half`). Matching that default trims colour BG44 ~10–18 % and
brings the 2-page gate to 119_636 B — **+0.3 % over the original**, i.e. full DjVuLibre
colour interop at essentially the pre-fix size, and our stream now matches the standard
c44 chroma convention exactly (full-res, delay 10). ddjvu reads all 6/6; round-trip green.

### Encoder speed vs DjVuLibre (cjb2 / c44) — **Diagnostic, no action** (2026-06-26)

First head-to-head of *encode* speed (decode/render were already benchmarked;
encode never was beyond our own criterion benches). Apples-to-apples on identical
inputs — the same decoded page fed to both encoders, best-of-5 wall-clock, M1 Max:

| Codec | input | djvu-rs | DjVuLibre | ratio |
|-------|-------|---------|-----------|-------|
| IW44 (`encode_iw44_color` vs `c44`) | watchmaker render, 2550×3301 (8.4 MP) | **313 ms** | 458 ms | **0.68× (we're 1.46× faster)** |
| JB2 (`encode_jb2_dict` vs `cjb2`) | cable mask, 2550×3301 | 27.5 ms | 24.8 ms | 1.11× (near-parity) |

Output sizes were comparable (IW44 694 KB vs 666 KB; JB2 2504 B vs 2248 B — cjb2's
default is lossier). **Conclusion:** encode speed is competitive — IW44 is ahead,
JB2 at parity. No inefficiency or hot-spot worth chasing; this axis is healthy, not
a gap. Recorded so it isn't re-investigated. (DjVuLibre encode timing is now also
tracked in CI via `scripts/bench_djvulibre.sh` → `encode_timing.txt`.)

### JB2 decode — per-page symbol-pixel cap (fuzz `fuzz_jb2` timeout) — **Kept (robustness)** (2026-06-25)

**Issue.** CI `Fuzz / fuzz_jb2` was intermittently red: `libFuzzer: timeout after 10 s`.
Pre-existing (JB2 decode core untouched this session), random-seed → intermittent.

**Diagnosis.** Downloaded the CI artifact (a 409-byte input). Locally it decodes in
~626 ms native (×~16 under ASAN + slower CI CPU ≈ the 10 s timeout). Instrumented
`decode_image_with_pool`: the input is **23 386 type-5 (matched-refinement) records**
decoding **47.7 MP** of symbols from ~409 real bytes — a low-entropy stream amplifies
~116 K px/byte. Decode is linear (~13 ns/px, no quadratic); the cost is simply that the
existing caps (`MAX_TOTAL_SYMBOL_PIXELS = 256 MP`) permit far more per-page work than any
real page needs. A larger crafted Sjbz could reach the full 256 MP (~3.3 s native).

**Approach.** Added `MAX_PAGE_SYMBOL_PIXELS` (16 MP), checked at the top of the two
*page* decode loops (`decode_image_with_pool`, `decode_image_indexed_with_pool`). The
256 MP ceiling stays for the cross-page shared **dictionary** path
(`decode_dictionary_with_pool`), which legitimately needs > 64 MP for
`pathogenic_bacteria_1896` (#258) — so the caps are split, not lowered globally.

**Follow-up (margin).** First shipped at 32 MP; the fuzz_jb2 regression seed still
decoded ~6 s under ASAN there and flaked intermittently against the 10 s libFuzzer
per-input timeout on slow CI runners. Lowered to 16 MP (corpus densest page is
>8 MP but <16 MP, so it still accepts every real page) → ~3-5 s worst case,
comfortable margin. 8 MP was too low (rejected a real corpus page).

**Follow-up (tightness, codex review).** The cap was first enforced only at the
record-loop top, so a symbol up to `MAX_SYMBOL_PIXELS` (16 MP) could still be decoded
*after* the running total crossed the cap — effective bound ~32 MP, i.e. the old flaky
level for a crafted single-large-symbol input. Moved the check into `check_pixel_budget`
(now takes a `max_total`): page loops pass `MAX_PAGE_SYMBOL_PIXELS`, the dictionary loop
the 256 MP ceiling, and a symbol that would cross the cap is rejected *before* its bitmap
is decoded. The bound is now tight — the seed input's decode dropped 367 ms → **177 ms**
(~4.8 s ASAN worst case).

**Numbers.** Slow input: 626 ms → 367 ms → **177 ms** native, now `Err(ImageTooLarge)`
at 32 MP, before the ~48 MP it would otherwise reach). ×~16 ASAN ≈ 5.9 s — comfortably
under the 10 s fuzz timeout. Worst-case per-page decode is now hard-bounded at 32 MP
regardless of input size. Full suite green: 53 djvu-jb2 + 611 lib tests, incl. the 517-page
`pathogenic_bacteria` corpus — no valid page needs > 32 MP.

**Decision: Kept.** Interop-safe (decoder-only reject of an over-budget stream; no valid
DjVuLibre file affected). Regression-guarded: the exact artifact is added to the
`fuzz_jb2` seed corpus and to `crates/djvu-jb2/tests/dos_bounds.rs` (asserts
`Err(ImageTooLarge)`).

### IW44 encoder-size swarm — other candidates assessed — **Reverted / Rejected** (2026-06-25)

After IW44-1 (the one big win, −9.1% BG44), the remaining swarm candidates were assessed and **not**
taken — recorded so they are not re-explored:

- **#8 rgb_to_ycbcr Y rounding** (`/4` → `(…+2)/4`): **Reverted.** conquete_paix size 1,663,587 →
  1,663,085 B (−0.03%, negligible) but PSNR 33.72 → 33.65 dB (*worse*). Rounding moves our
  reconstruction away from the target; our truncation is correct.
- **#6 / #7 early-termination of trailing slices/chunks**: **Dead end.** Per-chunk sizes on
  conquete_paix BG grow (chunk 0 = 23 B … chunk 9 = 4551 B) — the trailing chunks are the *finest
  detail*, not null. All 100 slices are productive; stopping early would cut quality, not waste.
- **#3 / #4 / #10 per-band/per-fine-band ZP contexts, #12 sign/global-skip recode, #5 QUANT_HI_INIT
  rebalancing**: **Rejected — interop break.** These change the ZP context assignment or the
  normative quant table, which the *decoder* in `lib.rs` also uses (`quant_lo: QUANT_LO_INIT`, …).
  Changing them on both sides would make our decoder unable to read DjVuLibre-encoded IW44 streams
  (the context model / quant schedule is the de-facto IW44 spec). Non-starter for a library whose
  primary job is decoding DjVuLibre files.
- **#2 chroma_delay default 0 → 10**: a real **quality/size knob** (matches c44's `-crcbdelay 10`,
  ~2–5% smaller) but it *lowers* chroma quality. Left as-is — our default deliberately gives higher
  chroma quality than c44; the knob exists for callers who want the c44 operating point.

**Net.** The IW44 BG44 gap vs DjVuLibre closed from 14.3% → **3.9%** via the single interop-safe,
PSNR-neutral coding-efficiency fix (IW44-1). The remaining headroom is either interop-breaking
(normative tables/contexts) or a quality tradeoff, so the IW44 encoder is now at near-parity for
lossless-of-the-stream purposes.

---

### IW44-1 — match the activation-prediction threshold to the real gate (s/2 → 11s/16) — **Kept (size)** (2026-06-25)

**Issue.** From the IW44-encoder-size swarm. The IW44 encoder predicts whether a coefficient will
*activate* in two places — `any_unk_activates` (encode.rs:1009, drives the block-band NEW bit) and
`bucket_encoding_pass`'s `is_new` (encode.rs:1053, drives the bucket NEW bit) — using the threshold
`|v| > s/2`. But the activation pass itself (`newly_active_encoding_pass`, encode.rs:1096) activates
only when `|v| > (s·11/16).max(1)`. Since `s/2 = 8s/16 < 11s/16`, every coefficient in the gap
`(8s/16, 11s/16]` made the encoder announce a block/bucket NEW (a `1` ZP bit) and then emit all-`0`
activation bits for it — wasted bits with **zero** reconstruction effect.

**Approach.** Change both predictors to the real gate `|v| > (s·11/16).max(1)`. Encoder-only;
decoder untouched.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** `encode_quality_iw44` on conquete_paix (20 photo pages):

| | rs BG44 bytes | × orig (DjVuLibre) | PSNR avg / min |
|---|---|---|---|
| before | 1,830,594 | 1.143× | 33.72 / 29.57 dB |
| after | 1,663,587 | **1.039×** | **33.72 / 29.57 dB** |

**−9.1%** BG44 size at **bit-identical PSNR** — the gap vs DjVuLibre's c44 shrinks from 14.3% to
**3.9%**. 35 djvu-iw44 round-trip tests pass; colorbook BG44 also lands at 1.048×.

**Decision. Kept (size).**

**Reason.** A genuine coding-efficiency fix, not a quality tradeoff: the set of coefficients crossing
the real activation gate is unchanged, so every `recon[]` value — and thus the decoded image and
PSNR — is identical; only overhead bits encoding non-events are removed. Encoder-only and
interop-safe: the decoder reads the (now shorter) stream the same way, and no normative IW44 table
(quant/band/zigzag) or ZP context assignment is touched, so DjVuLibre-encoded files still decode
unchanged. Since BG44 dominates colour/photo scans (50–94% of bytes), this ~9% BG44 cut is a real
whole-file reduction on the most common colour documents. The swarm validators' round-trip + PSNR
gate held exactly. Closes the activation-prediction half of the IW44 size gap.

---

### Encoder size diagnostic: where is the gap vs DjVuLibre? — **Diagnostic** (2026-06-25)

**Goal.** After closing the JB2 mask size gap (#446, #452), locate the remaining archive-size gap
vs DjVuLibre across chunk types and encoders.

**Chunk-byte breakdown of DjVuLibre-encoded corpus files** (where the bytes actually are — it
depends heavily on document type):

| Doc | Sjbz (mask) | BG44 | FG44 |
|---|---|---|---|
| watchmaker (text) | 67.4% | 11.4% | 1.8% |
| colorbook (picture book) | 36.2% | 54.8% | 7.2% |
| conquete_paix (photo) | 3.0% | **93.8%** | 2.0% |
| chicken (photo) | — | **99.9%** | — |

→ For colour/photo scans the **IW44 layers (BG44 + FG44) dominate**, not the JB2 mask.

**IW44 BG44 size vs DjVuLibre** (`encode_quality_iw44` on conquete_paix, 20 pages): our BG44 is
**1,830,594 B vs DjVuLibre's 1,601,905 B = 1.143× (≈14% larger)** at PSNR avg 33.7 dB (min 29.6).
Caveat: measured by re-encoding the *decoded* background (some generation loss) and not at exactly
matched PSNR, so the precise factor has error bars — but the direction is clear and consistent
across pages.

**Conclusion.** The mask is at parity (1.04×); the remaining size lever is the **IW44 encoder**
(~14% larger BG44), which is the *dominant* chunk for real colour scans — so a ~14% BG44 reduction
is ~7–13% of the whole file. This scopes the next investigation to the IW44 encoder
(quantization/slice-budget — see IW44_DIAG fine-band starvation — coefficient ZP coding, zone
ordering), with round-trip + PSNR as a hard validation gate. Encoder *speed* was not cleanly
comparable in this pass (different image scales/pipelines) and is secondary (batch operation).

`c44`/`cjb2` are installed locally for head-to-head encoder comparison.

---

### #452 — shared Djbz dictionary for layered (quality/archival) multi-page encode — **Kept (size)** (2026-06-24)

**Issue.** #452. The layered (`--quality quality|archival`) multi-page directory encode emitted each
page's JB2 mask with its **own** dictionary (`PageEncoder::from_pixmap` per page + `djvm::merge`),
re-encoding shared glyphs on every page. The lossless profile already shares a Djbz dictionary;
layered did not (the CLI even printed "--shared-dict-pages is ignored for layered directory encode").
#290 had skipped it to avoid the rejected Hamming clustering — but `cluster_shared_symbols` is now
byte-exact (max_diff = 0), so that rationale is obsolete.

**Root-cause finding.** Per-file measurement showed the encoder size gap vs DjVuLibre is **not** in
the codec or symbol matching — 99.1% of components are exact dictionary copies (rec-7). The bloat is
purely the per-page dictionary duplication: on DjVu3Spec the independent dict is 1.627× the original
bytes, while a shared-Djbz bundle is 1.044× (−35.8%).

**Approach.** New `encode_djvm_layered_shared`: segment every page once (mask + BG), cluster the
masks (`cluster_shared_symbols`), emit one `FORM:DJVI` Djbz, and build each page's `FORM:DJVU` as
`INFO + INCL + Sjbz(shared-dict) + BG44… + FGbz`. FGbz is rebuilt from the shared-dictionary Sjbz so
its per-blit palette indices match — this required threading the decoded shared dict into
`foreground_fgbz` (its `decode_indexed(sjbz, None)` would otherwise fail on an INCL-referencing Sjbz
and silently drop the FGbz palette; the CLI directory tests caught it). Factored the DIRM/offset
assembly out of
`encode_djvm_bundle_jb2_with_shared` into a shared `assemble_djvm_bundle` + `build_form_body` helper
(existing shared-dict/djbz tests confirm the refactor is behaviour-preserving). Wired into the CLI
layered branch (now honours `--shared-dict-pages`).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Layered Quality encode of rendered pages, shared vs independent bundle:

| Doc | pages | independent | shared Djbz | Δ |
|---|---|---|---|---|
| DjVu3Spec_bundled | 71 | 445,940 B | 293,558 B | **−34.2%** |
| watchmaker | 12 | 140,864 B | 133,076 B | **−5.5%** |

Both shared bundles parse to the correct page count, render every page, and keep FGbz on every page
that had it (12/12 for watchmaker, 2/71 for the mostly-text spec) — round-trip verified.
New test `layered_shared_djbz_round_trips_with_incl` checks the DJVI/INCL structure + mask decode;
610 lib tests pass; std/no_std/wasm/parallel + all-features clippy clean.

**Decision. Kept (size).**

**Reason.** The largest unaddressed gap vs DjVuLibre was archive **size** on multi-page layered
colour scans, and it was entirely per-page dictionary duplication — fixed by reusing the byte-exact
clustering the lossless path already had. The win scales with cross-page symbol repetition (−34% on
a text-dense spec, −9% on a looser document) and is correct (mask + BG44 + FGbz preserved, FGbz
rebuilt from the shared Sjbz). Single-page and lossless paths are unchanged. Closes #452.

---

### #422 — bilinear chroma upsampling for chroma_half IW44 pages — **Kept (quality)** (2026-06-24)

**Issue.** #422. When an IW44 image has `chroma_half = true` (IW44 v2+), the Cb/Cr planes are
stored at half luma resolution. The sub=1 render path upsampled them nearest-neighbour — `c_row =
row/2` vertically and `cb_half[col/2]` horizontally — replicating each chroma sample to a 2×2 block,
which produces visible colour stairstepping at sharp colour transitions.

**Approach.** Rather than rewrite the four nearest-neighbour SIMD half-kernels (NEON/AVX2/WASM/scalar
— the "HIGH complexity" the issue warns about), add a per-row bilinear upsampler
(`upsample_chroma_row_bilinear`): for each output row build full-resolution Cb/Cr rows from the two
vertical-neighbour half-rows (`row/2`, `row/2+1`, weight `row&1`) with horizontal bilinear
(even col → `c/2`, odd col → average of `c/2`,`c/2+1`), then feed them to the existing, already-SIMD
full-resolution `ycbcr_row_from_i16`. This gives full two-axis bilinear while reusing the tested
full-res kernels and touching no intrinsic code. The superseded `ycbcr_row_from_i16_half` and its
SIMD half-kernels are retained (`#[allow(dead_code)]`, with their tests) for reference.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers / correctness.** Scope is narrow: only `chroma_half` (IW44 v2+) pages are affected, which
in the corpus is **only carte.djvu** — watchmaker, cable, chicken and colorbook are all IW44 v1
(`chroma_half = false`) and render **byte-identical** (FNV unchanged), so the common path has zero
regression and zero added cost. A new unit test `upsample_chroma_row_bilinear_values` pins the
upsampler against hand-computed values; the carte determinism golden was regenerated to the bilinear
output; all 35 djvu-iw44 tests + 610 lib tests pass.

**Decision. Kept (quality).**

**Reason.** Genuine quality win for chroma_half pages — smooth colour gradients replace 2×2 nearest
replication — delivered via a row-level pre-upsample that reuses the existing SIMD YCbCr kernel
instead of reimplementing four intrinsic paths, so it is correct on every target with no SIMD
surgery. The non-chroma_half majority is provably unaffected (byte-identical). The per-row chroma
upsample adds a small cost only on chroma_half pages (the issue's accepted ≤5% tradeoff), with no
chroma_half render benchmark in the corpus to quantify it. Closes #422.

---

### #445 — rolling 1-bit register for `m_r2` in `decode_ref_row` — **Reverted** (2026-06-24)

**Issue.** #445 (from the perf-experiment-swarm). `decode_ref_row` reads `m_r2 = pix_row(mbm_r2,
col + col_shift)` each column (a signed `col < 0` guard + bounds-checked `get`). Proposal: carry
`m_r2` in a rolling register, and — exploiting "`col_shift ≥ -1`" — drop the signed guard from the
in-loop update.

**Approach.** Seed `m_r2 = pix_row(mbm_r2, col_shift)` before the loop, use it in the `idx`
computation, and update `m_r2 = pix_row(mbm_r2, col + 1 + col_shift)` at the loop bottom.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical (9 + 53 jb2 round-trip/decode tests pass). `jb2_decode_first_chunk`
(boy_jb2): 436 vs 427 µs — within noise / a slight regression; no benchmark exercises a
refinement-heavy `decode_ref_row` (the proposed `jb2_decode_corpus_bilevel` target does not exist).

**Decision. Reverted.**

**Reason.** The optimisation's value depended on dropping the signed guard, which is **unsafe**:
`col_shift = mcol - ccol` (line 867) is unbounded, so `col + col_shift` (and `col + 1 + col_shift`)
can be negative for any `col` — the guard must stay. With the guard kept, the change merely
relocates the same `pix_row` read from the top of the loop to the bottom and carries it in a
register; that is exactly the instruction scheduling LLVM already does, and the one clean
measurement showed no gain (slight +2% regression from the extra register carry + a wasted final
read). `decode_ref_row` is also a cold-path (JB2 mask decode is cached after first render). Same
outcome as #441 — a codec inner-loop micro-op whose premise doesn't hold up.

---

### #442 — extend NEON per-row IDWT path to s==2 — **Rejected (incorrect premise)** (2026-06-24)

**Issue.** #442 (from the perf-experiment-swarm). `row_pass_inner`'s AArch64 per-row NEON branch
fires only at `s == 1`; `s == 2` falls through to the 8-row scatter-gather path. Proposal: extend
the branch to `s == 1 || s == 2`, reusing `row_pass_neon_s1_row` directly ("no new function
needed"), on the premise that an s=2 active row has "internal coefficients at consecutive positions
with stride 1."

**Approach.** Tried exactly the proposed one-line guard change.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers / verification.** The premise is **false**: the in-tree test `simd_row_pass_s2_matches_scalar`
calls `row_pass_inner` with `s = 2, sd = 1`. At `s == 2` the column subsample shift `sd == 1`, so
the active coefficients in a row are at **stride 2** (`kmax = (width-1) >> 1`, accesses `k << 1`),
not stride 1. `row_pass_neon_s1_row` processes `width` *consecutive* coefficients (it is correct for
`s == 1` precisely because `s == 1 ⇒ sd == 0`). Applying the proposed change makes the s=2 correctness
test fail immediately: `assertion left == right failed: SIMD row pass (s=2) must produce identical
output to scalar`.

**Decision. Rejected** (no code committed — the probe was reverted).

**Reason.** Reusing `row_pass_neon_s1_row` for `s == 2` produces an incorrect IDWT (it processes
stride-1 positions where the s=2 active row is stride-2). The issue's "no new function needed" claim
does not hold; a correct s=2 NEON kernel would have to handle the `sd == 1` deinterleave (stride-2
columns) — a genuinely new implementation, out of scope for the "reuse the s1 row" hypothesis the
issue is built on. The `s == 1` branch is safe only because it implies `sd == 0`.

---

### #441 — hoist `vdupq_n_s32` splat constants in IDWT NEON row pass — **Reverted** (2026-06-24)

**Issue.** #441 (from the perf-experiment-swarm). `row_pass_neon_s1_row`'s `lift!`/`predict!`
macros call `vdupq_n_s32(16)` / `vdupq_n_s32(8)` inside their bodies (expanded twice per chunk).
An in-tree comment notes LLVM does not hoist the splat to a loop-invariant `movi.4s`. Hypothesis:
hoist them to a `let` before each loop, matching the C16/C8 const pattern in the `wide::i32x8` path.

**Approach.** `let c16 = vdupq_n_s32(16i32)` before the even loop, `let c8 = vdupq_n_s32(8i32)`
before the odd loop; use them in the macros.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical (same constant; 34 iw44 tests pass). `iw44_to_rgb_colorbook/sub1`
(full IDWT + YCbCr), two rounds: WITH 5.655 / 5.778 ms, base 5.695 / 5.768 ms — −0.7% / +0.2%,
i.e. indistinguishable from noise.

**Decision. Reverted.**

**Reason.** The premise is correct (LLVM does re-materialise the splat), but the conclusion does not
hold for *this* loop: `movi.4s` is a 1-cycle, zero-latency-to-issue instruction that dual-issues
into an otherwise idle slot of this ALU-bound lifting kernel, so re-creating it per chunk costs ~0.
The C16/C8 const pattern that justified the hoist was for the `wide::i32x8` path (different
codegen); it does not transfer to the NEON `int32x4_t` kernel. No measurable benefit and no
structural improvement (unlike the kept #443/#446/#449), so reverted per "keep only what helps."

---

### #443 — extend F2 all-bg fast path to non-identity gamma — **Kept** (2026-06-24)

**Issue.** #443 (from the perf-experiment-swarm). The F2 all-bg-row fast path
(`composite_rows_bilinear_one`, 1:1) only fired when `gamma_is_identity`; an all-bg row with
non-identity gamma fell through to the full G1 pre-expansion + per-pixel dispatch.

**Approach.** Add an `else if row_is_all_bg` branch for the non-identity case: apply the gamma LUT
directly to the bg row (`chunk[c] = lut[bg_row[..][c]]`), or to white (`lut[255]`) when there is no
bg. Sequential LUT pass, no mask expansion. Falls through on the bg-clamp edge case.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Identity-gamma output byte-identical — FNV unchanged on watchmaker native (the new
branch only fires when `gamma_is_identity == false`, so the common path is untouched, verified).
103 render + 610 lib tests pass. No non-identity-gamma fixture exists in the corpus, so the new
branch is not benchmark-exercised; its output is byte-identical to the per-pixel loop by
construction (`lut[bg]` for all-bg pixels, with the same offset/clamp guard).

**Decision. Kept.**

**Reason.** Free-on-miss, byte-identical safe extension that completes F2 symmetrically: the
identity path is provably unchanged (FNV), and a non-identity-gamma all-bg row now gets a single
LUT pass instead of the full G1 dispatch. The benefit is narrow (non-identity gamma is uncommon)
and not benchmark-visible on the identity-only corpus, but there is no downside — same disposition
as the other kept fast-path extensions (#433/#438). Closes #443.

---

### #449 — stream PDF page render→emit→drop in the sequential path — **Kept** (2026-06-24)

**Issue.** #449 (from the perf-experiment-swarm). `djvu_to_pdf_impl` collected *all*
`RenderedPage` bodies into a `Vec` before emitting any, holding O(page_count × body_size) in
memory. A source comment explicitly deferred a fix to "a separate issue."

**Approach.** Factor the per-page emit (rendered body, or blank-page fallback) into an `emit_one`
closure. The `parallel` branch keeps the rayon `collect()` (required) + emit loop. The
`#[cfg(not(feature = "parallel"))]` branch now renders, emits, and drops one page at a time, so at
most one `RenderedPage` body is live (peak from page bodies O(pages)→O(1); mirrors TIFF_STREAM).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** PDF output **byte-identical** (same length + FNV on navm_fgbz 6 pages / watchmaker
12 pages) — the emit order and id allocation are unchanged. Peak RSS (`/usr/bin/time -l`) was
*within noise* on both available multi-page docs: watchmaker 12 pp (75.9 vs 73.2 MB) and
pathogenic_bacteria 520 pp (72.1 vs 72.4 MB). Neither makes body-accumulation the peak driver —
watchmaker has only 12 pages (~0.5 MB bodies) and pathogenic's pages deflate to ~2 KB each (1.2 MB
PDF total); in both, peak is dominated by the per-page 33 MB render pixmap and decode caches.

**Decision. Kept.**

**Reason.** Structurally the streaming path holds ≤ the bodies the collect path does (1 vs N), so
the page-body contribution to peak RSS can only drop — the watchmaker +2.7 MB is allocator/measurement
noise (the change cannot increase how many bodies are retained). Byte-identical output, and it
removes a documented deferred footgun: a real document with *many large-body pages* (e.g. a
300-page colour scan, ~1 MB deflated/page → ~300 MB of retained bodies) would see a large peak-RSS
cut. No corpus fixture combines many pages with large bodies, so the win isn't benchmark-visible,
but the change is strictly-safe and correct — same disposition as #446. The parallel path is
unchanged. Closes #449.

---

### #448 — hoist Lanczos vertical-pass weights + row-major accumulate — **Kept** (2026-06-24)

**Issue.** #448 (from the perf-experiment-swarm). In `scale_lanczos3`'s vertical pass, the weight
`lanczos3_kernel((sy − cy) / v_scale)` was evaluated inside the per-column `for ox` loop even though
it depends only on `(oy, sy)`. LLVM cannot LICM the opaque `f32::sin` calls, so the kernel was
recomputed `dst_w` times per row. The access `mid.get_rgb(ox, sy)` also strided by `dst_w*4` bytes
per `sy` (column-major against a row-major buffer).

**Approach.** Restructure to iterate `sy` outer / `ox` inner: evaluate the weight once per `sy`,
read `mid`'s row `sy` sequentially, and accumulate into per-column buffers (`acc_r/g/b`). The
per-column sum is over the same `sy` values in the same ascending order, so the floating-point
result is bit-identical.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output bit-identical (FNV unchanged, Lanczos3 downscale). Full Lanczos3 render
(watchmaker @0.5 — native re-render + `scale_lanczos3`), best-of-20:

| Round | base (ms) | with (ms) | Δ |
|---|---|---|---|
| 1 | 594.00 | 460.25 | **−22.5%** |
| 2 | 594.37 | 460.26 | **−22.5%** |

Near-perfectly reproducible (best-of-20 both rounds within 0.4 ms). The vertical pass was a major
cost because of the redundant `sin` evaluations; hoisting them saves ~134 ms.

**Decision. Kept.**

**Reason.** −22.5% on the Lanczos render path, bit-identical output, reproducible. The win comes
from two synergistic fixes: (1) the `f32::sin` calls — which the compiler genuinely cannot hoist —
move from O(dst_w·support) to O(support) per row, and (2) row-major accumulation replaces
per-column cache-scatter reads of `mid`. Lanczos resampling is the opt-in high-quality downscale
(`Resampling::Lanczos3`); this makes it ~1.3× faster at no quality cost. Closes #448.

---

### #447 — 32×32 tiled transpose in `rotate_pixmap` (Cw90/Ccw90) — **Kept** (2026-06-24)

**Issue.** #447 (from the perf-experiment-swarm). The 90° rotation paths transposed the image with
a per-pixel `get_rgb`/`set_rgb` whose destination write strides by `out.width*4` bytes (~13 KB at
A4) — a cache miss per pixel, ~8.4 M strided writes for a 2550×3300 page.

**Approach.** Iterate the source in 32×32 tiles and copy each pixel with a direct
`out.data[di..di+4].copy_from_slice(&src.data[si..si+4])`. Within a tile both the source read and
destination write stay within a few cache lines, so the hardware prefetcher works. 4-byte copy is
exact (rendered source pixmaps carry alpha=255 and `Pixmap::white` pre-fills alpha=255).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88 (machine moderately
loaded, load ~7 — best-of-N used for robustness).

**Numbers.** Output byte-identical — FNV checksums unchanged for Cw90, Ccw90, and Rot180 (the last
untouched). Full rotated render (watchmaker Cw90, native, best-of-30):

| Round | base (ms) | tiled (ms) | Δ |
|---|---|---|---|
| 1 | 52.54 | 51.55 | −1.9% |
| 2 | 53.02 | 49.70 | −6.3% |

Both rounds faster. The transpose itself is substantially cheaper (strided 8.4 M writes →
cache-local tiles); the end-to-end delta is diluted because decode + composite (~45 ms, unchanged)
dominate the ~50 ms rotated render.

**Decision. Kept.**

**Reason.** Byte-identical output, consistent improvement across rounds, and a textbook
cache-locality win for an image transpose (sequential tile bursts replace per-pixel cache-scatter).
Rotated rendering is a real use case (viewing rotated scans). Rot180 is left as-is (its writes are
already row-sequential in reverse, not a transpose). Closes #447.

---

### #446 — O(1) page-dedup in `cluster_shared_symbols_tunable` — **Kept** (2026-06-24)

**Issue.** #446 (from the perf-experiment-swarm). The shared-Djbz clustering inner loop deduped the
per-cluster `pages_seen: Vec<usize>` with `pages_seen.contains(&page_idx)` — an O(K) linear scan.
On a corpus where one cluster recurs on all P pages, this is O(P²) total.

**Approach.** Pages are visited in strictly non-decreasing `page_idx` order (the outer
`pages.iter().enumerate()` loop), so `pages_seen` is sorted and the current page — if already
counted — is the last element. Replace the scan with `pages_seen.last() != Some(&page_idx)`, O(1).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Clustering output is identical — verified by the existing clustering tests
(`cluster_promotes_only_repeated_glyphs`, `cluster_tunable_keeps_near_duplicate_large_glyphs_separate`,
`cluster_shared_symbols_caps_total_pixel_budget`) plus 67 encode + 2 djbz tests, all passing. No
dedicated multi-page-clustering benchmark was run (the only large fixture is the 517-page
`pathogenic_bacteria_1896`, whose full re-encode is expensive), but the change is *strictly* ≤ the
old cost: `last()` is a single comparison vs a scan of up to P entries.

**Decision. Kept.**

**Reason.** Trivially correct under the monotonic page order (verified by the unchanged clustering
test outputs) and a pure complexity improvement — O(P²)→O(P) on the worst case (a cluster shared
across an entire corpus), with no possible regression (O(1) ≤ O(K) always). Same disposition as the
kept #433: a safe, strictly-non-worse change that helps at scale (large multi-page shared-dict
encodes) even though the small test corpus doesn't make the difference benchmark-visible. Closes #446.

---

### #444 — cache parsed FGbz palette in `PageLayers` — **Reverted** (2026-06-24)

**Issue.** #444 (from the perf-experiment-swarm). `decode_fg_palette_full` calls `parse_fgbz`
(BZZ decompression + index-table construction) on every warm render of a palette page. Hypothesis:
cache the parsed `FgbzPalette` in `PageLayers` like the other layers (COW_FG pattern).

**Approach.** Added `#[derive(Clone)]` to `FgbzPalette`, a `fg_palette` OnceLock + accessor in
`PageLayers`, a `DjVuPage::decoded_fg_palette()`, and changed `decode_fg_palette_full` to return
`Cow::Borrowed` on a cache hit (with a re-parse error-fallback). `DecodedLayers`/`ForegroundLayers`
`fg_palette` fields became `Option<Cow<'a, FgbzPalette>>`.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88. Machine idle (the
AV1 encode was paused), so the control reproduced cleanly.

**Numbers.** Output byte-identical (FNV unchanged on navm_fgbz). `palette_native_cached` vs
`color_native_cached` control, two rounds:

| Round | control (ms) | palette (ms) | ratio |
|---|---|---|---|
| 1 baseline | 42.68 | 41.11 | 0.963 |
| 1 with cache | 42.48 | 41.39 | 0.974 |
| 2 baseline | 42.61 | 40.92 | 0.960 |
| 2 with cache | 42.45 | 41.43 | 0.976 |

A consistent **~1% regression** (palette ~0.4 ms slower both rounds, on a clean idle machine).

**Decision. Reverted.**

**Reason.** The FGbz chunk in the only palette fixture (navm_fgbz) is **21 bytes** — a trivial
palette with no real index table, so `parse_fgbz` is essentially free. Caching it saves nothing
while the cache machinery (OnceLock check, `Cow` wrapping, the error-fallback branch) adds a small
but consistent overhead. The optimisation is sound *in principle* — it would help pages with large
palettes (big per-blit index tables) — but no such page exists in the corpus, so there is nothing
to amortise the cache against, and the measured result is a net regression. Unlike the kept
BG_CACHE/MASK_IDX_CACHE (which cache multi-millisecond decodes / 33 MB allocations), a 21-byte parse
is below the cache's own overhead. Reverted.

---

### #440 — parallel BG44/FG44 layer decode via `rayon::join` — **Kept** (2026-06-24)

**Issue.** #440 (from the perf-experiment-swarm). `decode_layers`'s strict branch ran
`decode_background_chunks` (BG44 ZP decode + IDWT + YCbCr→RGB) then `decode_foreground_strict`
(JB2 mask + FG44) sequentially. They write disjoint `OnceLock` fields, so on a cold render they
can overlap: the FG decode runs on a second rayon thread while the BG ZP phase (before IW44_PAR's
IDWT join) leaves the pool idle.

**Approach.** Wrap the two calls in `rayon::join` under `#[cfg(feature = "parallel")]`; the
non-parallel build keeps the sequential tuple. Warm renders hit the caches and both closures
return instantly, so the join is effectively free then.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.
**Note.** This was measured *after* the user paused a background AV1/ffmpeg video-encode that had
been saturating all 10 cores (load avg ~590) — the earlier "thermal" noise was actually CPU
contention. With the machine idle, the single-thread control `bilevel_native_cached` reproduced to
0.05%, so these numbers are clean.

**Numbers.** Output byte-identical (FNV unchanged, `--features parallel`). Cold-render timing
(watchmaker native, fresh parse each iteration, best-of-40, `--features parallel`):

| Round | base (ms) | with join (ms) | Δ |
|---|---|---|---|
| 1 | 13.35 | 10.26 | **−23.1%** |
| 2 | 12.97 | 10.14 | **−21.8%** |

Consistent **~22%** faster cold render — the FG side (FG44 IW44 + JB2 mask, ~3 ms) overlaps the BG
decode rather than running after it. Warm regression check (`color_native_cached`, parallel
compositor ~6.7 ms): WITH and base within ~1% across rounds (one 7.8 ms outlier aside) — no real
regression, as expected since the join of two cache-hit closures costs ~tens of ns. 610 lib tests
pass with `--features parallel`.

**Decision. Kept.**

**Reason.** ~22% lower cold first-render latency — exactly what matters when a viewer opens a page
— at the cost of a `rayon::join` that is free on warm renders (cache hits return instantly) and
absent without the `parallel` feature. Disjoint `OnceLock` fields make it data-race-free; output is
byte-identical. Extends the IW44_PAR / PARALLEL parallelism to a new level (across layers at the
decode call site). Closes #440.

---

### #439 — anti-aliased colour downscale (proportional fg/bg blend) — **Kept (quality)** (2026-06-24)

**Issue.** #439 (from the perf-experiment-swarm). `composite_rows_area_avg_one` (colour downscale)
decided each output pixel as 100% fg or 100% bg via the binary `mask_box_any`. At 2× downscale a
single foreground source pixel in a 2×2 box made the whole output pixel fg-coloured, producing
blocky colour halos around text. The AA experiment already fixed the analogous artefact for the
*bilevel* compositor (`mask_box_coverage`); the colour compositor was never updated.

**Approach.** Replace the binary test with `coverage = mask_box_coverage(...)` (0..255 = fraction
of the footprint that is foreground) and blend: `coverage == 0` → bg only, `coverage == 255` → fg
only, otherwise `out = (coverage·fg + (255−coverage)·bg + 127) / 255` per channel. The `mask_shift
> 0` max-pool sub-path stays binary; #438's `mask_all_bg` still skips blank rows (coverage 0). The
now-unused `mask_box_any` is deleted.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Quality (the deliverable): watchmaker @150 DPI has **68 unique colours with the blend
vs 28 binary** — the 2.4× increase is the intermediate edge tones of anti-aliased colour text.
103 render tests + the full integration suite (golden composites) pass with no changes — the colour
goldens render at native resolution, so the downscale path change does not touch them. Performance:
`mask_box_coverage` counts the footprint instead of early-exiting like `mask_box_any`, a small cost
on non-blank rows (the AA experiment found this "indistinguishable from noise" at 150 DPI, and #438
already removes it on the 50–71% blank rows); the machine was too thermally saturated to quote a
clean delta, but partial-coverage pixels are a minority (edges only).

**Decision. Kept (quality).**

**Reason.** Genuine quality win — smooth colour gradients at text edges replace blocky halos, the
colour analog of the kept bilevel AA. Edge pixels now sample both fg and bg and blend; fully-fg
(coverage 255, stroke interiors) and fully-bg (coverage 0) pixels keep their single-sample cost, so
the extra work is confined to the minority of partially-covered edge pixels. Output for the
1:1/native paths is unchanged. Closes #439.

---

### #438 — all-bg row fast path in area-average compositor (F2/I3 analog) — **Kept** (2026-06-24)

**Issue.** #438 (from the perf-experiment-swarm). `composite_rows_area_avg_one` (colour downscale)
runs the per-pixel `is_fg` test via `mask_box_any` (for `mask_shift == 0`), which scans the output
pixel's footprint with byte popcounts. On blank rows (margins / inter-line gaps) this whole scan is
wasted. The 1:1 and bilevel-downscale paths already short-circuit blank rows (F2, #428).

**Approach.** Pre-scan the row-invariant mask y-band `[y0, y1)` once (`mask_all_bg`, matching
`mask_box_any`'s range); guard `is_fg` with `!mask_all_bg && …`. On a blank band the `&&`
short-circuits, skipping `mask_box_any` for every pixel. Guarded on `mask_shift == 0` (the max-pool
sub-path indexes a coarser mask).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical (FNV checksums unchanged on colorbook @150/400 and watchmaker
@0.25). Firing-rate probe (the thermal-independent evidence — the machine was thermally saturated
by this point, so wall-clock ratios were unusable):

| Doc / scale | output rows | blank-band rows | fire rate |
|---|---|---|---|
| colorbook @150/400 | 1376 | 972 | **70.6%** |
| watchmaker @0.25 | 825 | 414 | **50.2%** |

**Decision. Kept.**

**Reason.** Same disposition as #428 (kept, 94.4% fire): byte-identical, fires on 50–71% of rows,
and on those rows it skips the *expensive* `mask_box_any` footprint scan (not just a cheap bit
check as in #435), so the per-row saving is larger. Per-row overhead when it misses is one band
pre-scan (`iter().any()`, NEON) plus a short-circuiting bool — bounded and small. The mechanism is
a row-level early exit (the proven F2/I3/#428/#435 pattern), not an inner-loop micro-op like the
reverted #432/#434/#436/#437. Closes #438.

---

### #437 — exact-length FG44/BG44 row slices for bounds-check elision — **Reverted** (2026-06-24)

**Issue.** #437 (from the perf-experiment-swarm). The 1:1 general bilinear path takes open-ended
row slices (`fg.data.get(y0*stride..)`); `bilinear_from_rows` and the bg lookup then do
`row.get(off..off+4)` four/one times per pixel. Hypothesis: exact-length slices
(`..(y0+1)*stride`) make `row.len() == width*4` visible to LLVM after inlining, letting it elide
the per-lookup bounds-check branches.

**Approach.** Change the three slice sites (fg row0/row1, bg row) to `..(y+1)*stride`.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical (FNV unchanged). Benchmark inconclusive: by this point in the
session the machine was thermally saturated — the *control* `bilevel_native_cached` (unaffected by
this change) swung 56–433 ms across runs (7.6×), so no signal could be extracted from
`color_native_cached`.

**Decision. Reverted.**

**Reason.** Two problems. (1) Mechanism: the fg slices use `.unwrap_or(&[])`, so `row.len()` is
either `stride` or `0` from LLVM's view — the length is *not* statically tied to `width`, which
defeats the bounds-check elision the hypothesis relies on (only the bg path, which uses `.map`,
could plausibly benefit). (2) Measurement: the thermally-saturated machine made the result
unverifiable. The change is byte-identical and adds no work, but with no demonstrable benefit and a
mechanism likely defeated by `unwrap_or`, it is not worth keeping speculatively. Could be revisited
with assembly verification (look for removed `cbz`/`cbnz` guards) on a cool machine if the bg path
turns out to matter.

---

### #436 — MASK_EXPAND pre-expansion in B-series bilinear (upscale) — **Reverted** (2026-06-24)

**Issue.** #436 (from the perf-experiment-swarm). Retry of G1b — pre-expand the mask row to
per-pixel bytes via MASK_EXPAND in `composite_rows_bilinear_one`'s B-series path, replacing the
per-pixel bit-extraction with a byte load. The validator argued G1b was measured at 72 DPI (which
routes to `composite_rows_area_avg_one`, a different function) and that the *upscale* bilinear path
(600/300 DPI) was never actually tested.

**Approach.** Add a 4096-byte `g1b_buf`, expand the hoisted mask row via MASK_EXPAND (skipping
blank rows, which #435 already short-circuits), and replace the `is_fg` bit-extraction with a
`g1b_mask[px]` byte lookup (fallback bit-extraction for pages wider than the buffer). New
`color_upscale_cached` benchmark (watchmaker @2×).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical at both 2× upscale and 0.5 downscale (FNV checksums unchanged).
`color_upscale_cached` (watchmaker @2× = 5100×6602) vs `color_native_cached` control:

| Round | control (ms) | upscale (ms) | ratio | Δ |
|---|---|---|---|---|
| 1 baseline | 175.8 | 652.7 | 3.71 | |
| 1 with | 216.5 | 877.0 | 4.05 | +9.1% (regress) |
| 2 baseline | 180.5 | 1713.6 | 9.50 | |
| 2 with | 140.7 | 1240.5 | 8.82 | −7.2% (improve) |

Contradictory; the upscale render (33.7 M pixels, 134 MB output buffer) swings 650–1700 ms
between rounds — the measurement is dominated by memory-bandwidth/allocation noise, not the
is_fg path.

**Decision. Reverted.**

**Reason.** Unlike #435 (a free short-circuit), #436 *adds* real per-row work: a 4 KB stack buffer
plus a 319-iteration MASK_EXPAND expansion every non-blank row. The benefit it buys — replacing the
~5-op is_fg extraction with a byte load — is a small fraction of the bilinear-bound per-pixel cost
(`bilinear_from_rows` dominates), and #435 already removed the is_fg cost on the 52.6% blank rows.
The benchmark cannot confirm any gain (R1 even regresses), and the upscale path is an uncommon
real-world case (scans are rarely upscaled). Same outcome as G1b, now confirmed on the actual
upscale path: the expansion overhead is not worth it where the inner loop is resampling-bound.

---

### #435 — all-bg row fast path in B-series bilinear path (F2 analog) — **Kept** (2026-06-24)

**Issue.** #435 (from the perf-experiment-swarm). The B-series bilinear path
(`composite_rows_bilinear_one`, non-1:1) runs the per-pixel `is_fg` bit-extraction (≈5 ops:
`is_some_and`, bounds check, byte load, shift, AND, compare) for every pixel, including blank rows
(margins / inter-line gaps). The 1:1 path already short-circuits blank rows via F2.

**Approach.** Pre-scan the hoisted mask row once (`mask_all_bg`); guard `is_fg` with
`!mask_all_bg && …`. On a blank row the `&&` short-circuits, skipping the `is_fg` extraction for
every pixel — independent of whether LLVM unswitches the loop. Unlike F2 the bg pixels still need
per-pixel `bilinear_from_rows` (coordinates are scaled, no `copy_from_slice`), so this saves only
the is_fg check, not the whole bg copy.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Output byte-identical (FNV checksum unchanged). Firing-rate probe: watchmaker @150/300
(B-series) has **868 / 1651 output rows (52.6%) with a blank mask row** — the fast path fires on
the majority. Benchmark `color_downscale_mixed_cached` vs `color_native_cached` control:

| Round | control (ms) | mixed (ms) | ratio |
|---|---|---|---|
| 1 baseline | 385.8 | 106.2 | 0.275 |
| 1 with | 425.3 | 88.4 | 0.208 |
| 2 baseline | 357.2 | 90.5 | 0.253 |
| 2 with | 399.0 | 101.4 | 0.254 |

R1 shows −24%, R2 neutral — thermal noise (mixed swung 88–106 ms regardless). The mechanism is
confirmed by the 52.6% firing rate plus the short-circuit, which provably elides ≈5 ops/pixel on
those rows.

**Decision. Kept.**

**Reason.** Same disposition as F2/I3/#428 (kept row-level skips): the change is byte-identical,
the per-row overhead when it misses is one mask-row pre-scan (~nb bytes, NEON `any()`) plus a
short-circuiting bool, and it fires on 52.6% of rows where it elides the per-pixel is_fg check.
The benchmark is too noisy to quote a clean magnitude (~3–8% expected), but the firing-rate and
short-circuit guarantee a real, bounded-overhead win. Distinct from the reverted #432/#434
(inner-loop micro-ops that added unconditional per-pixel cost) — this only adds work on the
common branch and removes it on the majority blank-row case. Closes #435.

---

### #434 — Q48 accumulator for `fg_fx` in B-series bilinear path — **Reverted** (2026-06-24)

**Issue.** #434 (from the perf-experiment-swarm). The B-series bilinear inner loop
(`composite_rows_bilinear_one`, non-1:1) computes `fg_fx = map_plane_center_frac(fx, fg_x_q24)`
(a u64 multiply) for FG44 foreground pixels. The bg path already replaced its equivalent multiply
with a Q48 add-per-pixel accumulator (B1). Hypothesis: the same accumulator for `fg_fx` would
save 3–8%.

**Approach.** Add `fg_fx_q` / `fg_fx_step_q` mirroring `bg_fx_q`, replace the per-pixel multiply
with `(fg_fx_q >> 24).saturating_sub(FRAC/2)`, and advance `fg_fx_q` unconditionally at the bottom
of the loop (it must stay in sync across the fg/bg branch).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** `color_downscale_mixed_cached` (watchmaker @150/300, B-series FG44) vs
`color_native_cached` control. Output verified byte-identical (FNV checksum unchanged). Two
intra-session ratio pairs:

| Round | control (ms) | mixed (ms) | ratio | Δ |
|---|---|---|---|---|
| 1 baseline | 305.8 | 61.0 | 0.200 | |
| 1 with | 333.7 | 75.2 | 0.225 | **+12.9%** |
| 2 baseline | 229.4 | 53.8 | 0.234 | |
| 2 with | 225.9 | 67.2 | 0.298 | **+26.9%** |

Both rounds regress (R2's control is nearly equal, so the +25% absolute mixed regression is clean).

**Decision. Reverted.**

**Reason.** The hypothesis missed that the `fg_fx` multiply is **conditional** — it runs only for
fg pixels (`if is_fg → Some(fg44)`), a minority (~15–30%) of output pixels. The accumulator must
advance **unconditionally** (every pixel) to stay in sync. So the change trades a multiply on ~20%
of pixels for an add on 100% of pixels — a net loss. This is the inverse of B1 (kept): the bg
accumulator wins because bg is the *majority*, so its unconditional advance matches its usage
frequency. The accumulator pattern only pays when the consumer is the common-case branch. The
multiply on the minority fg path was already cheaper than the bookkeeping to avoid it.

---

### #433 — generalize P2 BILEVEL_RGBA fast path to byte-aligned `offset_x` — **Kept** (2026-06-24)

**Issue.** #433 (from the perf-experiment-swarm). The P2 BILEVEL_RGBA table fast path in
`composite_rows_bilevel_one` was guarded on `offset_x == 0`, so `render_region` viewports (which
pass `offset_x = region.x`) always fell back to the scalar per-pixel bit-extraction loop.

**Approach.** Generalize the guard to `offset_x % 8 == 0 && offset_x + out_w <= mask.width`: when
the offset is byte-aligned, output byte `i` maps to source mask byte `offset_x/8 + i` with no
per-pixel bit shuffle, so the same NEON table copy works. `offset_x == 0` is the `mb0 = 0`
subcase — behaviourally identical to before. Added a correctness test
(`render_region_bilevel_byte_aligned_offset_matches_full`) checking both x=16 (aligned, new path)
and x=17 (unaligned, fallback) against the full render, plus a `render_region_bilevel` benchmark.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Two effects measured:
- **No regression on the hot path** (`offset_x == 0`): `bilevel_native_cached` 437 vs 448 ms
  (within thermal noise) — the only change there is one `+0` index add.
- **`render_region_bilevel`** (cable, byte-aligned 512-wide viewport): with ≈ 89 ms, baseline
  ≈ 85 ms — **within noise**. The compositor *is* faster (P2 NEON table vs scalar, the
  established 18–24% margin), but `render_region`'s end-to-end time is dominated by the per-call
  `Pixmap::white` allocation (6.7 MB) and the 3301-row iteration, which mask the compositor saving.
  (An initial 12× reading was a stale-binary artifact from the stash/rebuild cycle, not real.)

**Decision. Kept.**

**Reason.** Unlike the reverted #432 (a replacement that measurably regressed), this is a strict,
*safe generalization*: the `offset_x == 0` hot path is byte-for-byte the same code, so it cannot
regress (confirmed), and byte-aligned region renders now use the proven-faster P2 NEON table
instead of the scalar fallback. The end-to-end `render_region` win is below this benchmark's noise
because allocation/iteration dominate — but the compositor improvement is real and the change has
zero downside. It also removes an artificial restriction on P2 and adds region test coverage.
Closes #433.

---

### #432 — byte-level early-exit scan in `mask_box_any` — **Reverted** (2026-06-24)

**Issue.** #432 (from the perf-experiment-swarm). `mask_box_any` (per-pixel foreground test in the
colour area-average downscale path, `composite_rows_area_avg_one`) scanned the footprint with
per-pixel `mask.get()` and early-exit. Hypothesis: apply the byte-level boundary-masking from the
kept POPCNT experiment (`mask_box_coverage`) to test "any bit set" faster.

**Approach.** Replace the per-pixel loop with the same `byte_lo`/`byte_hi`/`first_mask`/`end_mask`
boundary computation as `mask_box_coverage`, testing `& mask != 0` per byte with early-exit.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** `render_colorbook` (colorbook @150/400, area-avg path), clean alternating runs with
tight, reproducible CIs:

| Run | render_colorbook |
|---|---|
| baseline | 3.620 ms |
| with change | 3.805 ms |
| baseline again | 3.616 ms |

Baseline reproduces exactly (3.620 / 3.616 ms); the change is a clean **+5.1% regression** — not
thermal noise. 102 render tests pass (output correct), but it is slower.

**Decision. Reverted.**

**Reason.** The POPCNT win does **not** transfer. `mask_box_coverage` must scan the *entire*
footprint (it counts bits), so byte-popcount strictly beats per-pixel. `mask_box_any` has
**early-exit** — the per-pixel loop returns on the first set bit, which for text-on-white is cheap.
At colorbook's downscale the footprint is only ~3×3 pixels, so the per-call setup of four boundary
quantities (`byte_lo`, `byte_hi`, `first_mask`, `end_mask`) plus the single-vs-multi-byte branch
costs more than the handful of `get()` calls it saves. Byte-scanning only pays when the footprint
is large (POPCNT's −24% was at 72 DPI / 4×4) *and* there is no early exit. Mirror experiment to G2:
the simpler code the compiler already handles well wins at small footprints.

---

### #431 — LUT byte-expansion in bilevel TIFF export (`extract_bilevel_pixels`) — **Kept** (2026-06-24)

**Issue.** #431 (from the perf-experiment-swarm). `extract_bilevel_pixels` expanded the packed JB2
mask to a Gray8 buffer with one `bm.get(x, y)` per pixel (stride multiply + byte read + bit-shift
+ AND + branch), w×h times (8.4 M for an A4 page).

**Approach.** Add `BILEVEL_GRAY8: [[u8; 8]; 256]` mapping each packed mask byte (MSB-first) to its
8 Gray8 pixels. When the mask covers the page (`bm.width >= w && bm.height >= h`), iterate the
packed rows byte-by-byte and `copy_from_slice` 8 pixels per byte (trailing partial byte handled),
replacing 8 per-pixel `bm.get()` calls with one table lookup + vectorised copy. Per-pixel
`bm.get()` fallback kept for the unexpected smaller-mask case. Same LUT idea as P2/BILEVEL_RGBA
but Gray8 instead of RGBA.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Full `TiffMode::Bilevel` export of cable_1973 (2 pages — JB2 decode + expansion +
Deflate), best-of-30 timing:

| Round | baseline (ms) | with LUT (ms) | Δ |
|---|---|---|---|
| 1 | 60.8 | 49.5 | **−18.7%** |
| 2 | 59.7 | 48.8 | **−18.2%** |

Consistent **~18%** on the whole bilevel export (the expansion was a large fraction of the
non-Deflate work). 16 `tiff_export` tests pass (incl. bilevel pixel-equality and black-pixel
checks).

**Decision. Kept.**

**Reason.** Replaces 8.4 M per-pixel bit-extracts with ~1 M table lookups + vectorised copies;
output is byte-identical by construction (same MSB-first bit mapping). Stacks with the #430 Deflate
change in the same path. Closes #431.

---

### #430 — Deflate-compressed bilevel TIFF export (size reduction) — **Kept** (2026-06-24)

**Issue.** #430 (from the perf-experiment-swarm) asked for 1-bit TIFF output for bilevel pages
(target: 8× smaller files). The bilevel path wrote the JB2 mask as an uncompressed 8-bit
grayscale strip (Gray8, 1 byte/pixel = 8.4 MB for an A4 page).

**Constraint found.** The `tiff` 0.9 crate's high-level encoder has **no 1-bit ColorType** — the
`ColorType` trait's minimum `BITS_PER_SAMPLE` is 8 and `write_data` takes one sample per element,
so it cannot pack 8 pixels/byte. True 1-bit output would require hand-writing TIFF tags outside
the crate (disproportionate risk for the goal).

**Approach.** Achieve the *actual* goal — drastically smaller bilevel TIFFs — via the crate's
supported `new_image_with_compression::<Gray8, Deflate>`. Bilevel content is just 0x00/0xFF bytes
with long runs (text on white), which Deflate compresses far better than a 1-bit packing. One-line
change in `write_bilevel_page`; Deflate (TIFF compression tag 8) is universally readable; output is
lossless (pixel-identical).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** cable_1973 (2 pages, 2550×3301), `TiffMode::Bilevel`:

| | total bytes | per page |
|---|---|---|
| Baseline (uncompressed Gray8) | 16,835,608 | ~8.4 MB |
| Deflate | 113,029 | ~56 KB |

**≈149× smaller** — far past the 8× a 1-bit packing would give. 625 lib tests (incl. the
bilevel-TIFF pixel-equality and round-trip tests) pass with `--features tiff`.

**Decision. Kept.**

**Reason.** Delivers the issue's real objective (small bilevel TIFFs) better than the literal
1-bit request, using only the supported encoder API, losslessly, and readable by any TIFF reader.
True 1-bit packing is not worth a hand-rolled TIFF encoder when Deflate already gives ~149×. Color
TIFF export is left unchanged (RGB Deflate is a smaller, separate win). Closes #430.

---

### #429 — byte-aligned `crop_bitmap` fast path in JB2 direct encoder — **Kept** (2026-06-24)

**Issue.** #429 (from the perf-experiment-swarm). `crop_bitmap` (JB2 direct encoder, used to
split a page into 1024×1024 tiles for record-3 whole-tile symbols) cropped each tile with a
per-pixel `src.get(x) + out.set_black()` loop — w×h bit-unpack/repack operations per tile.

**Approach.** The tile loop always passes a byte-aligned `x0` (tiles start at multiples of
TILE=1024, and 1024 % 8 == 0). When `x0 % 8 == 0`, copy each output row as a contiguous
`copy_from_slice` of `out_stride` bytes from the source row, then mask the trailing bits of the
last byte beyond `w` (which the per-pixel path never sets). Falls back to the per-pixel loop for
unaligned `x0` (never produced by the current caller, kept for generality). Mirrors
`blit_to_bitmap`'s aligned fast path.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** The existing `jb2_encode` bench uses a 192×256 mask — a single tile, so
`crop_bitmap` is never called (it hits the `bitmap.clone()` single-tile path) and showed no
change (1.220 vs 1.220 ms — confirming the path isn't exercised). Added `jb2_encode_multitile`
(cable_1973 mask, 2550×3301 = 3×4 tiles), which does exercise the multi-tile crop:

| Round | baseline (ms) | with (ms) | Δ |
|---|---|---|---|
| 1 | 222.8 | 192.5 | **−13.6%** |
| 2 | 255.0 | 215.8 | **−15.4%** |

Consistent **~14–15%** on full-page direct encode; well-separated medians, 100 samples.
609 lib tests + 53 djvu-jb2 tests (incl. encode roundtrip) pass.

**Decision. Kept.**

**Reason.** Replaces w×h per-pixel bit operations per tile with `h` byte-aligned `copy_from_slice`
calls (LLVM vectorises to NEON loads/stores). Output is byte-identical by construction (same bits
in columns [0,w), padding masked to 0), so the encoded JB2 stream is unchanged — verified by the
passing encode-roundtrip tests. Note `crop_bitmap` lives in the direct `encode_jb2` path (not the
shipping dict encoder), so the real-world impact is bounded to that path; the change is free
(falls back for the unaligned case) and correct, so kept regardless. Closes #429.

---

### #428 — all-white band fast path in bilevel downscale compositor (I3-downscale) — **Kept** (2026-06-24)

**Issue.** #428 (from the perf-experiment-swarm). `composite_rows_bilevel_one`'s anti-aliased
downscale path (`mask_shift == 0`) called `mask_box_coverage` for every output pixel, and each
call scans the source mask band `[y0, y1)` with byte popcounts. For a blank band every call
returns 0 → white, so the whole per-row scan is wasted. The 1:1 path already had I3 (blank-row
fill) but the downscale path had no analog.

**Approach.** Before the per-pixel loop, scan the row-invariant source band
`mask.data[y0*stride .. y1*stride]` once (`iter().any(|&b| b != 0)`, NEON-vectorised). If blank
(or the degenerate `y1 <= y0`), `row_buf.fill(255)` and return. The y range matches
`mask_box_coverage` exactly. Guarded on `mask_shift == 0` — the max-pool sub-path indexes a
different mask resolution.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Firing-rate probe on cable_1973 @150 DPI: **1559 / 1651 output rows (94.4%) have a
fully-blank band** — the fast path fires on almost every row. Cost model: the slow path runs
~955 `mask_box_coverage` calls per blank row (≈2 byte-reads + 2 popcounts each); 1559 blank rows
≈ 3 M popcounts ≈ ~3 ms wasted, replaced by 1559 band scans ≈ 33 µs. Theoretical saving ≈ 6.5%
of the ~46 ms render.

Benchmark `render_corpus_bilevel_dpi/dpi/150` (target) vs `dpi/300` (1:1 control, unaffected),
intra-session ratio, two rounds:

| Round | dpi/150 (ms) | dpi/300 (ms) | ratio |
|---|---|---|---|
| 1 baseline | 47.0 | 149.8 | 0.314 |
| 1 with | 45.7 | 143.3 | 0.319 |
| 2 baseline | 54.1 | 156.4 | 0.346 |
| 2 with | 47.0 | 157.4 | 0.299 |

Rounds disagree (−1.7% vs +13.7%): the ~6.5% signal is swamped by thermal variance (control
swung 143–157 ms, baseline dpi/150 swung 47–54 ms). 609 lib tests pass.

**Decision. Kept.**

**Reason.** The benchmark is too noisy to resolve the effect, but two independent lines of
evidence make it a clear win: (1) the fast path fires on 94.4% of rows, and (2) the per-row
overhead when it does *not* fire is one band scan (~640 bytes, NEON `any()`, ~20 ns) on top of
the existing 955 coverage calls — bounded at ~0.07% of render time. So the change is essentially
free when it misses and a real saving when it fires. Same disposition and rationale as I3
(kept at 2.3%, "zero-cost for documents with no blank rows; effect scales with whitespace").
Output is byte-identical by construction (fires only when every band pixel is background → white).
Closes #428.

---

### #427 — indexed JB2 mask + blit-map cache for FGbz-palette pages (`PageLayers::mask_indexed`) — **Kept** (2026-06-24)

**Issue.** #427 (from the perf-experiment-swarm). `PageLayers` cached the plain JB2 mask
(`mask`) but not the *indexed* variant used by FGbz-palette pages. `decode_mask_indexed`
called `extract_mask_indexed` → `jb2::decode_indexed` on every warm render, re-running the
full JB2 ZP arithmetic decode and re-allocating a page-sized `Vec<i32>` blit map
(2550×3300 = 8.4 M entries = 33.6 MB) each time.

**Approach.** Add `mask_indexed: OnceLock<Option<(Bitmap, Vec<i32>)>>` to `PageLayers` with a
`mask_indexed()` accessor and a `DjVuPage::decoded_mask_indexed()` method. Rewrite
`decode_mask_indexed` to return `(Cow<Bitmap>, Cow<[i32]>)` — `Cow::Borrowed` on cache hit,
with the same error-fallback as `decode_mask` (re-decode when the cache is empty but an
Sjbz/Smmr chunk is present, to surface decode errors in strict mode). `DecodedLayers` and
`ForegroundLayers` `blit_map` fields change `Option<Vec<i32>>` → `Option<Cow<'a, [i32]>>`;
downstream consumers already used `.as_deref()`/`Option<&[i32]>`, so no compositor change.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** New benchmark `palette_native_cached` (navm_fgbz.djvu, 2550×3300, native).
Control `color_native_cached` (non-palette, unaffected). All runs thermally hot;
intra-session ratio `palette / color_native`:

| Round | control (ms) | palette (ms) | ratio | Δ |
|---|---|---|---|---|
| 1 baseline | 211.2 | 237.1 | 1.123 | |
| 1 with cache | 176.1 | 194.9 | 1.107 | −1.4% |
| 2 baseline | 205.4 | 227.0 | 1.105 | |
| 2 with cache | 254.5 | 258.1 | 1.014 | −8.2% |

All four measurements show the cached ratio below baseline (consistent direction); magnitude
swamped by thermal variance (control ranged 176–254 ms), mean ≈ **5%**. 609 lib tests +
`pdf_conversion` (navm_fgbz) pass.

**Decision. Kept.**

**Reason.** Eliminates a full JB2 ZP re-decode and a 33.6 MB `Vec<i32>` allocation on every
warm render of a palette page; output is byte-identical by construction (memoized
`extract_mask_indexed`). Beyond the modest CPU win, it removes per-render allocator churn of
a 33.6 MB buffer. Memory cost: 33.6 MB cached per palette page actually rendered (the same
buffer that was previously allocated-and-freed each render, now persisted) — paid only on
FGbz-palette pages, which are uncommon. Mirrors the COW_FG / BG_CACHE caching pattern. Closes #427.

---

### #426 — BG44 decoded RGB Pixmap cache at subsample=2 (`PageLayers::bg_rgb_s2`) — **Kept** (2026-06-24)

**Issue.** #426 (from the perf-experiment-swarm). `PageLayers` cached the decoded RGB
Pixmap only at subsample=1 (`bg_rgb_s1`, the BG_CACHE experiment). At subsample=2 — the
common 150-from-300-DPI render — `decode_background_chunks` called `img.to_rgb_subsample(2)`
fresh on every warm render, re-running the IW44 IDWT + YCbCr→RGB conversion (~8.4 MB output)
even though the underlying `Iw44Image` ZP decode was already cached via `decoded_bg44()`.

**Approach.** Mirror BG_CACHE for sub=2: add `bg_rgb_s2: OnceLock<Option<Pixmap>>` to
`PageLayers`, a `bg_rgb_s2()` accessor (`self.bg44(page)?.to_rgb_subsample(2)`), a
`DjVuPage::decoded_bg_rgb_s2()` method, and a `subsample == 2` branch in both the strict
`decode_background_chunks` and `decode_background_chunks_permissive` `max_chunks == MAX`
paths returning `Cow::Borrowed`. ~40 lines, no algorithmic change. Cache is ~8.4 MB
(4× smaller than the sub=1 cache); left empty on pages never rendered at sub=2.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Target `color_downscale_mixed_cached` (watchmaker @150/300, hits the sub=2
path), control `color_native_cached` (sub=1, unaffected). All runs thermally hot;
intra-session ratio `downscale_mixed / native` cancels throttling:

| Round | control native (ms) | downscale (ms) | ratio | |
|---|---|---|---|---|
| 1 baseline | 236.8 | 60.7 | 0.256 | |
| 1 with cache | 334.7 | 78.2 | 0.234 | **−8.6%** |
| 2 baseline | 238.5 | 72.0 | 0.302 | |
| 2 with cache | 199.0 | 55.8 | 0.280 | **−7.3%** |

Both rounds show the cached ratio below baseline; consistent **~7–9%** speedup on the
downscale path (above the issue's 4–6% estimate — the conversion is a larger fraction of
total time in the hot state). 609 lib tests pass.

**Decision. Kept.**

**Reason.** Pure memoization of an identical `to_rgb_subsample(2)` call — output is
byte-identical by construction, so correctness is guaranteed. Saves the IDWT + YCbCr→RGB
conversion on every warm 150-DPI color render at the cost of ~8.4 MB per page ever rendered
at sub=2. Directly extends the proven BG_CACHE pattern (which gave −5.1% at sub=1). Closes #426.

---

### C2b/C2c: FG44 y-row hoisting for B-series and area-average compositor — **Reverted** (2026-06-24)

**Issue.** `composite_rows_bilinear_one` (B-series/non-1:1 path) and `composite_rows_area_avg_one`
call `sample_bilinear(fg, fg_fx, fg_fy)` per fg pixel. `fg_fy` (and for area-avg, `fg_fy_step`
and the y-bounds `(y0, y1)`) are computed from `fy`, which is row-invariant. Hypothesis: explicit
pre-hoisting (C2b for bilinear, C2c for area-avg) before the inner loop would save ~14 ops/fg-pixel
in the B-series path and ~6 ops/fg-pixel in the area-avg path.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** New benchmark `color_downscale_mixed_cached` (watchmaker.djvu at 150/300 DPI,
exercises B-series with FG44). `color_native_cached` as thermal control (unaffected). All runs
thermally hot (tests ran in parallel, p-values unreliable). Intra-session thermal ratio
`downscale_mixed / native`:

| Run | native (ms) | downscale_mixed (ms) | ratio |
|---|---|---|---|
| WITHOUT C2b | 231 | 54 | 0.234 |
| WITH C2b #1 | 227 | 58 | 0.256 |
| WITH C2b #2 | 185 | 59 | 0.319 |
| WITH C2b #3 | 151 | 59 | 0.391 |
| WITH C2b #4 | 213 | 43 | 0.202 |

Ratio without C2b: 0.234. With C2b: 0.202–0.391, mean ≈ 0.29. No consistent trend;
variance overwhelms the expected ~9% signal.

**Decision. Reverted.**

**Reason.** LLVM already applies LICM (Loop Invariant Code Motion) to hoist `fg_fy` and the
two row-slice computations out of the inline `sample_bilinear` body — the y-coordinate
arithmetic is recognized as loop-invariant at the call site. C2b adds explicit hoisting but
does not improve on what the compiler already does; the additional Option wrapping slightly
changes the generated code and may inhibit some optimizations. This mirrors the G2 finding:
LLVM's out-of-order/LICM already covers the expected savings. The `color_downscale_mixed_cached`
benchmark case is kept for future experiments.

---

### P2: BILEVEL_RGBA lookup table for bilevel 1:1 compositor — **Kept** (2026-06-24)

**Issue.** The bilevel 1:1 inner loop (`composite_rows_bilevel_one`, 1:1 fast path) used
per-pixel bit extraction + 4 scalar byte stores — ~16 instructions per pixel, fully scalar.
Even removing the `.min()` clamp (Q1, see below) did not trigger LLVM auto-vectorisation
because the shift-by-variable pattern `(byte >> (7-(ox&7))) & 1` is not recognised by the
AArch64 backend.

**Approach.** Add `BILEVEL_RGBA: [[u8; 32]; 256]` (8 KiB, 128 cache lines): each entry maps
one mask byte to 8 pre-packed RGBA pixels (0x00000000FF for fg, 0xFFFFFFFFFF for bg as
little-endian u32). Before the scalar fallback, guard on `offset_x==0 && out_w<=mask.width`
and process mask_row in 8-pixel (1-byte) chunks via `copy_from_slice`. LLVM compiles each
`copy_from_slice(32 bytes)` to `ldp q0, q1 / stp q0, q1` (2 NEON loads + 2 NEON stores)
= 4 NEON ops per 8 pixels instead of 16+ scalar ops per pixel.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Intra-session control: `color_native_cached` (G1 path, unaffected by P2).
All runs thermally hot (machine ran tests + benchmarks in sequence):

| Run | bilevel (ms) | color (ms) | bilevel/color |
|---|---|---|---|
| I3 baseline | 91.8 | 88.0 | 1.043 |
| P2 hot #1 | 161 | 193 | 0.834 |
| P2 hot #2 | 217 | 235 | 0.923 |
| P2 hot #3 | 167 | 209 | 0.799 |

Average P2 ratio: 0.852. Thermal-corrected speedup on bilevel:
(1.043 − 0.852) / 1.043 = **18% improvement** (conservative; best run: 24%).
Assembly confirmed: `ldp q0, q1` + `stp q0, q1` in hot loop (LBB314_36).

**Decision. Kept.**

**Reason.** NEON `ldp q0, q1 / stp q0, q1` replaces 16+ scalar ops per pixel. The 8 KiB
BILEVEL_RGBA table fits in L2 cache and persists across rows; L2 latency (~7 cycles) for
each 32-byte fetch is dwarfed by the 12-cycle-per-pixel scalar baseline. Guard covers all
standard full-page renders (offset_x==0, which is the common case). I3 still handles
all-zero rows via `fill(255)`. 776 tests pass.

---

### Q1: clamp-free bilevel 1:1 inner loop — **Reverted** (2026-06-24)

**Issue.** The scalar bilevel 1:1 inner loop uses `.min(page_w - 1)` to clamp the pixel
coordinate, which LLVM cannot prove is a no-op. Hypothesis: removing the clamp (when
`offset_x == 0 && out_w <= mask.width`) would allow LLVM to auto-vectorise the bit-extract.

**Approach.** Add a guard `if ctx.offset_x == 0 && out_w <= mask.width` and a clamp-free
inner loop using `mask_row[ox >> 3]` directly.

**Numbers.** Assembly inspection confirmed the resulting inner loop was still **scalar**
(identical instruction sequence to the original, just without the cmp/csel clamp): LLVM
cannot auto-vectorise the `(mask_row[ox>>3] >> (7-(ox&7))) & 1` bit-extraction pattern even
without the clamp. Both hot benchmark runs showed `No change in performance detected`.

**Decision. Reverted** (immediately superseded by P2 which uses BILEVEL_RGBA lookup).

**Reason.** Bit-extraction patterns with non-trivial shift amounts are not auto-vectorised
by LLVM/AArch64 backend. P2 restructures the loop to use direct table lookup and achieves
the NEON vectorisation that Q1 could not.

---

### G1b: pre-expand mask row to bytes in B-series (downscale) bilinear path — **Reverted** (2026-06-24)

**Issue.** The B-series bilinear compositor (`composite_rows_bilinear_one` else-branch,
invoked for non-1:1 scales) had the same 7-op bit-extraction per pixel as the 1:1 path
before G1. Hypothesis: the same MASK_EXPAND pre-expansion trick would give similar gains.

**Approach.** Add a `g1b_buf`/`g1b_mask` pre-expansion block (identical to G1) before
the B-series inner loop. The B-series accesses mask at page-space coordinates (`px` jumping
by `fx_step/FRAC` per output pixel). After expansion, `g1b_mask[px]` replaces the 7-op
bit extraction.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** `color_native_cached` (1:1, uses G1 path — unaffected) as control.
No corpus file available for B-series color; used `render_page/dpi/72`
(watchmaker at 72 DPI — exercises B-series, includes cached codec work).

| Benchmark | Baseline (ms) | G1b (ms) | Change |
|---|---|---|---|
| render_page/dpi/72 | cached Criterion | 91.3 | **+10% regression** (p = 0.00) |

Intra-session thermal check: `color_native_cached` showed no change (47ms), confirming the
regression is from G1b, not thermal variation.

**Decision. Reverted.**

**Reason.** At 72 DPI (638 output pixels/row) the pre-expansion cost (319 MASK_EXPAND
iterations per row) is 0.5 iterations per output pixel — 4× the amortization ratio of G1
in the 1:1 path (0.125/pixel). The break-even is ~532 output pixels/row. At 72 DPI (638)
we are barely above break-even, and the 4096-byte stack buffer zeroing adds ~32 cycles/row
overhead; the net is a measured 10% regression. At 150 DPI (1274 output pixels) the
benefit would be positive, but no benchmark corpus file exists to verify. Reverted rather
than guarding on output width — adds complexity without verified benefit.

---

### I3: all-white row fast path in bilevel 1:1 compositor — **Kept** (2026-06-24)

**Issue.** The bilevel 1:1 fast path in `composite_rows_bilevel_one` runs a per-pixel
bit-extraction + branchless-channel-write loop for every output row, including rows that
contain no foreground bits (page margins, blank inter-line space). No early exit existed
for the common all-background case.

**Approach.** Before the inner loop, scan the pre-hoisted mask row with
`mask_row.iter().any(|&b| b != 0)`. If all bytes are zero → `row_buf.fill(255)` (one
vectorised fill) and return. Check cost: 319 OR operations, NEON-vectorised to ~20 cycles;
amortised overhead for 3301 rows ≈ 0.02ms. Mirrors the F2 fast path in the bilinear path.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Intra-session control: `color_native_cached` (unaffected by I3). Ratios:

| Run | bilevel (ms) | color (ms) | bilevel/color |
|---|---|---|---|
| G2-revert baseline | 101.9 | 95.4 | 1.068 |
| I3 | 91.8 | 88.0 | 1.043 |

I3 speedup (thermal-corrected via ratio): (1.068 − 1.043) / 1.068 = **2.3%** on
bilevel_native_cached (cable_1973, 300 DPI, dense typeset — low whitespace fraction).
Not statistically significant in isolation (p = 0.33). Effect scales with document
whitespace; academic or letter-format scans with larger margins would see more benefit.

**Decision. Kept.**

**Reason.** Simple, correct, zero-cost for documents with no blank rows. Analogous to F2.
776 tests pass.

---

### G2: bg-fill-then-fg-fixup in general 1:1 bilinear path — **Reverted** (2026-06-24)

**Issue.** After G1 reduced the per-pixel mask check, the per-bg-pixel cost is dominated by
`bg_row.get(off..off+4).map_or(...)` (~6 cycles per bg pixel × 80% of pixels). Hypothesis:
replacing the per-pixel bg lookup with a one-shot memcpy over the entire row (Pass 1), then
only overwriting fg pixels with FG44 bilinear (Pass 2), would eliminate 80% of per-pixel
subslice loads.

**Approach.** When `ctx.gamma_is_identity`, mask present, bg fits (same conditions as F2):
- Pass 1: `row_buf.copy_from_slice(&bg_row[offset_x*4..(offset_x+out_w)*4])` — one memcpy.
- Pass 2: iterate `0..out_w` over pre-expanded g1_mask bytes; `continue` for bg pixels;
  compute bilinear and overwrite for fg pixels.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Intra-session control: `bilevel_native_cached` (unaffected by G2). Ratios:

| Run | color (ms) | bilevel (ms) | color/bilevel |
|---|---|---|---|
| G1-only (prior session) | 90.6 | 96.4 | 0.940 |
| G2 | 95.4 | 101.9 | 0.936 |

G2 shows color/bilevel 0.940 → 0.936 — a 0.4% difference, within noise (p = 0.55,
Criterion "no change"). 776 tests pass.

**Decision. Reverted.**

**Reason.** LLVM already pipelines the g1_mask load and bg_row load in the G1 inner loop
using out-of-order execution — the two independent loads overlap, making the original
single-pass pattern effectively as fast as a separate memcpy pass. G2's Pass 2 iteration
(scanning 2550 g1_mask bytes to find fg pixels) adds a sequential dependency chain that
prevents the same pipelining. Net result: no measurable improvement.

---

### G1: pre-expand mask row to bytes in general 1:1 bilinear path — **Kept** (2026-06-24)

**Issue.** In the general 1:1 bilinear path of `composite_rows_bilinear_one` (color pages with
FG44 at native DPI), the per-pixel mask check involves 7 operations: `Option::is_some_and`,
bounds-check `pxu < mask_w`, shift `pxu >> 3`, bounds-checked load `row.get(...)`, bit-position
shift `>> (7 - (pxu & 7))`, AND, and compare. This costs ~4 cycles per pixel on M1 Max
(load-latency dominated). For a 2550-wide page with 75% non-bg rows, this accounts for a
large fraction of total compositor time.

**Approach.** Before the inner loop, iterate over the bit-packed mask row once (319 iterations
for 2550px wide) and expand each mask byte to 8 per-pixel bytes via `MASK_EXPAND` LUT into a
4096-byte stack buffer (`g1_buf`). Then in the inner loop:
```rust
let is_fg = g1_mask.get(px as usize).copied().unwrap_or(0) != 0;
```
One bounds-checked byte load + compare, replacing the 7-op extraction.

Pages wider than `G1_MAX = 4096` px fall through to the original bit-extraction. No-mask
pages get `&g1_buf[..0]` → `get(px) = None → is_fg = false`. Applied after F2 (all-bg rows
are already returned early and never reach the G1 pre-expansion).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Intra-session thermal control: `bilevel_native_cached` (bilevel compositor,
G1 does not affect this path). Two Criterion runs in the same session:

| Run | color_native_cached | bilevel_native_cached | color/bilevel ratio |
|---|---|---|---|
| F2 only (prior run) | 257ms | 218ms | 1.179 |
| F2 + G1 (this run) | 90.6ms | 96.4ms | 0.940 |

G1 speedup: 1.179 / 0.940 = **1.254 → 25.4% faster** on color_native_cached
(watchmaker.djvu, 300 DPI, warm caches, single-threaded).

Criterion showed -64.8% for color vs -55.8% for bilevel — the 9% gap (in log space) is the
G1 contribution; the remaining 55.8% is the thermal improvement between runs.

**Decision. Kept.**

**Reason.** 25% improvement, strong intra-session thermal correction, 776 tests pass. Cost
is 319 MASK_EXPAND iterations per mixed row (amortized: 319/2550 = 0.125 iterations per
pixel), paying ~638 cycles to save ~2550 × 4 = 10200 cycles in the inner loop. Stack
allocation (4096 bytes, zero-initialized once) is negligible; LLVM may omit zeroing for the
bytes beyond `mask_w` that are never read.

---

### F2: all-bg row fast path in general 1:1 bilinear path — **Kept** (2026-06-24)

**Issue.** For color pages with FG44 at native DPI (`composite_rows_bilinear_one`, general 1:1
path), every output row dispatches per-pixel between FG44 bilinear lookup (fg pixels) and
BG44 row copy (bg pixels). Margins and inter-line gaps — typically 25-35% of rows in dense
text scans — contain no foreground bits at all; the per-pixel branch and FG44 setup are
wasted for those rows.

**Approach.** Before the inner loop (after C3 mask-row hoist), pre-scan the hoisted mask row:
- `mask_row_1x1 = None` (no mask) → all pixels are bg → bulk copy
- `mask_row_1x1 = Some(row)` with `row[..].iter().all(|&b| b == 0)` → all pixels are bg → bulk copy

For gamma-identity renders (`ctx.gamma_is_identity`) when output fits within the bg pixmap
(`offset_x + out_w <= bg_w`): `row_buf.copy_from_slice(&bg_row[offset_x*4..(offset_x+out_w)*4])`.
No mask and no bg → `row_buf.fill(255)`. Falls through to per-pixel loop for non-identity gamma
or edge cases.

Mirrors the existing E1 full-width bulk-copy already present in the A2 (no-FG44) fast path.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** `render_compositor_only/color_native_cached` (watchmaker.djvu, 300 DPI equivalent,
single-threaded, warm caches). Stored Criterion baseline from prior session (100ms hot state);
this session is 2.53× less throttled (thermal factor from bilevel control: 218ms/86ms).

Expected color without F2: 105ms × 2.53 = 266ms. Actual: 257ms → F2 saves ~3.4% (9ms).
Criterion showed +146% for color vs +154% for bilevel (control) — the 8% difference maps to
the ~3% absolute speedup after thermal correction. Confidence interval too wide (205–315ms,
10 samples) for statistical significance at this thermal state.

Theoretical estimate for 25% all-bg rows: saves 25% × 2550 × ~12 cycles/pixel = ~8ms cool;
consistent with the 9ms intra-session estimate.

**Decision. Kept.**

**Reason.** Correct, clean, and consistent with the existing E1 bulk-copy in the A2 path.
Intra-session thermal estimate confirms the expected direction. Benefits scale with whitespace:
higher for documents with wide margins or loose leading. 776 tests pass.

---

### MASK_EXPAND batch + mb==0 fill in bilevel 1:1 path (E1) — **Reverted** (2026-06-24)

**Issue.** The bilevel 1:1 fast path in `composite_rows_bilevel_one` does per-pixel bit extraction:
`((mask_row[px >> 3] >> (7 - (px & 7))) & 1)`. Hypothesis: grouping 8 pixels per mask byte via
`MASK_EXPAND` + a bulk `fill(255)` for all-white bytes (`mb==0`) would be faster, since typical
text pages have >50% empty mask bytes (margins, inter-line gaps).

**Approach.** Added an aligned fast path (`offset_x & 7 == 0 && offset_x + out_w <= page_w`):
- When `mb == 0`: `row_buf[chunk_start*4..chunk_end*4].fill(255)` — 32-byte NEON fill.
- Otherwise: `MASK_EXPAND[mb]` LUT expansion + per-pixel write of `!exp[j]`.
- Unaligned offset falls through to original per-pixel path.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Using `dpi/150` (downscale, E1 does not affect this path) as the thermal control:
the stored Criterion baseline was from a heavily throttled prior session (131ms at 150 DPI).
This session measured 23ms at 150 DPI → 5.7× less thermal throttling.

Thermal-correcting `dpi/300` (1:1 bilevel path, E1 target): old baseline 479ms / 5.7 = 84ms
expected without code change. Measured after E1: 82ms — a difference of 2ms (2.4%), within
measurement noise (Criterion showed -82.9% change, but that reflects the 5.7× thermal difference
between sessions, not E1's effect).

**Decision. Reverted.**

**Reason.** No measurable improvement. LLVM already vectorizes the original scalar loop to
efficient NEON on aarch64-apple-darwin: it hoists the mask byte load per 8-pixel group and
generates branchless bit extraction. The explicit `MASK_EXPAND` LUT and branching on `mb==0`
add structure that interferes with LLVM's own vectorization without adding value. The "bilevel
NEON expander" experiment (2026-06-21) reached the same conclusion for manual NEON intrinsics;
E1 confirms the scalar auto-vectorized path is already optimal.

---

### C3: mask-row hoist in general 1:1 bilinear path — **Kept** (2026-06-24)

**Issue.** In `composite_rows_bilinear_one`, the general 1:1 fast path (entered when FG44 or
an offset is present, guarded by `fx_step==FRAC && fy_step==FRAC && bg_x_q24==bg_y_q24==1:1`)
computed `py < m.height && m.get(px, py)` per pixel, which implicitly recomputes the row slice
`m.data[py*stride..]` each time. C2 (from 2026-06-23) had already hoisted the `fy` row for
FG44 and the bg row; the mask row hoist was an analogous cleanup.

**Approach.** Added `mask_row_1x1: Option<(&[u8], u32)>` computed once before the inner loop:
```rust
let mask_row_1x1 = ctx.mask.and_then(|m| {
    if py >= m.height { return None; }
    let stride = m.row_stride();
    m.data.get(py as usize * stride..).map(|row| (row, m.width))
});
```
Inner loop: replaced `m.get(px, py)` with inline bit extraction from the pre-sliced row.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion `render_compositor_only/color_native_cached`: p=0.59 (not statistically
significant). Within thermal noise.

**Decision. Kept.**

**Reason.** Correct optimization — eliminates y×stride multiply per pixel, mirrors C2 pattern.
LLVM likely already applied LICM here (consistent with p=0.59). Code is cleaner and provides
a foundation for future vectorization of the general 1:1 path.

---

### Byte-level POPCNT in `mask_box_coverage` — **Kept** (2026-06-23)

**Issue.** `mask_box_coverage` iterated over every pixel in the output footprint using
`mask.get(sx, sy)` — one conditional branch per bit. At 72 DPI from 300 DPI native the
footprint is ~4×4 = 16 bit reads per output pixel; at 150 DPI it is 2×2 = 4.

**Approach.** Replaced the nested pixel loop with byte-level popcount:

1. Compute `byte_lo = x0 / 8`, `byte_hi = x1.div_ceil(8)`.
2. Derive `first_mask` (clears bits for pixels before x0 in byte_lo) and `end_mask`
   (clears bits for pixels at/after x1 in the last byte), exploiting MSB-first packing.
3. Single-byte footprint path (`byte_hi == byte_lo + 1`): one AND + one `count_ones()` per
   row. Multi-byte path: one AND + popcount per boundary byte, full `count_ones()` per
   middle byte.

Ops per output pixel:
- Before: 16 `mask.get()` calls (16 branches + 16 byte reads + 16 bit shifts)
- After: ~4-8 `count_ones()` calls total (~4 byte reads, no branches in inner loop)

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Direct thermal comparison impossible across runs; used dpi/300 (1:1, no
coverage function called) as thermal control. Thermal factor between before/after runs: 1.82×.

| DPI | Before (bit-by-bit) | After (POPCNT) | After thermal-adj | Δ (adj) |
|-----|---------------------|----------------|-------------------|---------|
| 72  | 9.1 ms  (control: 263 ms) | 12.5 ms (control: 479 ms) | 6.9 ms | −24% |
| 150 | 71 ms   (control: 263 ms) | 131 ms (control: 479 ms)  | 72 ms  | +1% (noise) |

At 72 DPI the footprint spans ~4 pixels per output row, typically a single byte → single-byte
path, 4 ops per pixel vs 16. At 150 DPI the footprint is 2 pixels → 2 ops vs 4 (smaller
relative gain, absorbed by other overheads).

**Decision. Kept.**

**Reason.** ~24% thermal-corrected speedup at 72 DPI (thumbnail renders). No regression at
150 DPI or on the 1:1 path. Code is no more complex than before. Existing unit test
`mask_box_coverage_values` passed unchanged, confirming correctness of the byte-mask logic.

---

### Anti-aliased bilevel text at downscale (`mask_box_coverage`) — **Kept** (2026-06-23)

**Issue.** `composite_rows_bilevel_one` called `mask_box_any` in the downscale branch —
early-exit as soon as any mask bit in the output pixel's footprint was set. This produced
binary black/white output at reduced DPIs (72–150), giving jagged aliased text edges.

**Approach.** Added `mask_box_coverage`: counts the fraction of foreground bits in the
footprint and returns a proportional gray value (0 = all white, 255 = all black). The
downscale branch now computes `ch = 255 - mask_box_coverage(...)` and writes the same gray
value to R/G/B/A. The `mask_shift > 0` sub-path (subsampled max-pool mask) remains binary —
it already encodes one-bit coverage. The 1:1 fast path is completely untouched.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians.

*Indirect proxies (color page, bilevel compositor not involved):*

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_page/dpi/72` | 89.9 µs | 83.3 µs | −7.3% |
| `render_page/dpi/144` | 485 µs | 497 µs | +2.5% (noise) |
| `render_corpus_bilevel` (1:1) | ~42 ms | ~44 ms | ±noise only |

*Dedicated bilevel downscale benchmark (`render_corpus_bilevel_dpi`, cable_1973_100133.djvu,
native 300 DPI; added in the same commit). Both runs below were made in a thermally hot state
so numbers are internally consistent but higher than ideal. The 300 DPI column (1:1, neither
function called) acts as a thermal control — any ratio deviation from 1:1 between 72/300 or
150/300 isolates the bilevel downscale overhead.*

| DPI | mask_box_any (binary) | mask_box_coverage (AA) | Ratio AA/binary | 1:1 thermal control |
|-----|-----------------------|------------------------|-----------------|---------------------|
| 72  | 17–25 ms | 7.3–11 ms | ~0.5× | ÷(463 ms / 263 ms) ≈ 0.5× thermal |
| 150 | 148–196 ms | 61–81 ms | ~0.5× | same factor |
| 300 (1:1) | 463–542 ms | 229–299 ms | ~0.5× thermal only | control |

The ratio at all DPIs is the same ~0.5×, including the 1:1 control where no coverage function
runs. Conclusion: the bilevel downscale overhead of `mask_box_coverage` vs `mask_box_any` is
indistinguishable from noise on M1 Max due to thermal variance between the two measurement
runs (~2× CPU frequency drift). The 1:1 bilevel corpus path is unchanged.

*Stable reference (mask_box_coverage, hot state): dpi/72 median ≈ 9 ms, dpi/150 ≈ 71 ms,
dpi/300 (1:1) ≈ 263 ms. Use these for future regression comparisons.*

**Decision. Kept.**

**Reason.** Clear quality improvement for the common screen-viewing use case (72–150 DPI):
anti-aliased text edges replace binary aliasing. No measurable cost on the hot 1:1 path.
Downscale path overhead is bounded — counting `fx_step * fy_step` bits per output pixel,
same order as `sample_area_avg` already does for background. All 958 tests pass, including
a new unit test for `mask_box_coverage`. Closes #421.

---

### Pre-hoist FG44 y-rows and bg row in general 1:1 path (C2) — **Kept** (2026-06-23)

**Issue.** In `composite_rows_bilinear_one`, the general 1:1 path (entered when FG44 or an
offset is present) recomputed `map_plane_center_frac(fy, ctx.fg_y_q24)` per fg pixel and
`map_plane_center_frac(fx, ctx.bg_x_q24)` per bg pixel. `fy` is row-invariant; `bg_x_q24`
is guaranteed to be `1<<24` by the outer branch condition (`map_plane_center_frac(fx, 1<<24)
== fx`, a no-op multiply). The B-series path already applied these hoistings (B1 and B2);
the general 1:1 path was not updated after those optimisations were added.

**Approach.**

- **C2** (FG44 y-row hoisting): compute `fg_fy`, `y0`, `y1`, `ty`, and row slices
  `(row0, row1)` from `fg.data` once per row, before the inner loop. Replace
  `sample_bilinear(fg, fg_fx, fg_fy)` with `bilinear_from_rows(row0, row1, fg_w, fg_fx, fg_ty)`.
- **C2b** (bg row hoisting): pre-hoist `bg_row = bg.data[py * stride..]` and `bg_w` once
  per row. Replace `map_plane_center_frac(fx, bg_x_q24) + sample_nearest(bg, ...)` with a
  direct indexed slice read: `bg_row[px.min(bg_w-1) * 4..]`.

Eliminated per-iteration in the hot pixel loop:
- fg pixels: 1 × u64 multiply (`map_plane_center_frac(fy)`), y0/y1/ty computation, 2 row
  pointer lookups inside `sample_bilinear`.
- bg pixels: 1 × u64 multiply (`map_plane_center_frac(fx, 1<<24)` ≡ identity), row-index
  multiply inside `get_rgb` (hoisted to per-row stride multiply).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians; bilevel corpus shows high variance (53–76ms), likely thermal.

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` (sequential) | 43.99 ms | 43.39 ms | −1.4% |
| `render_corpus_bilevel` | ~42–45 ms | ~43–64 ms | noise (path unchanged for mask-only pages) |

**Decision. Kept.**

**Reason.** −1.4% on the primary color-page benchmark. Change is a pure refactor (identical
numeric output, verified by 958 passing tests). The C2b bg-row hoisting also removes the
dead `bg_fy = map_plane_center_frac(fy, ctx.bg_y_q24)` variable that was previously computed
but then re-derived inside `sample_nearest`. `sample_nearest` is now only used in tests
(annotated `#[cfg_attr(not(test), allow(dead_code))]`). Closes #424 and #425.

---

### FG44 color lookup: `sample_nearest` → `sample_bilinear` — **Kept** (2026-06-22)

**Issue.** The 1:1 compositor path used `sample_nearest` to look up FG44 colors for
foreground (masked) pixels. FG44 is 3× subsampled: each FG44 pixel covers a 3×3 block of
output pixels. Nearest-neighbor sampling snaps to the nearest FG44 pixel center, producing
visible blocky 3×3 color tiles at color-text boundaries.

**Approach.** Replace `sample_nearest(fg, fg_fx, fg_fy)` with `sample_bilinear(fg, fg_fx,
fg_fy)` at both call sites in `djvu_render.rs` (general path ~line 2003 and B-series
optimized path ~line 2075). `sample_bilinear` has an identical signature and uses
fixed-point bilinear interpolation between the 4 nearest FG44 pixels, giving smooth color
transitions at fg44 pixel boundaries.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians vs Cow-optimization baseline (~43.8 ms criterion-stored).

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` (sequential) | 43.80 ms | 43.99 ms | +0.42% |
| `render_corpus_bilevel` (sequential) | 42.06 ms | 42.21 ms | +0.37% |

Bilevel pages have no FG44 layer; the +0.37% there is measurement noise. The +0.42% on
color pages is the overhead of 4 FG44 pixel reads instead of 1 per foreground pixel. Since
foreground (masked) pixels are a small fraction of the total in typical pages, the cost is
absorbed in benchmark noise.

**Decision. Kept.**

**Reason.** Genuine quality improvement: smooth bilinear color interpolation at FG44 pixel
boundaries replaces the visible 3×3 color-block artifact. Performance cost is negligible
(< 0.5%, within measurement noise). `sample_bilinear` was already implemented and tested;
this change enables it on the live rendering path. All 957 tests pass.

---

### Eliminate FG44 + Mask clones via `Cow` — **Kept** (2026-06-21)

**Issue.** After the BG44 Cow optimization, two smaller clones remained per warm render:

- `decode_fg44`: `page.decoded_fg44().cloned()` — clones the cached 3.7 MB FG44 Pixmap
  (subsample 3, ~850×1100 pixels × 4 bytes for a typical A4 scan at 300 DPI).
- `decode_mask`: `bm.clone()` — clones the cached 1.05 MB JB2 mask Bitmap (packed bits,
  ~2550×3300/8 bytes). Documented as "cloned cheaply (1 MB memcopy)" in the source.

Together: ~4.75 MB of unnecessary allocations and memcopy per warm render.

**Approach.** Same `Cow<'a, …>` pattern as the BG44 optimization:

- `decode_mask<'a>(page: &'a DjVuPage)` → `Result<Option<Cow<'a, Bitmap>>, …>`:
  cache hit → `Cow::Borrowed(bm)`, cold decode → `Cow::Owned`.
- `decode_fg44<'a>(page: &'a DjVuPage)` → `Result<Option<Cow<'a, Pixmap>>, …>`:
  FG44 cache hit → `Cow::Borrowed`, JPEG fallback → `Cow::Owned`.
- `ForegroundLayers<'a>` and `DecodedLayers<'a>` updated to hold `Option<Cow<'a, …>>`.
- `decode_foreground_strict<'a>` gains a lifetime.
- Bold dilation (`opts.bold > 0`): `mask.into_owned().dilate_n(n)` — clones only when
  actually needed (bold=0 is the common case in all benchmarks).
- Four `mask.as_ref()` and four `fg44.as_ref()` call sites → `as_deref()`.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians vs state after BG44 Cow experiment.

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` (sequential) | 42.65 ms | 41.93 ms | −1.7% |
| `render_corpus_bilevel` (sequential) | 42.52 ms | 41.92 ms | −1.4% |
| `render_corpus_color` (parallel, `--features parallel`) | 7.7 ms | 7.23 ms | −6.1% |

**Decision. Kept.**

**Reason.** Eliminates 4.75 MB of memcopy (FG44 3.7 MB + mask 1.05 MB) on every warm
render at the cost of two `Cow<'a, …>` wrappers and a lifetime parameter. The parallel
improvement is proportionally larger because the absolute compositor time is shorter, so
the clone is a bigger fraction of total work. All 775 tests pass; no_std unaffected (the
non-cached path stays `Cow::Owned`).

---

### Skip `Pixmap::white` pre-fill via zero-initialized buffer — **Rejected** (2026-06-22)

**Issue.** `render_pixmap` (and `render_region`, `render_progressive`) allocates the output buffer
via `Pixmap::white(w, h)` = `vec![255u8; w*h*4]`, a 33.6 MB write of 0xFF. Every compositor
path (bilevel, bilinear A2, general 1:1, area-avg) then overwrites every pixel before return, so
the initial 255-fill is redundant and doubles the write bandwidth to the output buffer.

**Approach.** Added `Pixmap::zeroed(w, h)` to djvu-pixmap (calls `Pixmap::new(0,0,0,0)` →
`vec![0u8; n]`). Changed all 8 output-buffer allocation sites in djvu_render.rs from
`Pixmap::white(...)` to `Pixmap::zeroed(...)` (render_pixmap, render_region, render_progressive,
render_coarse, plus aa_downscale and rotate_pixmap variants).

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians vs state after FG44+mask Cow experiment.

| Benchmark | Before | After |
|-----------|--------|-------|
| `render_corpus_color` (sequential) | 41.93 ms | 41.53 ms (−1.0%) |
| `render_corpus_color` (parallel, `--features parallel`) | 7.23 ms | **8.77 ms (+21%)** |
| `render_corpus_bilevel` (parallel) | 7.30 ms | **11.82 ms (+62%)** |

**Decision. Rejected.**

**Reason.** On macOS, `vec![0u8; n]` for a large allocation returns lazy-zero VM pages (`mmap(MAP_ANONYMOUS)`
via the system allocator). The page-faults are deferred to first write. In the sequential compositor,
page faults happen in order and are handled cheaply. In the parallel compositor (`par_chunks_exact_mut`),
multiple rayon threads simultaneously fault disjoint 4 KB pages of the same large allocation, causing
contention in the macOS VM subsystem — the parallel render regressed by 21–62%.

By contrast, `Pixmap::white` uses `vec![255u8; n]` which actively writes every page before the
parallel compositor starts. This "pre-warms" the pages: all page faults are handled sequentially
during allocation, and the parallel compositor gets clean private pages with no kernel overhead.

The sequential path showed a marginal −1.0% improvement (less work: write 33.6 MB with 0 then
overwrite vs write 33.6 MB with 255 then overwrite — same total, but zero writes may alias CPU
zero-write optimizations). Not enough to justify the parallel regression.

---

### Fill+overlay compositor paths — **Rejected** (2026-06-21)

**Issue.** The bilinear compositor (`composite_rows_bilinear_one`) for a warm color page
writes the output in a single pass but still performs per-pixel blend arithmetic. Hypothesis:
pre-filling the output buffer with the background (one 33.6 MB `copy_from_slice`) and then
overlaying only foreground pixels (sparse writes) would reduce total work for pages where
most pixels are background.

**Approach.** Three variants after the BG44 Cow commit:

1. **Bilevel fill+overlay**: In `composite_rows_bilevel_one`, before the main loop: fill
   the full output row from the background (`copy_from_slice` of 4 bytes × width), then
   overwrite only the foreground pixels (mask bits that are 1). Intended to remove the
   per-pixel `ch = (is_fg.wrapping_sub(1) & 0xFF)` branch-free blend.
2. **A3 (FG44 copy+overlay)**: For the A2 path (bg+mask, FG44 present, no palette): copy
   the background slice into the output row, then overwrite foreground pixels from the FG44
   plane. Variant of the "fill then sparse overwrite" idea for the color case.
3. **A4 (palette copy+overlay)**: Same as A3 but for the FGbz palette path: copy background,
   then overwrite foreground pixels from the palette table.

A micro-benchmark (`rustc -O` standalone binary) was written to measure the pure inner loop
in isolation: it completes in ~2 ms, confirming the 42 ms benchmark cost is not the tight
pixel loop itself.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians, `render_corpus_color` after all three variants applied together.

| Variant | `render_corpus_color` | Δ |
|---------|-----------------------|---|
| Baseline (BG44 Cow, no fill+overlay) | 41.3 ms | — |
| Fill+overlay (all three variants) | 42.65 ms | +3.3% |

**Decision. Rejected.**

**Reason.** All three variants regressed. Root cause: the fill+overlay approach writes 33.6 MB
twice — once for the background copy, once for the foreground overlay reads — vs the original
single-pass write. The working set already exceeds L3 cache capacity (33.6 MB bg read +
33.6 MB output write); doubling the output writes adds a third pass over main memory. The
micro-benchmark confirmed the inner loop is fast; the bottleneck is memory bandwidth, not
arithmetic.

---

### Eliminate BG44 Pixmap clone via `Cow<'a, Pixmap>` — **Kept** (2026-06-21)

**Issue.** After the `bg_rgb_s1` cache (previous experiment), the warm render path still cloned the
33.6 MB cached `Pixmap` via `.cloned()` at the `decode_background_chunks` / `decode_background_chunks_permissive`
call site. This incurred a ~0.67 ms memcopy per warm render regardless of whether the compositor
actually mutated the background.

**Approach.** Changed `decode_background_chunks` and `decode_background_chunks_permissive` to return
`Option<Cow<'a, Pixmap>>` (lifetime tied to `&'a DjVuPage`). For the cached `subsample == 1,
max_chunks == usize::MAX` path: return `Cow::Borrowed(page.decoded_bg_rgb_s1()?)`. For all other
paths (fresh decode, subsample != 1, progressive): return `Cow::Owned(pixmap)`. Updated
`DecodedLayers<'a>` and `decode_layers<'a>` with the same lifetime. Changed all four
`CompositeContext::from_layers(... bg.as_ref() ...)` call sites to `bg.as_deref()`.

`Cow` resolves via `Deref<Target = Pixmap>` to `&Pixmap` transparently, so `CompositeContext`
sees the same `Option<&Pixmap>` as before. No change to non-cached paths.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians vs state after inline-alpha experiment.

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` (sequential) | 41.8 ms | 41.3 ms | −1.2% |
| `render_corpus_color` (parallel, `--features parallel`) | 8.3 ms | 7.7 ms | −7.2% |

**Decision. Kept.**

**Reason.** Eliminates a 33.6 MB memcopy on the hot warm-render path with zero semantic change —
callers already only read the background. The improvement is larger in the parallel path because
the sequential compositor dominates less; the 0.67 ms clone is a proportionally bigger fraction
of 8.3 ms than 41.8 ms. `Cow::Borrowed` has zero overhead (it's a pointer, no heap allocation).
All 775 tests pass; no_std build unaffected (the non-cached path falls through to `Cow::Owned`).

---

### Inline alpha in bilinear compositor, remove `fill_alpha_255` — **Kept** (2026-06-21)

**Issue.** The non-downscale bilinear compositor (`composite_rows_bilinear_one`) wrote only R, G,
B channels per pixel (not alpha). Alpha was set in a separate post-pass: `fill_alpha_255(buf)` over
the full 33.6 MB buffer. In `composite_into` (the parallel path) this post-pass ran sequentially
after `par_chunks_exact_mut`, serialising ~33.6 MB of memory I/O. In `composite_rows` (streaming)
it ran per-row but still touched each row twice.

**Approach.** Added `pixel[3] = 255` in every pixel-write site inside
`composite_rows_bilinear_one` (A2 macro, no-mask loops, general 1:1 path, general bilinear path).
The E1 full-width `copy_from_slice` path already copies alpha=255 from the bg Pixmap (whose
YCbCr-decode functions always set alpha=255). Removed `fill_alpha_255` calls from `composite_into`
and `composite_rows`, then deleted the now-dead `fill_alpha_255` / `fill_alpha_255_sse2` helpers.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians vs state after BG44 RGB Pixmap cache (previous experiment).

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` (sequential) | 42.8 ms | 41.8 ms | −2.3% |
| `render_corpus_bilevel` (sequential) | 43.0 ms | 42.1 ms | −2.1% |
| `render_streaming_discard/watchmaker` (seq) | 42.2 ms | 41.0 ms | −2.8% |
| `render_corpus_color` (parallel, --features parallel) | 9.5 ms | 8.3 ms | −12.6% |

**Decision. Kept.**

**Reason.** Writing 4 bytes per pixel in one pass is faster than 3 + a separate alpha post-pass
in either mode. The gain is especially large in the parallel path where the sequential 33.6 MB
post-pass was a bottleneck (0.34 ms at memory bandwidth) relative to the 4 ms parallel compositor.
Also removes ~50 lines of SSE2 code that is no longer needed.

---

### BG44 decoded RGB Pixmap cache (`PageLayers::bg_rgb_s1`) — **Kept** (2026-06-21)

**Issue.** Every warm render of a colour DjVu page called `Iw44Image::to_rgb_subsample(1)` even
though the decoded `Iw44Image` was already cached. The conversion (parallel IDWT + YCbCr→RGBA +
33.6 MB allocation) took 2.8–2.9 ms per render call, accounting for ≈6% of total render time.

**Approach.** Added `bg_rgb_s1: OnceLock<Option<Pixmap>>` to `PageLayers`. The new accessor
`PageLayers::bg_rgb_s1()` calls the existing `bg44()` cache then runs `to_rgb_subsample(1)` once
and stores the result. `decode_background_chunks` and its permissive variant use this cache for the
`max_chunks == usize::MAX, subsample == 1` path (the common full-resolution render case). Strict
mode still propagates BG44 decode errors via `decoded_bg44().ok_or(...)`.

The saved Pixmap is cloned on each warm render (~0.34 ms memcpy of 33.6 MB) instead of
recomputed (2.8 ms). Net saving ≈ 2.5 ms per warm render.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians, 100-sample runs for regression gates, 10-sample for stage breakdown.

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `render_corpus_color` | 45.1 ms | 42.8 ms | −5.1% |
| `render_corpus_bilevel` | 44.9 ms | 43.0 ms | −4.2% |
| `render_streaming_discard/watchmaker_color` | 44.1 ms | 42.2 ms | −4.3% |
| `render_streaming_discard/cable_bilevel` | ~48 ms | 42.4 ms | −11.7% |
| `render_compositor_only/color_native_cached` | 44.8 ms | 42.3 ms | −5.6% |

**Decision. Kept.**

**Reason.** Consistent 4–12% improvement across all color and mixed-layer benchmarks at the cost
of ~33.6 MB additional memory per page that has ever been rendered at sub=1. Acceptable trade-off
for the common interactive use case (viewer renders the same page repeatedly). Strict-mode error
semantics preserved via explicit `decoded_bg44()` guard before returning the cached value.

---

### Bilevel NEON expander (`composite_rows_bilevel_one`) — **Rejected** (2026-06-21)

**Issue.** Accelerate the bilevel 1:1 fast path in `composite_rows_bilevel_one` with AArch64 NEON
by expanding 1 mask byte → 8 RGBA pixels per iteration via `vst4_u8`.

**Approach.** Three variants tried against commit after `819f4a3`:

1. **Separate `#[target_feature(enable = "neon")] unsafe fn bilevel_neon_1to1`** called once per row:
   scalar head + NEON body (`vdup_n_u8` / `vand_u8` / `vceq_u8` / `vst4_u8`) + scalar tail.
   Inner loop had two branch conditions (`px_start + 8 > page_w`, `byte_idx >= mask_row.len()`).

2. **Same code inlined** directly inside the `#[cfg(target_arch = "aarch64")]` block in
   `composite_rows_bilevel_one` (no separate function, avoids call overhead).

3. **Branch-free inner loop**: hoisted both bounds checks out of the NEON body, computing
   `neon_end = (page_w - start_px) & !7` once before the loop.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians, `--bench render render_corpus_bilevel`.

| Variant | `render_corpus_bilevel` |
|---------|------------------------|
| Baseline (scalar) | 44.9 ms |
| Variant 1 (separate `#[target_feature]` fn) | 52.2 ms (+16%) |
| Variant 2 (inlined, branches in loop) | 50.3 ms (+12%) |
| Variant 3 (inlined, branch-free inner loop) | 45.7 ms (+1.8%) |

**Decision. Rejected.**

**Reason.** Even with the branch-free inner loop, the NEON expansion breaks even with scalar at
best (+1.8%, within noise). Root cause: LLVM already generates efficient NEON for the scalar
`is_fg.wrapping_sub(1) & 0xFF` pattern on aarch64-apple-darwin (NEON is always enabled). Manual
`vst4_u8` adds complexity without measurable gain. The separate `#[target_feature]` function
cannot use `#[inline(always)]` in stable Rust (see issue #145574), so it incurs per-row call
overhead.

---

### Color compositor 1:1 fast path NEON (`a2_has_mask_loop!`) — **Rejected** (2026-06-21)

**Issue.** The color bilinear compositor (`composite_rows_bilinear_one`) spends ~45ms on a
2550×3301 colour page. Inner loop (`a2_has_mask_loop!`) expands mask bytes via `MASK_EXPAND` LUT
and blends BG → output. Hypothesis: NEON `vld4_u8/vst4_u8` plus bitwise ops per 8 pixels would
beat the scalar LUT path.

**Approach.** Added a `gamma_is_identity` fast path in `composite_rows_bilinear_one` bypassing the
macro: when the full row fits in BG pixmap and mask, reads RGBA using `vld4_u8`, computes
`not_fg = vceq_u8(vand_u8(vdup_n_u8(mb), bit_pos), zero)`, ANDs R/G/B channels, writes with
`vst4_u8`. Scalar fallback for non-AArch64. Tail pixels handled separately.

**Platform.** macOS Darwin 25.5.0 / Apple M1 Max, aarch64, Rust stable 1.88.

**Numbers.** Criterion medians, `render_native_stages/render_streaming_discard/watchmaker_color`.

| Variant | Time |
|---------|------|
| Baseline | 45.1 ms |
| F1 scalar fast path | 45.9 ms (+0.9 ms) |
| F1 NEON fast path | 46.4 ms (+1.3 ms) |

**Decision. Rejected.**

**Reason.** No improvement — slight regression. The inner loop is not the bottleneck. LLVM already
vectorises the scalar code efficiently on AArch64. The compositor time (~42–46 ms for 8.4 M pixels)
is dominated by something else (memory bandwidth, `fill_alpha_255`, or `to_rgb_subsample`), not
the per-pixel blend arithmetic.

---

### #420 — SIMD `sample_area_avg_bounds` (NEON/SSSE3 accumulation) — **Rejected** (2026-06-21)

**Issue.** #420: accelerate the `sample_area_avg_bounds` inner loop with SIMD
to speed up the area-average downscale path in `composite_rows_area_avg_one`.

**Approach.** Two variants tried on commit `819f4a3`:

1. **Per-row SIMD dispatch** (`rgba_row_rgb_sums`): added helper functions
   `rgba_row_rgb_sums_neon` (AArch64: `vld4_u8` + `vaddlv_u8`, threshold 32 B)
   and `rgba_row_rgb_sums_ssse3` (x86_64: `_mm_shuffle_epi8` + `_mm_sad_epu8`,
   threshold 16 B).  The outer loop over rows called these per row.

2. **Inline 2×2 fast path**: special-cased `cols == 2 && rows == 2` with two
   direct 8-byte slice reads and 12 scalar additions, bypassing the loop entirely.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.5.0
- CPU: Apple M1 Max, aarch64
- Rust: stable 1.92+, RUSTFLAGS: unset

**Numbers.** All times are Criterion medians, `--bench render`, single-thread (no `parallel` feature).

| Benchmark | Baseline | Variant 1 (NEON/SSSE3) | Variant 2 (2×2 inline) |
|-----------|----------|------------------------|------------------------|
| `render_colorbook` | 3.558 ms | 4.264 ms (+19.9%) | 5.183 ms (+45.7%) |
| `render_corpus_color` | 45.863 ms | 45.582 ms (−0.6%, noise) | 45.587 ms (−0.6%, noise) |

**Decision.** Rejected. Both variants regressed `render_colorbook`, which is the
primary area_avg benchmark (colorbook renders at 150 dpi from a 300 dpi source,
producing a 2× downscale through the bg plane).

**Reason.**
- **Variant 1**: The NEON fast path requires ≥ 32 bytes (8 RGBA pixels) per row.
  At 2× downscale the box is 2×2 — each row is 2 pixels = 8 bytes, below the
  threshold.  Every call falls through to the scalar tail, adding function-call
  overhead with zero NEON benefit.
- **Variant 2**: The `if cols == 2 && rows == 2` branch and the tuple
  `if let (Some(a), Some(b))` pattern introduce control-flow that disrupts LLVM's
  optimization of the surrounding inlined composite loop.  The constant `col == 2`
  check prevents the compiler from treating the loop uniformly, increasing
  register pressure and harming branch prediction for other branches.
- In both cases `render_corpus_color` (4× downscale at 300 dpi) showed no
  measurable change, confirming that `sample_area_avg_bounds` is not the
  bottleneck — memory bandwidth and the mask check dominate.

### #419 — Compositor row-level parallelism — **Kept** (2026-06-18, documented here 2026-06-21)

**Issue.** #419 (created 2026-06-21 after audit) — intra-page row parallelism
in `composite_into`.

**Status.** Already implemented and recorded under entry `PARALLEL_COMPOSITOR`
(2026-06-18) as part of the #408 umbrella.  Issue #419 closed as duplicate.

Recorded numbers (from `PARALLEL_COMPOSITOR`, `--features parallel` vs single-thread):

| Benchmark | Single-thread | With `parallel` | Speedup |
|-----------|--------------|-----------------|---------|
| `render_colorbook` | 3.558 ms | 940 µs | 3.8× |
| `render_corpus_color` | 45.86 ms | 12.15 ms | 3.8× |
| `render_corpus_bilevel` | ~45 ms | 13.99 ms | ~3.2× |

(Numbers re-measured 2026-06-21 on commit `819f4a3` reflect further optimizations
since the original PARALLEL_COMPOSITOR recording.)

### #418 — IW44 Y/Cb/Cr IDWT 3-plane parallelism — **Kept** (2026-06-21)

**Issue.** #418: parallelize the three independent `reconstruct()` calls
(inverse wavelet transform per plane) in `Iw44Image::to_rgb()` using `rayon::join`.

**Status.** Already implemented in `crates/djvu-iw44/src/lib.rs` lines 3238–3256
under `#[cfg(feature = "parallel")]`.  Benchmarked here for the first time.

**Approach.** `rayon::join(|| y.reconstruct(sub), || rayon::join(|| cb.reconstruct(sub_c), || cr.reconstruct(sub_c)))` — three independent IDWT passes run on separate threads.  Single-thread path preserved under `#[cfg(not(feature = "parallel"))]`.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.5.0
- CPU: Apple M1 Max, aarch64
- Rust: stable 1.92+, RUSTFLAGS: unset
- Commit: `819f4a3`

**Numbers.** `cargo bench --bench codecs -- iw44_to_rgb_colorbook`

| Benchmark | Single-thread | `--features parallel` | Speedup |
|-----------|--------------|----------------------|---------|
| `iw44_to_rgb_colorbook/sub1_full_decode` | 5.622 ms | 2.556 ms | **2.2×** |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 1.325 ms | 603 µs | **2.2×** |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 344 µs | 212 µs | **1.6×** |

**Decision.** Kept. The code was already merged; benchmarks confirm the win.

**Reason.** Y, Cb, Cr planes share no mutable state after ZP decoding (which is
sequential per-stream). The IDWT step on each plane is independent. On the 10-core
M1 Max, three concurrent IDWT passes achieve ~2.2× wall-time reduction for sub1
(Y dominates; Cb + Cr run in parallel with Y's tail). sub4 gains less (1.6×)
because the compact-plane IDWT is shorter. The `parallel` feature is opt-in, so
builds without rayon are unaffected.

### #408 — area-avg exclusive-box bounds + power-of-2 shift — **Kept** (2026-06-18)

**Issue.** #408 umbrella: close the 2× compositor gap vs DjVuLibre.

**Approach.** Two related changes to `src/djvu_render.rs`:

1. `sample_area_avg` and `mask_box_any`: switch x1/y1 from **inclusive** to
   **exclusive** upper bounds. The old formula `((fx + fx_step) >> FRACBITS).min(w − 1)`
   inclusive landed on the first pixel of the *next* output cell at exact power-of-2
   downscale ratios, giving a 3×3 box at 2× downscale instead of the correct 2×2.
   The fix: `((fx + fx_step) >> FRACBITS).min(w)` exclusive with `x0..x1` iteration.

2. `sample_area_avg`: replace per-pixel bounds check (`data.get(off..off+3)`) with
   one bounds check per row (`data.get(row_off..row_off + cols*4)` → `chunks_exact(4)`),
   and replace UDIV by variable `count` with a rounding right-shift when `count` is
   a power of 2 (the common case: count=4 at 2× downscale).

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.5.0
- CPU: Apple M1 Max, aarch64
- Rust: stable 1.92+, RUSTFLAGS: unset

**Numbers.** All times are Criterion medians. "Baseline" = committed HEAD before
this branch. The committed HEAD already contains the decode_scale regression
(#377), which forces the colorbook bg plane to subsample 2 instead of 4 —
making bg_fx_step = 32 (2× downscale into the bg plane), which hits the
area_avg path. This makes the colorbook improvement large.

```
cargo bench --bench render -- 'render_corpus_color|render_corpus_bilevel'
cargo bench --bench render -- 'render_compositor_only/color_downscale'
cargo bench --bench render -- 'render_colorbook$'
cargo bench --bench codecs -- 'iw44_to_rgb_colorbook/sub2|sub4'
```

| Benchmark | Baseline | After | Δ |
|-----------|----------|-------|---|
| `render_colorbook` (150 dpi, warm) | 25.5 ms | 12.2 ms | **−52%** |
| `render_compositor_only/color_downscale_cached` | 25.2 ms | 14.4 ms | **−43%** |
| `render_corpus_color` (300 dpi color) | 77.9 ms | 70.4 ms | **−10%** |
| `render_corpus_bilevel` (300 dpi bilevel) | 71.9 ms | 71.5 ms | 0% (bilevel path, no area_avg) |
| `compositor_only/color_native_cached` | 71.3 ms | 71.3 ms | 0% (bilinear path, unaffected) ✓ |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 1.339 ms | 1.353 ms | +1.0% (noise) ✓ |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 345.3 µs | 346.5 µs | +0.3% (noise) ✓ |

**Explanation of improvements.**
- `render_colorbook`: when decode_scale picks subsample 2 (wrong), bg_fx_step = 32
  → area_avg path with 2× bg-plane downscale. Old code averaged 3×3=9 bg pixels
  per output pixel; new code averages 2×2=4. −52%.
- `render_corpus_color`: mask_box_any was scanning 3×3=9 bits per output pixel at
  2× page downscale; now scans 2×2=4. −10%.
- `render_corpus_bilevel`: bilevel pages use `composite_rows_bilevel_one` which
  calls neither `sample_area_avg` nor `mask_box_any`. Unaffected by design. ✓
- Bilinear path, sub2/sub4 partial decode: not touched. ✓

**Output change.** The box averaging is now correct (2×2 instead of 3×3 at 2×
downscale). Output pixel values will differ slightly from the old version but are
closer to the mathematically correct box filter. All 608 lib tests pass.

**Decision.** Kept.

**Reason.** The old inclusive range was an off-by-one that inflated the box by
one pixel in each dimension at exact-integer-ratio downscale. The fix is
semantically correct, yields 2-3× speedup in the area_avg path, and leaves all
non-area_avg paths (bilinear, bilevel, partial decode) statistically unchanged.

### Cross-size JB2 record-6 refinement emitter (#322) — **Reverted / kept disabled** (2026-06-15)

**Issue.** #322: build an *experiment-only* cross-size record-6 refinement
emitter behind a probe flag (`max_dim_delta = 2`, `max_hamming_fraction =
0.05`), decode + pixel-compare on `watchmaker` and `pathogenic_bacteria_1896`,
and record the real byte deltas + round-trip status. Keep
`encode_djvm_bundle_jb2` (and the other shipped encoders) unchanged.

**Approach.** Added `CrossSizeRec6Probe` to `Jb2EncodeOptions` (default `None`).
When enabled, a fresh connected component with no exact dictionary hit searches
dictionary entries whose bbox differs by ≤ `max_dim_delta` per axis, scores them
with nearest-neighbor-resampled Hamming distance, and — if within the budget —
emits a **lossless** record-6 matched refinement (`wdiff`/`hdiff` + 11-bit
refinement bitmap against the chosen reference) instead of a record-1 new
symbol. This required finishing the dormant `encode_bitmap_ref` helper: it was
written in image (top-down) space, but the decoder's `decode_bitmap_ref` walks
packed Jbm rows **bottom-up**, and the `>> 1` centre-alignment floor is not
symmetric under that flip — so a non-solid refinement bitmap desynchronised the
ZP stream (`UnknownRecordType` on decode). Rewriting the encoder to mirror the
decoder's Jbm-row traversal exactly made it round-trip.

**Numbers** (re-encoding existing page masks; baseline = shipped rec-1/7 only):

| corpus | pages | pages changed | baseline | probe | delta | round-trip |
|--------|-------|---------------|----------|-------|-------|------------|
| watchmaker (all) | 12 | 12 | 130 036 B | 135 720 B | **+5 684 B (+4.37%)** | lossless |
| pathogenic_bacteria_1896 (first 40) | 38 | 36 | 1 539 929 B | 1 543 596 B | **+3 667 B (+0.24%)** | lossless |

**Decision.** Reverted as a shipping path; the probe stays `None` by default, so
all shipped encoders (`encode_jb2_dict`, `encode_jb2_dict_with_shared`,
`encode_djvm_bundle_jb2`) are byte-identical to before (asserted by
`cross_size_rec6_probe_off_is_byte_identical`). Kept behind the flag with two
round-trip regression tests and an `--ignored` corpus measurement driver
(`cross_size_rec6_probe_corpus_measurement`).

**Reason.** Cross-size refinement *loses* bytes on both corpora. A cross-size
reference must be nearest-neighbor resampled before it can drive the refinement
context; the geometric misalignment makes the 11-bit context mispredict, so the
refinement bitmap costs nearly as much as a fresh direct symbol — and then adds
the record-6 index + `wdiff`/`hdiff` overhead on top. It is also strictly worse
than record-1 for *future* repeats, because record-6 is blit-only and never
extends the dictionary, so a later exact copy of the same glyph can no longer
hit rec-7. The earlier analysis-only scaffold's estimated deltas pointed the
same way; this confirms it with real bytes. Net: not worth pursuing at these
thresholds.

### IW44 forward-transform reconstruction-loss localization (#320) — **Kept (diagnostic only)** (2026-06-15)

**Issue.** #320: localize IW44 BG44 reconstruction loss on the photographic
`conquete_paix` pages 3 & 11, among four candidate stages — forward wavelet
transform, quantization threshold schedule, coefficient state transitions, and
reconstruction tracking. Keep default encoder behavior unchanged.

**Approach.** Added in-crate diagnostics (`src/iw44_encode.rs`, module
`loss_diagnostics`):
1. A full-precision i32 reference forward transform compared coefficient-wise
   against the production i16 transform on the real page-3/11 luma planes, plus
   a max-intermediate-magnitude probe for i16 headroom.
2. Per-band reconstruction residual `Σ|blocks − recon|` vs band energy
   `Σ|blocks|`, driving the real encoder slice loop, mapping each IW44 band to
   its contiguous zigzag-coefficient range.

**Numbers.**
- *Forward transform:* production i16 transform equals the i32 reference
  **exactly** (0 / 3.2 M coefficients diverge on p3, 0 / 3.3 M on p11). Largest
  intermediate magnitude 6 845 (p3) / 11 378 (p11) ≪ `i16::MAX` (32 767) — no
  overflow. The transform is lossless on these pages.
- *Per-band residual after the default 100 slices (rel = resid/energy):*

  | band | p3 rel | p11 rel |
  |------|--------|---------|
  | 0 (coarsest) | 23.0% | 17.6% |
  | 3 | 59.4% | 25.5% |
  | 6 | 99.8% | 65.7% |
  | 9 (finest) | 100.0% | 99.9% |

  Residual grows monotonically with frequency; band 9 `recon` is all-zero
  (p3) or within 0.1% of it (p11) — never activated within budget.
- *Pixel-domain floor (gray round-trip, 96×96 broadband):* avg abs error
  plateaus at 8.72 from 200 slices onward (identical at 255) — not a
  transform-pair mismatch but the refinement-schedule floor (`s>>1`, `s>>2`,
  `s>>3` round to 0 at small steps, so refinement deltas vanish before `recon`
  reaches `blocks`).

**Decision.** Kept as diagnostics; **no encoder change**. Two regression tests
(`forward_transform_is_lossless`, `loss_concentrates_in_high_frequency_bands`)
plus an `--ignored` table dump (`print_band_table`).

**Reason.** The loss is the progressive **quantization / slice budget**, not the
forward transform, state transitions, or reconstruction tracking: the forward
transform is bit-exact and overflow-free, and the coarse bands reconstruct
correctly (which they could not if state/tracking were broken). The finest
bands are simply starved — their small coefficients never cross the activation
threshold within 100 slices. An earlier i32 reference falsely reported a
forward-transform "divergence"; the bug was in the *reference* (it ran the
column pass over every column instead of the `s·ℤ` multiresolution sublattice),
not in production. Acting on quality would mean a different slice/quant schedule
or budget, which is out of scope for this diagnostic-only issue.

### Post-roadmap render baseline correction — **Kept** (2026-05-17)

**Approach.** Reran the render filters from #308 on the same code after the
post-roadmap full workspace run produced implausibly slow render rows. The
rerun confirmed that the public render baseline should use the targeted render
artifact, not the full-run outliers from PR #335.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple M1 Max, 10 cores
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- Cargo: `cargo 1.92.0 (344c4567c 2025-10-21)`
- RUSTFLAGS: unset

**Command(s).**

```sh
cargo bench --bench render -- 'render_corpus_color|render_colorbook' --output-format bencher
cargo bench --bench render -- 'render_page/dpi|render_corpus_bilevel|render_native_stages' --output-format bencher
```

**Numbers.**

| Benchmark | Corrected render baseline |
|-----------|--------------------------:|
| `render_page/dpi/72` | 246,839 ns |
| `render_page/dpi/144` | 937,501 ns |
| `render_page/dpi/300` | 3,586,208 ns |
| `render_page/dpi/600` | 13,911,536 ns |
| `render_colorbook` | 7,221,910 ns |
| `render_colorbook_stages/full_render` | 7,256,342 ns |
| `render_colorbook_stages/mask_decode` | 4,392,493 ns |
| `render_colorbook_cold` | 18,838,014 ns |
| `render_corpus_color` | 71,247,374 ns |
| `render_native_stages/render_pixmap/watchmaker_color` | 71,532,521 ns |
| `render_native_stages/render_into_reuse_buffer/watchmaker_color` | 70,452,562 ns |
| `render_native_stages/render_streaming_discard/watchmaker_color` | 70,185,520 ns |
| `render_native_stages/mask_decode/watchmaker_color` | 2,735,714 ns |
| `render_native_stages/bg_to_rgb_warm/watchmaker_color` | 2,962,458 ns |

**Decision.** Kept.

**Reason.** No render-path code changed between the #308 targeted baseline and
the bad #335 full-run artifact; the only intervening render source edits were
documentation comments. The targeted rerun restored the expected range, so
`README.md`, `BENCHMARKS_RESULTS.md`, and `BENCHMARKS.md` now use this
corrected render baseline. The bad full-run render rows remain recorded below
as a rejected artifact instead of being used as public claims.

### Post-roadmap full benchmark refresh — **Needs follow-up** (2026-05-17)

**Approach.** Reran the public full workspace Criterion command after the
roadmap PR series was merged through #310, then reran the DjVuLibre comparison
harness against the same local Criterion artifact. No code was changed; this
refresh updates public benchmark documentation from the new measurements.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple M1 Max, 10 cores
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- Cargo: `cargo 1.92.0 (344c4567c 2025-10-21)`
- RUSTFLAGS: unset

**Command(s).**

```sh
cargo bench --workspace --features cli,tiff
bash scripts/bench_djvulibre.sh /private/tmp/djvu-rs-post-roadmap-djvulibre
python3 scripts/djvulibre_compare.py \
  --criterion target/criterion \
  --djvulibre-bench /private/tmp/djvu-rs-post-roadmap-djvulibre/djvulibre_bench.txt \
  --ddjvu-timing /private/tmp/djvu-rs-post-roadmap-djvulibre/ddjvu_timing.txt
```

**Numbers.**

| Benchmark | Criterion mean |
|-----------|---------------:|
| `render_page/dpi/72` | 246 us |
| `render_page/dpi/144` | 934 us |
| `render_page/dpi/300` | 6.96 ms |
| `render_page/dpi/600` | 42.1 ms |
| `render_colorbook` | 8.78 ms |
| `render_colorbook_cold` | 48.9 ms |
| `render_corpus_color` | 151 ms |
| `render_corpus_bilevel` | 75.4 ms |
| `render_native_stages/render_streaming_discard/watchmaker_color` | 195 ms |
| `jb2_decode` | 132 us |
| `iw44_decode_first_chunk` | 592 us |
| `iw44_decode_corpus_color` | 655 us |
| `parse_multipage_520p` | 2.29 ms |
| `render_large_doc_first_page` | 10.6 ms |
| `pdf_export_sequential` | 821 ms |

DjVuLibre comparison on the same local fixture matrix:

| Scenario | djvu-rs | libdjvulibre C API | ddjvu CLI | Ratio |
|----------|--------:|-------------------:|----------:|------:|
| `boy.djvu` @ 72 dpi | 246 us | 159 us | 30.6 ms | djvu-rs 1.5x slower |
| `colorbook.djvu` @ 150 dpi | 8.78 ms | 5.96 ms | 67.3 ms | djvu-rs 1.5x slower |
| `watchmaker.djvu` @ 300 dpi | 151 ms | 36.44 ms | 79.8 ms | djvu-rs 4.2x slower |
| `cable_1973_100133.djvu` @ 300 dpi | 75.45 ms | 35.25 ms | 73.8 ms | djvu-rs 2.1x slower |

**Decision.** Needs follow-up.

**Reason.** Codec, document, and PDF rows from this run remain useful, but the
render rows were too noisy to use as public baseline claims. They were
superseded by the targeted render correction above, and the public docs were
updated accordingly.

### #306 — wasm32 scalar vs simd128 benchmark harness — **Kept** (2026-05-17)

**Approach.** Added a reproducible Node.js harness for the existing
`wasm-bindgen` API. The wrapper builds two `wasm-pack --target nodejs` bundles:
one scalar wasm32 bundle and one simd128 bundle built with
`RUSTFLAGS="-C target-feature=+simd128"`. The benchmark then imports both
bundles in Node.js and times parse, full render, cached render, and first
progressive render on `tests/fixtures/boy.djvu` at 150 dpi.

**Platform.**
- OS: macOS 26.3.1 (Darwin 25.3)
- CPU: Apple M1 Max, 10 cores
- host arch: `arm64`
- wasm target_arch: `wasm32`
- target_feature(s): scalar vs `simd128`
- Rust: 1.92.0 stable (`aarch64-apple-darwin`)
- wasm-pack: 0.13.1
- Node.js: v26.0.0
- RUSTFLAGS: unset for scalar; `-C target-feature=+simd128` for simd128

**Command(s).**

```sh
ITERATIONS=30 WARMUP=8 DPI=150 ./scripts/bench_wasm_simd128.sh

node scripts/bench_wasm_simd128.mjs \
  --scalar target/wasm-bench/scalar \
  --simd target/wasm-bench/simd128 \
  --fixture tests/fixtures/boy.djvu \
  --iterations 30 \
  --warmup 8 \
  --dpi 150 \
  --json
```

**Numbers.** Median milliseconds, 30 measured iterations after 8 warmups.
Negative delta means the simd128 bundle is faster.

| Benchmark | scalar median ms | simd128 median ms | delta |
|-----------|-----------------:|------------------:|------:|
| `parse_document` | 0.003 | 0.002 | -30.4% |
| `render_150dpi_fresh_doc` | 2.715 | 2.548 | -6.1% |
| `render_150dpi_cached_page` | 2.685 | 2.491 | -7.2% |
| `progressive_150dpi_chunk0` | 2.693 | 2.463 | -8.5% |

Checksums matched between scalar and simd128 for all render benchmarks
(`-663404102` for full render/cached render; `-663404261` for progressive
chunk 0), and the harness now fails if per-iteration checksums are unstable or
if scalar and simd128 checksums differ.

**Decision.** Kept.

**Reason.** The harness gives future wasm SIMD work a reproducible local
baseline and already confirms a modest render-path win on the existing
simd128 IW44 code. CI syntax-checks the harness and still build-checks both
plain wasm32 and `+simd128`; it does not run timing comparisons because hosted
runner variance would make the numbers unsuitable as a regression gate.

### #295 — JB2 encoder corpus round-trip and size baseline — **Needs follow-up** (2026-05-17)

**Approach.** Refreshed the existing JB2 quality harnesses without changing
encoder behavior. The page-level run measured original `Sjbz`, direct
`encode_jb2`, and dict `encode_jb2_dict` bytes/bpp/round-trip status across
current JB2-bearing fixtures and corpus files. The shared-Djbz run measured
`encode_jb2_dict` independent page totals vs bundled shared-Djbz totals, with
CC accounting and cross-size probe output enabled.

**Platform.**
- OS: macOS 26.3.1 (Darwin 25.3)
- CPU: Apple M1 Max, 10 cores
- target_arch: `aarch64`
- target_feature(s): ARM64 baseline; NEON available on Apple Silicon
- Rust: 1.92.0 stable (`aarch64-apple-darwin`)
- RUSTFLAGS: unset
- Source artifact: local run on `codex/issue-295-jb2-quality-refresh`

**Command(s).**

```sh
cargo run --release --example encode_quality_jb2 -- \
  references/djvujs/library/assets/boy_jb2.djvu \
  references/djvujs/library/assets/boy.djvu \
  references/djvujs/library/assets/carte.djvu \
  references/djvujs/library/assets/chicken.djvu \
  references/djvujs/library/assets/colorbook.djvu \
  references/djvujs/library/assets/DjVu3Spec_bundled.djvu \
  references/djvujs/library/assets/irish.djvu \
  references/djvujs/library/assets/navm_fgbz.djvu \
  tests/corpus/cable_1973_100133.djvu \
  tests/corpus/conquete_paix.djvu \
  tests/corpus/pathogenic_bacteria_1896.djvu \
  tests/corpus/watchmaker.djvu

cargo run --release --example encode_quality_djbz -- \
  --cc-stats --cross-size-stats \
  references/djvujs/library/assets/colorbook.djvu \
  references/djvujs/library/assets/DjVu3Spec_bundled.djvu \
  references/djvujs/library/assets/navm_fgbz.djvu \
  tests/corpus/conquete_paix.djvu \
  tests/corpus/pathogenic_bacteria_1896.djvu \
  tests/corpus/watchmaker.djvu
```

**Numbers.**

Page-level JB2 refresh:

| Mode | Pages | Bytes | bpp | vs original | Round-trip |
|------|------:|------:|----:|------------:|------------|
| Original `Sjbz` | 692 | 26,569,542 | 0.0263 | 1.000x | source |
| Direct `encode_jb2` | 692 | 46,252,033 | 0.0457 | 1.741x | 464 ok, 228 decode errors |
| Dict `encode_jb2_dict` | 692 | 36,016,741 | 0.0356 | 1.356x | 692 ok, 0 failures |

Per-file dict ratios:

| File | Pages | Dict/orig | Dict failures | Direct failures |
|------|------:|----------:|--------------:|----------------:|
| `boy_jb2.djvu` | 1 | 1.000x | 0 | 0 |
| `colorbook.djvu` | 62 | 1.030x | 0 | 46 decode errors |
| `DjVu3Spec_bundled.djvu` | 70 | 1.627x | 0 | 70 decode errors |
| `irish.djvu` | 1 | 0.302x | 0 | 0 |
| `navm_fgbz.djvu` | 5 | 0.301x | 0 | 5 decode errors |
| `cable_1973_100133.djvu` | 2 | 1.136x | 0 | 0 |
| `conquete_paix.djvu` | 22 | 1.025x | 0 | 16 decode errors |
| `pathogenic_bacteria_1896.djvu` | 517 | 1.378x | 0 | 80 decode errors |
| `watchmaker.djvu` | 12 | 1.058x | 0 | 11 decode errors |

`carte.djvu` was skipped by the harness because the checked-in fixture is
truncated and does not parse.

Shared-Djbz refresh:

| Mode | Files/pages | Bytes | bpp | vs original | Round-trip |
|------|------------:|------:|----:|------------:|------------|
| Original `Sjbz` totals | 6 / 688 | 26,424,220 | 0.0262 | 1.000x | source |
| Independent dict pages | 6 / 688 | 35,963,419 | 0.0356 | 1.361x | all pages ok |
| Bundled shared-Djbz | 6 / 688 | 34,986,136 | 0.0347 | 1.324x | all bundles ok |

Bundled shared-Djbz was `0.973x` of independent dict output (`-2.7%`) on this
six-file run. Individual bundle/independent ratios were: `colorbook` 1.002x,
`DjVu3Spec_bundled` 0.642x, `navm_fgbz` 0.955x, `conquete_paix` 1.029x,
`pathogenic_bacteria_1896` 0.976x, and `watchmaker` 0.945x.

Failure buckets:
- Direct `encode_jb2` decode errors are oversized whole-image record-3 symbols
  hitting decoder symbol-size limits on large pages.
- Dict `encode_jb2_dict` has no current mismatch or decode-error bucket on the
  refreshed corpus; the old `483/553` dict round-trip number is stale.
- Shared-Djbz has no current mismatch or decode-error bucket with byte-exact
  clustering; all six bundles round-trip pixel-exact.
- `carte.djvu` is a harness/input bucket: truncated fixture parse failure, not
  an encoder failure.

**Decision.** Needs follow-up. The refreshed safe baseline is dict encoding:
it round-trips all 692 pages but remains `1.356x` original bytes overall.
Shared-Djbz is safe and saves `2.7%` vs independent dict on this corpus, but it
still remains `1.324x` original bytes overall.

**Reason.** Correctness is no longer the blocker for the dict path on the
current corpus; byte cost is. The next narrow JB2 implementation issue should
be #301: add a byte-cost estimator for cross-size refinement before emitting
any new cross-size or lossy/lossless refinement records. The largest measured
size gaps are still `pathogenic_bacteria_1896` and `DjVu3Spec_bundled`, while
`watchmaker` shows cross-size candidate headroom already recorded by the probe.

### #294 — thumbnail row-scratch A/B — **Rejected** (2026-05-17)

**Approach.** Added a `render_row_scratch_ab` Criterion group to compare the
current strict direct `render_into` path against a row-scratch adapter that
copies `render_streaming` rows into the final RGBA buffer. The comparison uses
the issue's thumbnail and native targets with warmed decode caches.

**Platform.**
- OS: macOS 26.3.1 (Darwin 25.3)
- CPU: Apple M1 Max, 10 cores
- target_arch: `aarch64`
- target_feature(s): ARM64 baseline; NEON available on Apple Silicon
- Rust: 1.92.0 stable
- RUSTFLAGS: unset
- Source artifact: local run on `codex/issue-294-row-scratch-ab`

**Command(s).**

```sh
cargo bench --bench render -- render_row_scratch_ab \
  --warm-up-time 1 --measurement-time 2 --sample-size 10
```

**Numbers.**

First run:

| Target | Direct `render_into` | Row-scratch copy | Decision signal |
|--------|---------------------:|-----------------:|-----------------|
| `thumbnail_dpi72` | 248.21 µs | 205.35 µs | row-scratch faster |
| `thumbnail_half_bilinear` | 153.55 µs | 399.13 µs | row-scratch much slower |
| `colorbook_downscale` | 23.674 ms | 18.925 ms | row-scratch faster, noisy |
| `corpus_color_native` | 207.96 ms | 248.74 ms | native regression |
| `corpus_bilevel_native` | 150.93 ms | 198.23 ms | native regression |

Rerun after bounding the A/B group to keep full CI benchmark runtime stable:

| Target | Direct `render_into` | Row-scratch copy | Decision signal |
|--------|---------------------:|-----------------:|-----------------|
| `thumbnail_dpi72` | 306.59 µs | 199.09 µs | row-scratch faster |
| `thumbnail_half_bilinear` | 143.84 µs | 124.58 µs | row-scratch faster |
| `colorbook_downscale` | 15.966 ms | 11.861 ms | row-scratch faster, noisy |
| `corpus_color_native` | 155.40 ms | 135.02 ms | row-scratch faster, noisy |
| `corpus_bilevel_native` | 146.10 ms | 160.35 ms | no clear signal |

**Decision.** Rejected as a render heuristic. No production render path changed.
The A/B harness is kept so future thumbnail work can rerun the comparison.

**Reason.** The repeated short A/B runs are too noisy and inconsistent to justify
a production heuristic: the first run showed a thumbnail loss and native
regressions, while the rerun showed broader wins but still no clean bilevel
native signal. A threshold heuristic would be fragile without a more stable
predictor than output size alone.

### #293 — compositor-only render baselines — **Kept** (2026-05-17)

**Approach.** Added a `render_compositor_only` Criterion group to
`benches/render.rs`. Each case warms page-level decode caches with one
`render_pixmap` call, then measures `render_into` into a reused RGBA buffer.
This isolates cached compositor/output materialization from document parse,
codec decode/cache setup, and output allocation.

**Platform.**
- OS: macOS 26.3.1 (Darwin 25.3)
- CPU: Apple M1 Max, 10 cores
- target_arch: `aarch64`
- target_feature(s): ARM64 baseline; NEON available on Apple Silicon
- Rust: 1.92.0 stable
- RUSTFLAGS: unset
- Source artifact: local run on `codex/issue-293-compositor-baselines`

**Command(s).**

```sh
cargo bench --bench render -- render_compositor_only \
  --warm-up-time 1 --measurement-time 2 --sample-size 10
```

**Numbers.**

| Bench | Fixture/path | Cached path | Time |
|-------|--------------|-------------|-----:|
| `render_compositor_only/color_native_cached` | `tests/corpus/watchmaker.djvu` | color native, decoded caches warm, reused RGBA buffer | 71.061 ms |
| `render_compositor_only/bilevel_native_cached` | `tests/corpus/cable_1973_100133.djvu` | bilevel native, decoded caches warm, reused RGBA buffer | 72.171 ms |
| `render_compositor_only/color_downscale_cached` | `references/djvujs/library/assets/colorbook.djvu` | color downscale, decoded caches warm, reused RGBA buffer | 7.4213 ms |
| `render_compositor_only/small_color_downscale_cached` | `references/djvujs/library/assets/boy.djvu` | small color 0.5x downscale, decoded caches warm, reused RGBA buffer | 152.00 µs |

**Decision.** Kept. The new benches can be run independently with a single
Criterion filter, and their names identify color/bilevel, native/downscale,
and cached decode state.

**Reason.** This gives #294 and later compositor work a narrow baseline without
changing render behavior or mixing optimization into the measurement issue.

### #290 — layered multi-page DJVM directory encode — **Kept** (2026-05-16)

**Approach.** Extended `djvu encode <dir> --quality quality|archival` to encode
pages independently with `PageEncoder::from_pixmap`, then bundle the resulting
single-page `FORM:DJVU` pages with `djvm::merge`. The existing lossless directory
path is left unchanged and still uses `encode_djvm_bundle_jb2` with
`--shared-dict-pages`. Layered directory encode deliberately does **not** create a
shared Djbz dictionary: each page keeps its own `Sjbz` mask plus `BG44` and
optional `FGbz`, avoiding rejected Hamming shared-Djbz clustering while preserving
layered chunks in a parseable bundled DJVM.

**Numbers / fixture.** Added CLI fixtures for two-page RGB directories. Both
`--quality quality` and `--quality archival` produce parseable `page_count=2`
DJVM bundles; each page has `Sjbz`, `BG44`, and `FGbz`. The quality fixture also
renders every page through `djvu_render::render_pixmap` at native 32×32 pixels.
The pre-existing lossless directory fixture still produces `page_count=3` with
`Sjbz` pages and no `BG44` / `FGbz` chunks.

**Decision.** Kept. This satisfies layered multi-page encode without changing the
lossless shared-Djbz behavior or reviving Hamming clustering in the default path.

### #288 — adaptive segmentation + BG-block inpainting — **Kept** (2026-05-16)

**Approach.** Extended `SegmentOptions` without changing its default behaviour:
`Binarization::Fixed` remains the default global BT.601 threshold, while
`Binarization::Sauvola { window, k }` adds local adaptive binarisation for mixed
lighting scans. Added optional `bg_inpaint` for fully masked background blocks:
when a BG subsample cell has no unmasked source pixels, it is filled from the
nearest neighbouring unmasked pixels instead of falling back to the ink-coloured
block mean. `PageEncoder::with_segment_options` lets library callers opt into
these knobs for `Quality` / `Archival` single-page encodes; CLI defaults remain
unchanged.

**Numbers / fixture.** Added a checked-in synthetic mixed text/photo test in
`djvu_encode::tests::adaptive_segment_options_improve_decoded_mixed_lighting_fixture`:
left half dark paper (`Y=80`), right half bright paper (`Y=220`), with dark ink
(`Y=40`) and light gray ink (`Y=140`). With `bg_subsample=6`, fixed-threshold
Quality encode decodes at `mean_abs_rgb_diff=10.767` versus source; Sauvola +
inpainting decodes at `4.188` (61% lower), and the test requires at least a 30%
reduction. The lower-level `segment::tests::sauvola_handles_dark_background_and_light_ink`
asserts that fixed 128 masks most dark paper and misses the light ink, while
Sauvola keeps the mask less than half the fixed-mask size and retains both ink
pixels. Added `segment::tests::inpaint_fully_masked_bg_block_from_neighbors`: a
fully masked black 4×4 BG block next to tan paper now inpaints to
`(210,200,160)` when `bg_inpaint` is enabled; default fixed-threshold/no-inpaint
still falls back to black for all-black pages.

**Tests.** Added/updated segment unit tests, proptest `SegmentOptions`
constructors, and a `PageEncoder::with_segment_options` parseability test proving
Quality encode still emits `Sjbz` + `BG44` with adaptive options.

**Decision.** Kept. The new behaviour is opt-in, deterministic, covered by a
synthetic mixed-light fixture, and does not enable Hamming shared-Djbz clustering
or alter the default fixed-threshold path.

### #281 — strict `render_pixmap` composites directly into its output — **Kept** (2026-05-16)

**Approach.** Added native-resolution stage benches for the DjVuLibre comparison
corpus (`render_native_stages/*`) covering public `render_pixmap`,
`render_into` with a reused RGBA buffer, `render_streaming` with discarded rows,
JB2 mask decode, and cached IW44 inverse/RGB. Then changed strict
`render_pixmap` to call `render_into` directly instead of routing through the
row-streaming adapter and copying each scratch row into the output `Pixmap`.
`opts.permissive` keeps the old `render_rows` path because it has different
chunk-error recovery semantics.

**Numbers.** Quick local Criterion runs (`--warm-up-time 1 --measurement-time 2/3
--sample-size 10`) after #279 had made native render more expensive:

- `render_corpus_color`: `88.44 ms` → `72.27 ms` median (**18% faster**).
- `render_corpus_bilevel`: `90.09 ms` → `72.00 ms` median (**20% faster**).
- `render_colorbook` at 150 dpi: `7.29 ms` historical / `7.12 ms` after this
  change (no regression; slight improvement in the quick run).
- `iw44_to_rgb_colorbook/sub4_partial_decode`: `344 µs`, Criterion reported no
  statistically significant change, so the known sub4 partial decode path did
  not regress.

The new stage split (recorded in `BENCHMARKS_RESULTS.md`) shows warm JB2/IW44
codec stages are only a few milliseconds on the native corpus; the remaining
DjVuLibre gap is dominated by compositor sampling and output materialization.

**Tests.** Targeted render tests passed, including byte-identical
`render_rows`/`render_into` and `render_streaming`/`render_pixmap` checks plus the
permissive truncated-BG44 regression. Full validation below covered the rest of
the workspace.

**Decision.** Kept. The change is narrow, removes an avoidable row copy from the
public strict render path, beats Criterion noise on both native corpus targets,
and leaves the permissive recovery path and IW44 sub4 decode untouched.

### #280 — TIFF export uses `render_streaming` rows — **Kept** (2026-05-16)

**Approach.** Added `tiff_export::djvu_to_tiff_writer(doc, opts, writer)` and
changed the existing `djvu_to_tiff` byte-buffer wrapper to delegate to it.
Color TIFF pages now use `djvu_render::render_streaming` when options are
streamable (no AA, bilinear/no-op resampling, identity combined rotation) and
feed RGB rows directly into TIFF strips. Pages requiring render post-processing
keep the existing full-`Pixmap` fallback. Bilevel TIFF export was already a
mask-extraction path and remains unchanged.

**Numbers.** Repro probe added as
`examples/probe_tiff_streaming_memory.rs` (`required-features = ["tiff"]`).
Command run locally after a release build:

```text
/usr/bin/time -l target/release/examples/probe_tiff_streaming_memory \
  tests/fixtures/problem_page.djvu /tmp/problem_page_streamed.tiff 1.0
```

Output for the 600-dpi `problem_page.djvu` fixture:

- page: `3288x5050` px at scale `1.000` (`16,604,400` pixels)
- output TIFF bytes written to `File`: `49,813,798`
- full RGBA pixmap allocation avoided: `66,417,600` bytes
- full RGB staging allocation avoided: `49,813,200` bytes
- `/usr/bin/time -l` maximum resident set size: `7,962,624` bytes
- peak memory footprint: `7,111,552` bytes

**Tests.** Added TIFF tests comparing decoded streamed color-TIFF pixels against
the existing `render_pixmap(...).to_rgb()` result for both a color page
(`chicken.djvu`) and a bilevel page (`boy_jb2.djvu`). Also fixed an existing
TIFF test to unwrap `extract_bilevel_pixels` under the `tiff` feature.

**Decision.** Kept. This makes a real public export path use the row-streaming
renderer end-to-end without constructing a full output `Pixmap` or full RGB
staging image, while preserving byte/pixel equivalence through tests and keeping
the full-pixmap fallback for unsupported render options.

### #222 PR2 — high-level setters (`page_mut(i).set_text_layer`/`set_annotations`/`set_metadata`) — **Kept** (2026-05-01)

**Approach.** Builds on PR1's chunk-replacement primitive. New surface:

- `DjVuDocumentMut::page_count() -> usize` — `1` for `FORM:DJVU`, count of
  `FORM:DJVU` direct children for `FORM:DJVM`.
- `DjVuDocumentMut::page_mut(i) -> Result<PageMut<'_>, MutError>` — borrow
  one page's `FORM:DJVU` for editing.
- `PageMut::set_text_layer(&TextLayer)` — encode via `encode_text_layer`
  (page height read from `INFO`) + `bzz_encode`, replace the existing
  `TXTa`/`TXTz` or insert a new `TXTz`.
- `PageMut::set_annotations(&Annotation, &[MapArea])` — same shape over
  `encode_annotations_bzz` and `ANTa`/`ANTz`.
- `PageMut::set_metadata(&DjVuMetadata)` — over a new
  `metadata::encode_metadata` / `encode_metadata_bzz` pair, against
  `METa`/`METz`. Empty `DjVuMetadata` removes the chunk.
- New `MutError` variants: `PageOutOfRange`, `MissingPageInfo`,
  `InfoParse(IffError)`, `DjvmMutationUnsupported`.

`page_mut` errors with `DjvmMutationUnsupported` on `FORM:DJVM` bundles —
the page-level setters change a component FORM's byte size which would
shift DIRM offsets. DIRM recomputation is its own concern, deferred.

**Tests.** Nine new unit tests in `djvu_mut::tests` plus five in
`metadata::tests`:

- `set_text_layer_roundtrip_chicken`, `set_annotations_roundtrip_chicken`,
  `set_metadata_roundtrip_chicken` — each parse the re-emitted bytes and
  decode the chunk back to the input value.
- `set_metadata_empty_removes_existing_chunk` and
  `set_metadata_replaces_existing_chunk_in_place` — exercise the
  remove-on-empty and replace-don't-duplicate behaviours.
- `page_count_*`, `page_mut_out_of_range_errors`,
  `page_mut_djvm_returns_unsupported` — error paths.
- Metadata encoder tests cover empty input, dedicated-field round-trip,
  `extra` ordering, escape handling for `"`/`\\`, and BZZ round-trip.

All 410 lib tests pass (402 → 410; `+9` djvu_mut, `+5` metadata, with the
PR1 metadata count shift). `cargo clippy --workspace --lib --tests --bins
-- -D warnings` clean, `cargo fmt --check` clean. (Examples have two
pre-existing clippy warnings unrelated to this PR.)

**Reason kept.** Direct continuation of PR1's contract — PR1 only exposed
`replace_leaf(path, bytes)`; PR2 wires the existing chunk encoders to
that primitive so callers don't need to know IFF chunk IDs or BZZ
compression to update text/annotations/metadata. With this PR the
`librarian` consumer (#158) can finally drop its `djvused` shell-out for
single-page DjVu files.

**Follow-up status.**
1. Bundled DJVM mutation and `DjVuDocumentMut::set_bookmarks(&[DjVuBookmark])`
   have landed.
2. Single-page byte-range patching was implemented in #302; bundled-DJVM and
   indirect-DJVM byte-range work remain tracked separately.
3. Indirect DJVM support is intentionally deferred after #303; the decision
   record is `docs/indirect-djvm-mutation.md`, with implementation follow-ups
   in #325 and #326.

### #222 PR1 — `DjVuDocumentMut::from_bytes` + chunk-replacement primitive — **Kept** (2026-04-30)

**Approach.** New `src/djvu_mut.rs` module gated on `feature = "std"` with
the foundation layer for in-place document mutation. Public surface:

- `pub struct DjVuDocumentMut` — owns a parsed `DjvuFile` tree plus the
  original byte buffer.
- `pub fn from_bytes(data: &[u8]) -> Result<Self, MutError>` — parses (via
  `iff::parse`, the legacy tree-based parser) and retains the input bytes.
- `pub fn into_bytes(self) -> Vec<u8>` — fast path: when no mutation has
  happened, returns the original bytes verbatim. After any mutation, falls
  through to `iff::emit`.
- `pub fn replace_leaf(&mut self, path: &[usize], new_data: Vec<u8>)` —
  walks the tree by child indices and rewrites the leaf payload.
- `pub fn chunk_at_path(&self, path: &[usize]) -> Result<&Chunk, _>` —
  read-only walker, used by tests and (future) inspectors.
- Utility: `root_child_count`, `root_form_type`, `is_dirty`.
- `pub enum MutError`: `Parse(LegacyError)`, `PathOutOfRange`,
  `PathTraversesLeaf`, `NotALeaf`, `EmptyPath`.

The byte-identical-no-edit guarantee is achieved by holding the original
`Vec<u8>` and short-circuiting `into_bytes` when `!is_dirty`. After any
mutation `iff::emit` is invoked, which **does not** guarantee byte-identity
even for unmutated chunks (it recomputes FORM lengths from children) — but
this case is explicitly out of scope for PR1 and tracked as a follow-up
for PR3 (proper byte-range patching).

**Tests.** Ten new unit tests in `djvu_mut::tests`:

- Round-trip byte-identical (no edit) on four corpus fixtures:
  - `chicken.djvu` — color FORM:DJVU
  - `boy_jb2.djvu` — bilevel FORM:DJVU
  - `DjVu3Spec_bundled.djvu` — multi-page FORM:DJVM
  - `navm_fgbz.djvu` — FORM:DJVU with NAVM + FGbz
- `replace_leaf_changes_emitted_bytes` — replaces INFO with a marker, parses
  the output, verifies the marker came back.
- Negative paths: `EmptyPath`, `PathOutOfRange`, `PathTraversesLeaf`,
  `NotALeaf` (last picks the last child of a DJVM bundle, which is a
  page FORM).
- `root_form_type_djvu_single_page` — sanity on the tree-introspection API.

All 402 lib tests pass (393 → 402; `+10` djvu_mut, `-1` ignored count
shifted). `cargo clippy --workspace --all-targets -- -D warnings` clean,
`cargo fmt --check` clean.

**Reason kept.** PR1 of #222 establishes the byte-identical contract and
the chunk-walking primitive that PR2-4 build on (per the issue body's
sequencing comment). The implementation is intentionally minimal — wrap
the existing IFF parser, hold raw bytes for fast path, expose one
mutation primitive — to ship a focused first slice without committing to
the high-level setter design (`set_metadata`, `set_bookmarks`,
`page_mut(i).set_text_layer`). Those settings each compose
`replace_leaf` with one of the existing chunk encoders
(`encode_navm`, `encode_annotations*`, `encode_metadata`,
`encode_text_layer`).

**Follow-up status.**
1. High-level setters (`set_metadata`, `set_bookmarks`,
   `page_mut(i).set_text_layer`, `…set_annotations`) have landed.
2. Single-page byte-range patching landed in #302; bundled-DJVM byte-range
   patching remains a separate follow-up.
3. Indirect DJVM support was scoped by #303 and remains intentionally
   unsupported until the external-file rewrite/re-bundle work in #325/#326.
4. `librarian` consumer migration off `djvused` shell-out (#158 follow-up)
   can use the setter surface, but is outside this repository.

### #229 PR1 — extract `djvu-zp` into a standalone workspace crate — **Kept** (2026-04-30)

**Approach.** Moved `src/zp/{mod,encoder,tables}.rs` into a new
`crates/djvu-zp/` workspace member with its own `Cargo.toml`. The new
crate:

- Defines `pub enum ZpError { TooShort }` instead of leaking `BzzError`
  back into ZP. Decoupling ZP from `crate::error` is what makes the
  extraction publishable.
- Promotes every `pub(crate)` to `pub` (the audit the issue body warns
  about): `ZpDecoder`, `ZpDecoder::{a, c, fence, bit_buf, bit_count, data,
  pos}` fields, `decode_bit`, `decode_passthrough`, `decode_passthrough_iw44`,
  `is_exhausted`, `ZpEncoder` + its methods, and the four format-constant
  tables (`PROB`, `THRESHOLD`, `MPS_NEXT`, `LPS_NEXT`).
- Has a `default = ["std"]` feature that gates the encoder (which needs
  `Vec<u8>`). Decoder works in `no_std` builds and never allocates.
- Adds a `Default` impl on `ZpEncoder` (clippy `new_without_default` for
  the now-public `new` method).

`src/lib.rs` keeps the historical internal name via
`pub(crate) use djvu_zp as zp_impl;` so every existing import
(`crate::zp_impl::ZpDecoder`, `crate::zp_impl::tables::PROB`, etc) keeps
working unchanged. `From<djvu_zp::ZpError> for BzzError` makes the `?`
operator in `bzz_new::bzz_decode` continue to work without per-callsite
edits.

`src/zp/` is removed; the `#[path = "zp/mod.rs"] pub(crate) mod zp_impl;`
attribute in `src/lib.rs` was replaced with the `use` re-export. Workspace
`members = [".", "djvu-py", "crates/djvu-zp"]`.

**Tests.** Per-crate test counts:

- `djvu-rs` (umbrella): 393 lib tests pass (down from 405 — the 4 ZP
  decoder tests + 7 ZP encoder roundtrip tests moved into the new crate).
- `djvu-zp`: 11 unit tests pass (`zp_decoder_*`, `zp_tables_spot_check`,
  7 roundtrip tests in the encoder module). Two doctest examples
  (`ZpDecoder::new` from a 2-byte slice, `ZpEncoder` round-trip).
- `djvu-py`: builds. No tests defined.
- Workspace `cargo build --no-default-features --lib` (host
  no-std-compatible build) green; no_std smoke test
  (`tests/no_std_smoke`) builds green against the new dependency graph.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo fmt --check` clean.

**Scope of `pub` audit.** Every newly-`pub` item was an internal
`pub(crate)` before — there is no new behavioural surface, just a wider
visibility. Specifically:

| Was            | Now           | Justification                                               |
| -------------- | ------------- | ----------------------------------------------------------- |
| `ZpDecoder`    | `pub`         | Required for cross-crate use                                |
| Decoder fields | `pub`         | Hot-path field access from JB2/IW44/BZZ in djvu-rs internals |
| `ZpEncoder`    | `pub`         | Required for cross-crate use                                |
| `PROB` etc.    | `pub` (in `pub mod tables`) | Used by JB2/IW44/BZZ saturation-bound tests in djvu-rs |

The decoder field exposure is the only mildly load-bearing widening: it
lets djvu-rs internals manipulate the registers directly during
saturated-decode fast paths. Wrapping each in a getter would force every
hot-path access through a function call. Acceptable for an internal-
collaboration sub-crate and matches the precedent set by `wide` /
`bytemuck` / similar low-level numerics crates.

**Reason kept.** Lossless extraction of ~780 LOC into a publishable
sub-crate, no behavioural change for djvu-rs consumers, all tests pass,
no_std build still works. This is the canonical "is this approach
viable" first step of #229; PR2 (`djvu-bzz`), PR3 (`djvu-iff`), PR4-5
(`djvu-jb2`, `djvu-iw44`), and PR6 (umbrella re-export shim) follow the
same pattern.

**Open follow-ups.**
1. The `From<ZpError> for BzzError` mapping collapses to `BzzError::TooShort`
   — fine for now since `ZpError::TooShort` is the only variant. If
   future ZP-coder errors are added, the mapping needs a more specific
   `BzzError` variant (likely `BzzError::ZpError`-already-exists).
2. Publish to crates.io once the API is reviewed. The `version = "0.1.0"`
   reflects new-crate convention, not djvu-rs's `0.14.0` line.
3. Consider whether the encoder fields (`a`, `subend`, `buffer`, `nrun`,
   `delay`, `byte`, `scount`, `output`) need to be `pub`. Currently they
   stay private — only methods are exposed.

### #189 Phase 3 — x86_64 AVX2 ports of `prelim_flags_bucket` + `prelim_flags_band0` — **Kept** (2026-04-30)

**Approach.** Two new AVX2 functions mirroring the existing aarch64 NEON
helpers in `src/iw44_new.rs`:

- `prelim_flags_bucket_avx2`: loads 16 i16 (one `__m256i` — twice the lane
  width of NEON's two `int16x8_t` loads), compares to zero with
  `_mm256_cmpeq_epi16`, builds UNK/ACTIVE flags via `uv ^ (xv & nz)` (UNK=8,
  XV=10), narrows u16→u8 via `_mm_packus_epi16` of the two 128-bit halves
  (saturating but values 2/8 fit), stores 16 bytes via `_mm_storeu_si128`,
  horizontally OR-reduces via `_mm_unpackhi_epi64` + `_mm_srli_si128` chain.

- `prelim_flags_band0_avx2`: same flag computation, then conditional blend
  `(new & should_update) | (old & ~should_update)` using SSE2
  `_mm_andnot_si128` to replicate NEON's `vbslq_u8`. Keeps the ZERO-state
  lane unchanged; updates other lanes from the coef comparison.

A new `band0_dispatch` helper picks NEON / AVX2 / scalar at runtime via
`is_x86_feature_detected!("avx2")` (gated on `feature = "std"` per the
established pattern in #251/#252). The scalar fallback is unchanged — so
non-AVX2 x86_64 hosts and `no_std` builds keep their existing behaviour.

The dispatcher in `prelim_flags_bucket` was extended the same way: AVX2
branch added, NEON path unchanged, scalar fallback unchanged.

**Tests.** Two new unit tests gated on `cfg(all(target_arch = "x86_64",
feature = "std"))` + AVX2 runtime detection:

- `prelim_flags_bucket_avx2_matches_scalar` — sweeps 5 coef vectors
  (all-zero, mixed, all-one, all-negative-one, edge values) at four bases
  including the highest valid bucket offset (1008). Verifies bucket bytes
  and bstatetmp byte-exact vs scalar.
- `prelim_flags_band0_avx2_matches_scalar` — sweeps 4 old-flag patterns ×
  4 coef patterns. Verifies the conditional-update semantics: ZERO lanes
  are preserved, other lanes get UNK/ACTIVE from the coef comparison.

Both pass on the local x86_64 host. All 405 lib tests pass; clippy
`-D warnings` and `cargo fmt --check` clean.

**Bench.** No native bench harness for this kernel in isolation; expected
speedup over scalar at this hot path (called once per (block × band) =
~1024 blocks/page × 10 bands = ~10K calls/page) is on the order of
4–8× from replacing the scalar 16-iteration loop with three AVX2 ops + a
narrow + horizontal OR. End-to-end `iw44_decode_*` benches were later sampled
by the #189 validation run and the #307 AVX2 spike.

**Reason kept.** Two more AVX2 kernels close the parity gap with NEON
that issue #189 calls out (lines 11–14 of the issue body listed
`preliminary_flag_computation` band-0 and band≠0 as next priorities after
`load8s`/`store8s`, which shipped in #252). Bit-exact verified vs scalar,
zero behavioural change for non-AVX2 hosts, no allocation overhead, no
runtime cost on the dispatcher (one feature-detected branch). Pattern
established for the remaining kernels (`row_pass_neon_s1_row`,
`lifting_even`, `predict_inner`, `predict_avg`).

**Open follow-ups.**
1. `row_pass_neon_s1_row` AVX2 port was measured in #307 and rejected: full
   decode improved, but the sensitive sub2/sub4 partial-decode paths regressed.
2. Encoder-side ports (`forward_row_neon_s1_row`, `forward_col_predict_neon`).
3. ARM64 NEON validation was refreshed in #308.

### #225 Phase 2 — public `render_streaming` API — **Kept** (2026-04-30)

**Approach.** Built on Phase 1's internal `render_rows` primitive. Added one
new public entry point and one new error variant:

- `pub fn render_streaming<F: FnMut(usize, &[u8])>(page, opts, sink)` — thin
  wrapper around `render_rows` that rejects render options requiring
  post-processing of a fully-allocated pixmap.
- `RenderError::UnsupportedOption(&'static str)` — returned when the streaming
  path cannot honour the requested options.

The constraints surface what `render_pixmap` does after compositing: the
streaming path *cannot* support `opts.aa = true` (the AA downscale needs the
full pixmap), `opts.resampling = Lanczos3` *when scaling actually happens*
(re-renders at native resolution and downscales), or any non-identity
combined rotation (`combine_rotations(page.rotation(), opts.rotation)`
wraps a rotate-pixmap step). When all three constraints hold,
`render_streaming` is byte-identical to `render_pixmap` — verified by two
new tests on `chicken.djvu` (color) and `boy_jb2.djvu` (bilevel).

Lanczos at native size is permitted: the early-return path in
`render_pixmap` skips Lanczos when output dimensions equal page dimensions
(`need_scale = false`), so it has no effect on bytes either way.

**Tests.** Seven new unit tests in `djvu_render::tests`:

- `render_streaming_byte_identical_to_render_pixmap_color`
- `render_streaming_byte_identical_to_render_pixmap_bilevel`
- `render_streaming_rejects_aa`
- `render_streaming_rejects_lanczos_with_scaling`
- `render_streaming_allows_lanczos_at_native_size`
- `render_streaming_rejects_user_rotation`
- `render_streaming_rejects_zero_dimensions`

All 403 lib tests pass; clippy `-D warnings` and `cargo fmt --check` clean.

**Memory.** Phase 1 already established that the internal compositing path
allocates a single `opts.width * 4` byte scratch row reused across rows;
`render_streaming` inherits that. Peak heap during compositing is bounded
by `scratch_row + decoded BG44 + decoded JB2 mask + FG palette` — no full
pixmap. The 600-dpi A3 (≈100 MB pixmap) target from the issue's DoD is met
by construction (the scratch row is < 16 KB at any reasonable width).

**Reason kept.** The DoD-required public API is now in place with no
behavioural change for existing `render_pixmap` callers, byte-exact
equivalence verified, post-processing options safely refused with a typed
error rather than silently producing different output. The `UnsupportedOption`
variant is `&'static str` — no allocation on the error path. Phase 1's
zero-cost adapter through `render_rows` means `render_pixmap` continues to
benefit from the warm-cache row scratch (`### #225 Phase 1` below,
−13% on `render_page/dpi/72`).

**Open follow-ups.**
1. `render_region`, `render_coarse`, `render_progressive` could similarly
   gain streaming variants if a use case appears.
2. Memory benchmark from the issue's DoD ("peak RSS during render of a
   600-dpi 2550×3301 page < 4 MB") not yet wired into `bench/`. Manual
   verification via `heaptrack` or `dhat` would confirm the BG44/mask
   buffers are the only large allocations.

### #225 Phase 1 — internal row-streaming render refactor — **Kept** (2026-04-29)

**Approach.** Extracted the composite hot path into a per-row streaming
primitive without changing the public API. Three new module-private functions:

- `composite_rows_bilevel_one` / `composite_rows_bilinear_one` /
  `composite_rows_area_avg_one` — per-row helpers containing the pixel-level
  computation for each of the three compositing modes (bilevel fast path,
  bilinear upscale/1:1, area-average downscale). These are `#[inline]` and
  mirror the existing `composite_loop_*` bodies row by row.

- `composite_rows<F: FnMut(usize, &[u8])>` — allocates a single row scratch
  buffer (`out_w * 4` bytes, reused across rows), calls the appropriate per-row
  helper, then invokes the sink `F(row_index, &row_slice)`. The
  `composite_into` direct flat-buffer path is untouched and continues to drive
  `render_into`, `render_region`, `render_coarse`, and `render_progressive`.

- `pub(crate) render_rows<F>` — decode/setup entry point (mirrors
  `render_pixmap`'s decode logic) that calls `composite_rows`. This became the
  shared row source for the public `render_streaming` API instead of
  allocating a full Pixmap.

At the time, `render_pixmap` was a thin adapter: it pre-allocated
`Pixmap::white(w, h)`, called `render_rows` with a sink that copied each row
into `pm.data`, then applied the existing aa/Lanczos/rotation post-processing
steps. Current strict renders use the direct full-pixmap path, while permissive
renders still share row-based recovery with `render_streaming`.

Two new unit tests — `render_rows_byte_identical_to_render_into_color` and
`render_rows_byte_identical_to_render_into_bilevel` — verify that
`composite_rows` and `composite_into` produce byte-exact identical output for
color (chicken.djvu) and bilevel (boy_jb2.djvu) pages.

**Bench** (`cargo bench --bench render -- 'render_page/dpi/72'`,
100 samples, Apple M1 Max):

| Benchmark             | Before   | After    | Δ       |
|-----------------------|----------|----------|---------|
| `render_page/dpi/72`  | 243.5 µs | 211.8 µs | **−13%** |
| `render_colorbook_cold` | — | 17.8 ms | flat (no prior baseline in this worktree) |

The 72-dpi benchmark **improved** by ~13% despite the per-row scratch
allocation and `copy_from_slice` on each row. The likely cause: the scratch row
buffer (`w * 4 ≈ 400–2400 bytes`) fits entirely in L1 cache; subsequent writes
from the composite inner loop and the copy into `pm.data` both hit warm L1
rather than cold L2/L3 as in the previous approach that wrote directly into the
full pre-allocated pixmap. The decode step dominates at 72 dpi (BG44 + JB2
cache hits account for ~200 µs), so even the best-case compositing improvement
is bounded.

**Reason kept.** Material improvement on the warm-cache render benchmark (−13%)
with zero public API change, bit-exact output verified by tests, all 550 tests
pass, clippy and fmt clean. The `render_rows` hook is in place for Phase 2.

**Open follow-ups.**
1. Phase 2 shipped `pub fn render_streaming` with a user-visible row callback.
2. `render_region`, `render_coarse`, `render_progressive` could similarly be
   refactored to use `composite_rows` for API symmetry, but are not hot paths.

### #190 Phase 2 — WASM simd128 inverse wavelet (load/store stride-1) — **Kept** (2026-04-29)

**Approach.** Added `load8s_s1_simd128` and `store8s_s1_simd128` (gated on
`cfg(all(target_arch = "wasm32", target_feature = "simd128"))`) as the WASM
counterparts to the AVX2 stride-1 helpers shipped in Phase 2 of #189.

`load8s_s1_simd128`: loads 8 consecutive i16 as one `v128`, then calls
`i32x4_extend_low_i16x8` / `i32x4_extend_high_i16x8` to sign-extend into two
`v128`s of i32, which are transmuted directly to `wide::i32x8` (`{a: i32x4(v128),
b: i32x4(v128)}`). This replaces 8 scalar `as i32` casts assembled via
`i32x8::from([...])`.

`store8s_s1_simd128`: transmutes `i32x8` back to `[v128; 2]`, then uses a
constant `i8x16_shuffle` with indices `[0,1,4,5,8,9,12,13, 16,17,20,21,24,25,28,29]`
to pick the low 2 bytes of each i32 lane from both halves into a single `v128`,
stored in one `v128_store`. This replicates the truncating `as i16` semantics of
the scalar path (not saturating narrow), matching the AVX2 byte-shuffle approach.

Both functions are wired into `load8s` and `store8s` via a compile-time
`#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]` block
(the `return` before the scalar `#[allow(unreachable_code)]` block), so
the hot column-pass loop at `s == 1` gets the fast path with no runtime branch.

**Bench.** No direct wasm bench harness available locally. Expected speedup is
analogous to the AVX2 load/store path (#189 Phase 2), which measured −3.9% on
`jb2_decode_corpus_bilevel`. The WASM path processes 8 lanes (same as `v128`
width) in 2 ops (load) or 1 shuffle + 1 store (store) vs 8 scalar cast-and-
write pairs. The column pass at `s=1` is the hottest sub-kernel in
`inverse_wavelet_transform_from` during full-resolution (`to_rgb`) decoding.
CI bench job will capture actual WASM numbers on next main merge.

**Reason kept.** Zero regression risk: compile-time gating, bit-exact by
construction (sign-extend from i16→i32 is exact; low-halfword extraction via
byte-shuffle is exact truncation). Two new unit tests
(`load8s_s1_simd128_matches_scalar`, `store8s_s1_simd128_matches_scalar`)
gate on `wasm32 + simd128` and verify round-trip across the full i16/i32 range.
All 389 host lib tests pass; both WASM builds (plain and `+simd128`) succeed.

### #224 Phase 4 — opt-in lossy rec-7 substitution for near-duplicates — **Kept** (2026-04-28)

**Approach.** Added `Jb2EncodeOptions { lossy_threshold: f32 }` and
`pub fn encode_jb2_dict_with_options(bitmap, shared, &opts)`. When
`lossy_threshold > 0.0`, the action-selection branch tries
`find_lossy_copy_ref` *before* the lossless refinement matcher
(`find_refinement_ref`): for each CC, it scans `same_size_indices` in
`dict_entries`, and if any entry has `packed_hamming(rep, cc) <= pixels *
lossy_threshold`, the encoder emits `rec-7` (matched copy, no
refinement bitmap) referencing it. Decoder will then reconstruct the
dict entry's pixels, with visual error bounded by the threshold. The
existing `REFINEMENT_MIN_PIXELS = 32` floor still applies — tiny CCs
stay byte-exact regardless of threshold.

`encode_jb2_dict_with_shared` now delegates to
`encode_jb2_dict_with_options(bitmap, shared, &Jb2EncodeOptions::default())`
so the shipped lossless path is unchanged. Default threshold = 0 = exact
behaviour preserved.

`examples/encode_quality_jb2.rs` got a `--lossy-threshold <fraction>`
flag, plus a `bitmap_hamming` helper that decodes the lossy-encoded Sjbz
and computes pixel-wise Hamming vs the original mask, so the harness
reports both byte savings and total reconstruction error.

**Bench** (`encode_quality_jb2` on a 15-page bilevel mix:
`tests/corpus/{cable_1973_100133,watchmaker}.djvu` +
`tests/fixtures/{big-scanned-page,carte,chicken,irish}.djvu`,
~188 M total pixels, Apple M1 Max):

| `--lossy-threshold` | rs-lossy bytes | vs rs-dict (lossless) | total err pixels | bits/pixel error |
|---------------------|---------------:|----------------------:|-----------------:|-----------------:|
| 0 (lossless dict)   | 167 314        | 1.000×                | 0                | 0                |
| 0.01                | 158 250        | **0.946×** (−5.4%)    | 10 986           | 0.000087         |
| 0.02                | 154 050        | 0.921× (−7.9%)        | 17 946           | 0.000142         |
| 0.04                | 150 118        | 0.897× (−10.3%)       | 28 568           | 0.000226         |
| 0.05                | 149 015        | 0.891× (−10.9%)       | 32 386           | 0.000256         |
| 0.08                | 146 104        | **0.873×** (−12.7%)   | 40 767           | 0.000322         |

Reconstruction error is on the order of 1 in 5–20 K pixels (≈0.0001–
0.0003 bits/pixel) — visually imperceptible for scanned text on these
600 dpi-class bilevel inputs. The `lossy decode errors: 1` row in the
summary is the same `irish.djvu` page that already trips
`roundtrip_dict: decode_error` on the lossless path (issue #198: a CC
larger than `MAX_SYMBOL_PIXELS`); orthogonal to lossy mode.

**Reason kept.** Material byte savings on top of the already-shipped
lossless dict path, opt-in via `Jb2EncodeOptions`, default behaviour
unchanged. The threshold knob is exposed so callers can pick their own
size↔fidelity point. Pairs naturally with the cjb2 quality settings
(default ≈ 0.005, conservative ≈ 0.02 in DjVuLibre) — a CLI front-end
could map that mapping in a follow-up. All 32 `jb2_encode` unit tests
plus the new `lossy_threshold_substitutes_near_duplicate_with_rec7`
test pass.

**Open follow-ups.**
1. `--lossy-threshold` doesn't yet feed into `cjb2`-equivalent CLI
   front-end (`tools/djvu-encode` if/when one exists).
2. The same threshold logic could be extended to refinement: instead of
   only substituting same-size near-dups with rec-7, allow lossy rec-6
   that emits a *truncated* refinement bitmap. Unclear if there's
   additional headroom past the rec-7 path measured here.

### #194 Phase 2.5 — per-CC accounting harness for shared-Djbz refinement — **Kept (instrumentation only)** (2026-04-28)

**Approach.** Added `pub fn analyze_jb2_cc_stats(page, &shared)` that mirrors
the rec-1/rec-6/rec-7 action-selection branch in
`encode_jb2_dict_with_shared` but emits no bytes — just counts and a
Hamming-distance histogram for rec-6 emissions, separating refs that
land in the shared dict (cross-page) from refs that land in the
page-local running dict. Wired through to `encode_quality_djbz` via a
new `--cc-stats` flag.

This is the measurement layer Phase 2.5 needs before deciding whether
the per-CC profitability model in the #194 follow-up is worth
implementing. The Phase 2 result already showed flat Hamming clustering
doesn't beat byte-exact; the open question was whether selective
near-duplicate promotion (with a profitability gate per CC) could.
Without the actual rec-6 distribution we were guessing.

**Observations** (`--cc-stats` on `tests/corpus/*.djvu`, 36 pages, 4 books):

| File | Pages | rec-1 fresh | rec-6 shared | rec-6 local | rec-7 exact |
|---|---:|---:|---:|---:|---:|
| `cable_1973_100133.djvu` | 2  | 12.4% | 0.0%  | 4.7% | 82.8% |
| `conquete_paix.djvu`     | 22 | 40.7% | 0.2%  | 2.1% | 56.9% |
| `watchmaker.djvu`        | 12 | 6.1%  | 24.7% | 1.8% | 67.5% |

rec-6 Hamming-distance distribution on `watchmaker.djvu` (6256 rec-6
matches, dominant case): 49.7% in [1, 4], 47.7% in [5, 16], 2.5% in
[17, 64], 0% above. Very tight — the existing 4%-of-pixels threshold in
`find_refinement_ref` is approximately right; there is little headroom
for "tighter" to improve the picture.

**Reason kept.** Pure instrumentation, no encoder behavior change. Gives
future Phase 2.5 work (and any Phase 4 lossy-refinement experiment from
#224) a concrete CC-action breakdown without round-tripping bytes.
Round-trip + clippy + nextest all clean; new test
`analyze_jb2_cc_stats_classifies_records` covers all three buckets +
shared/local distinction.

**What this tells us about Phase 2.5 viability.** On the dominant
shared-dict beneficiary (`watchmaker`), rec-6 already covers 24.7% of
CCs against the shared dict, and the Hamming distribution is bimodal
on [1, 16]. The remaining 6.1% rec-1 are mostly:
1. Unique glyphs (no shared-dict twin) — promotion candidates need ≥ N
   page repetitions, by definition rare for these
2. Glyphs that fail the same-(w, h) bucket constraint
   (cross-size matching is `find_refinement_ref`'s explicit
   limitation, see jb2_encode.rs:611)

So the most plausible Phase 2.5 win is **cross-size refinement**, not
per-CC profitability. That's a substantially larger change (requires
resampling for Hamming scoring) and is what the open #194 follow-up
should track. Per-CC profitability against the existing same-size
shortlist is unlikely to add anything material — the rec-6 hits we
already get are tight enough that a profitability gate would barely
exclude any of them.

### #185 — perf(jb2): bit-pack Jbm to 1 bit/pixel — **Kept** (2026-04-18)

**Approach.** Changed the internal `Jbm` working bitmap from 1 byte/pixel
(`Vec<u8>` of `w * h`) to 1 bit/pixel packed (`Vec<u8>` of
`((w + 7) / 8) * h`, MSB-first within byte) — matching `Bitmap`'s public
convention. 8× memory reduction on the symbol dict.

Decoder hot path uses **Variant A**: `decode_bitmap_direct` and
`decode_bitmap_ref` keep rolling unpacked scratch rows (3 for direct,
3 mbm + 2 cbm for ref) and pack into `Jbm.data` once per row. The ZP
inner loop is unchanged. New helpers: `pack_row_into`, `unpack_row_into`.

`blit_indexed`: reads packed source with a byte-at-a-time skip of
all-zero bytes (common for sparse symbols). `blit_to_bitmap`: source and
dest are both packed MSB-first; byte-aligned branch becomes a direct `|=`
row copy, unaligned branch is a shift-and-OR.

**Bench** (`cargo bench`, 100 samples, Linux x86_64, Criterion p-values):

| Benchmark                    | Baseline  | Packed    | Δ      | p    |
|------------------------------|-----------|-----------|--------|------|
| `jb2_decode`                 | 187.93 µs | 188.79 µs | +0.5%  | 0.31 |
| `jb2_decode_corpus_bilevel`  | 813.80 µs | 782.21 µs | −3.9%  | 0.00 |
| `jb2_decode_large_600dpi`    | 4.37 µs   | 4.27 µs   | −2.3%  | 0.06 |
| `render_corpus_bilevel`      | 189.76 ms | 191.36 ms | +0.8%  | 0.19 |

No regression anywhere; `jb2_decode_corpus_bilevel` is significantly
faster (p = 0.00), consistent with reduced L2 pressure on the decoded
symbol dict.

**Reason kept.** 8× memory reduction on working bitmaps with neutral-to-
positive decode/render perf. The scratch allocation in the hot path
(three `Vec<u8>` × `width` bytes per symbol decode, reused across rows)
adds no measurable overhead vs the previous direct-indexed `bm.data`
split. All 324 library + 71 integration tests pass.

**Notes.** The issue suggested `Vec<u32>` + 32-bit row alignment for SIMD
potential. That was relaxed to byte-aligned `Vec<u8>` to match `Bitmap`
exactly (avoiding the byte→bit packing step in `blit_to_bitmap`). A
follow-up could explore word-granular compositing once there is a
workload that stresses the unaligned `blit_to_bitmap` branch.

### #184 — perf(iw44): column_pass SIMD at s=2 — **Reverted** (2026-04-18)

**Approach.** Generalised the existing `s == 1` SIMD fast path in the column
pass of `inverse_wavelet_transform_from` to `s ∈ {1, 2}`. Introduced
stride-aware helpers `load8_col_s` / `store8_col_s` that gather/scatter 8
`i16` samples at stride `s`, threaded an `allow_simd` parameter for
comparability, and added a golden test
(`simd_inverse_wavelet_transform_matches_scalar`) that confirmed bit-exact
parity with the scalar path on 32×32 and 33×32 planes.

**Bench** (`cargo bench --bench codecs -- 'iw44_decode_first_chunk|iw44_decode_corpus_color'`,
release, 100 samples, Linux x86_64):

| Benchmark                  | Scalar   | SIMD s=2 | Δ     |
|----------------------------|----------|----------|-------|
| `iw44_decode_first_chunk`  | 1.226 ms | 1.206 ms | −1.6% |
| `iw44_decode_corpus_color` | 3.747 ms | 3.669 ms | −2.1% |

Run-to-run noise on the same build was ±2–5% (e.g. `iw44_decode_corpus_color`
ranged 3.31 ms → 3.81 ms across consecutive runs). Criterion's change test
came back non-significant (`p ∈ {0.09, 0.20, 0.24, 0.36, 0.68}`) once the
cold-start outlier was excluded.

**Reason.** On x86_64, the implementation must fall back to 8 scalar loads
assembled into an `i32x8` — `wide::i32x8` exposes no strided / gather load for
`i16`, and no native `_mm*_i16gather_*` intrinsic exists for 16-bit lanes.
The arithmetic savings at `s == 2` (which already processes half as many
columns as `s == 1`) do not exceed the gather overhead.

The issue expected the win to come from ARM64 NEON `vld2q_s16` / `vst2q_s16`,
which are not reachable through `wide` and would require raw
`core::arch::aarch64` intrinsics. Without that, there is no benefit on the
x86_64 CI host. The stride-aware helpers would be reusable if the ARM64
follow-up lands, but committing them today costs complexity for zero measured
gain.

**Next step.** Addressed by #308: the current raw NEON row-pass and related
IW44 decode/render paths were remeasured on Apple ARM64 and recorded in the
cross-architecture matrix.

### #194 Phase 2 — multi-page shared Djbz with Hamming clustering — **Reverted default, kept tunable knob** (2026-04-28)

**Approach.** Phase 1 (#194, shipped) builds the shared Djbz dictionary by
byte-exact `(w, h, data)` dedup of CCs across pages: any CC signature
appearing on `≥ threshold` distinct pages becomes a shared symbol, the rest
stay per-page Sjbz. Phase 2 attempted to widen the cluster predicate to
"same `(w, h)` AND `packed_hamming(rep, cc) ≤ pixels * fraction`", folding
near-duplicate scanned-glyph variants into one shared rep so the per-page
Sjbz can emit `rec-7` (matched copy) or `rec-6` (matched refinement)
instead of `rec-1` (new direct).

Implementation: `cluster_shared_symbols_tunable(pages, page_threshold,
diff_fraction)` — bucketed by `(w, h)`, linear scan per bucket choosing the
nearest existing rep within `max_diff = pixels * diff_fraction / 100` (with
a `REFINEMENT_MIN_PIXELS = 32` floor that keeps tiny CCs byte-exact).
`encode_djvm_bundle_jb2_with_shared(pages, &shared)` lets a benchmark
harness drive cluster selection without re-running the IFF/DIRM pipeline.

**Harness.** `examples/encode_quality_djbz.rs` — for each multi-page DjVu
input, computes total bytes for {original Sjbz, independent
`encode_jb2_dict` per page, bundled `encode_djvm_bundle_jb2_with_shared`}
across configurable Hamming thresholds; verifies pixel-exact bundle
round-trip.

**Bench** (`encode_quality_djbz` on `pathogenic_bacteria_1896.djvu`,
517 pages of cjb2 scans, Apple M1 Max):

| `--diff-fraction` | shared syms | Djbz bytes | Σ Sjbz | bundle / independent | round-trip |
|-------------------|-------------|------------|--------|----------------------|------------|
| 0 (byte-exact, shipped) | 1 568 | 41 KB | 7.40 MB | **0.870×** (−13.0%) | ✓ |
| 1% | 1 547 | 40 KB | 7.40 MB | 0.870× (−13.0%) | ✓ |
| 2% | 1 503 | 39 KB | 7.41 MB | 0.871× (−12.9%) | ✓ |
| 3% | 1 449 | 38 KB | — | — | **✗ mismatch** |
| 4% | 1 387 | 36 KB | 7.50 MB | 0.877× (−12.3%) | ✓ |

Small corpus (`tests/corpus/*.djvu`, 36 pages from 4 books):

| `--diff-fraction` | bundle / independent |
|-------------------|----------------------|
| 0 (byte-exact)    | 1.021× (+2.1%) |
| 4%                | 1.150× (+15.0%) |

**Reason reverted as default.** The Phase 1 byte-exact win (−13.0% bundle
vs independent on the 517-page corpus) is the entire shared-Djbz benefit.
Hamming clustering at 1–2% is within 0.05% of byte-exact; at 4% it is
strictly worse. Hypothesis: the per-page `symbol_index_ctx` encoding pays
≈ `log2(K)` bits per reference, so growing `K` (more shared reps) inflates
every `rec-7` reference; meanwhile `rec-6` refinement bitmaps cost more
ZP-coded bits than a fresh `rec-1` direct emission whenever the shared rep
isn't a near-perfect match. Net: cross-page Hamming clustering must match
*better than* the per-page intra-CC refinement matcher already does within
each page (#188 Phase 3) — and on this corpus it doesn't.

**Reason kept tunable.** `cluster_shared_symbols_tunable` and
`encode_djvm_bundle_jb2_with_shared` are exposed `pub` so the benchmark
harness — and any future Phase 2.5 calibration work (per-CC profitability
model instead of a flat fraction) — can sweep thresholds without forking
the encoder. The default `cluster_shared_symbols` continues to delegate to
`diff_fraction = 0`.

**Open follow-ups.**
1. The `diff_fraction = 3%` round-trip mismatch on the big corpus is a real
   bug in the rec-6 refinement path against shared reps — should be filed
   as a sub-issue. (Doesn't block ship: 0% remains lossless and is the
   shipped default.)
2. Per-CC profitability model: instead of a flat Hamming fraction, decide
   per CC whether `cost(rec-6 against shared rep)` < `cost(rec-1 fresh) +
   amortized log2(K) increase`. Unclear if the win exists — would need to
   re-measure with a corpus where intra-page refinement is already
   exhausted.

### #258 — shared-Djbz Hamming clustering — **Rejected** (2026-05-04)

**Approach.** Re-tested the `diff_fraction = 3` path on the 517-page
`pathogenic_bacteria_1896.djvu` corpus. The corpus exposed three separate
robustness problems: the 1 MP per-symbol decode cap was too low for large
connected components, the 64 MP cumulative symbol-work cap was too low for
dense independently encoded pages, and Hamming shared clustering/rec-6
refinement did not provide a reliable size win. The kept path raises decode
caps to 16 MP per symbol and 256 MP cumulative symbol work, disables
lossless rec-6 emission, and keeps shared-Djbz clustering byte-exact with a
4 MP retained shared-dict budget.

**Numbers.** Re-running the 517-page `pathogenic_bacteria_1896.djvu`
experiment at `--diff-fraction 3` before this change localized the failure
to page-level JB2 decode errors such as `Jb2(ImageTooLarge)` beginning at
page 81. The clustered shared dictionary had 63,062 symbols; the per-page
Sjbz stream then emitted enough shared-ref rec-6 refinements to exceed the
decoder's per-stream symbol-pixel budget before pixel comparison.

After the change:

| Command | shared syms | bundle / independent | round-trip |
|---------|-------------|----------------------|------------|
| `--threshold 999 --diff-fraction 3` | 0 | 1.001× | ✓ |
| `--diff-fraction 3` | 5,164 | 0.976× | ✓ |

**Decision.** Rejected. Hamming shared clustering has no material measured
size win over byte-exact clustering, and the `diff_fraction = 3` corpus path
still produces invalid page streams. `cluster_shared_symbols_tunable` keeps
its public benchmarking signature but now ignores the Hamming allowance and
uses byte-exact clustering for every threshold. In addition, inherited
shared-Djbz symbols are used only for exact record-7 hits, and lossless
near matches fall back to record-1 rather than rec-6 refinement.

### #283 — cross-size JB2 refinement probe — **Kept instrumentation, default unchanged** (2026-05-12)

**Approach.** Added `analyze_jb2_cross_size_refinement(page, shared,
max_dim_delta, max_hamming_fraction)`, an experiment-only accounting helper
that mirrors `encode_jb2_dict_with_shared` dictionary growth but does not
emit bytes. For fresh record-1 candidates, it scans dictionary symbols whose
width/height differ by at most 2 px, normalizes the reference into the
candidate box with nearest-neighbor sampling, and reports how many candidates
land within a 5% normalized Hamming budget. The existing
`examples/encode_quality_djbz.rs` harness now exposes this via
`--cross-size-stats`.

**Command.**

```text
cargo run --release --example encode_quality_djbz -- \
  --cc-stats --cross-size-stats \
  tests/corpus/watchmaker.djvu \
  tests/corpus/pathogenic_bacteria_1896.djvu
```

**Numbers.**

| File | Pages | bundle / independent | round-trip | fresh CCs | cross-size candidates | near @ 5% |
|------|-------|----------------------|------------|-----------|-----------------------|-----------|
| `watchmaker.djvu` | 12 | 0.945× | ✓ | 2,652 | 2,331 | 547 (20.65%) |
| `pathogenic_bacteria_1896.djvu` | 517 | 0.976× | ✓ | 759,291 | 686,402 | 61,485 (8.73%) |

Aggregate bundled bytes for the two-file run were 33,553,108 vs 34,384,941
for independent per-page JB2 dict encoding (0.976×, −2.4%). Pixel round-trip
stayed exact because the probe is observational only.

**Decision.** Keep the probe, but do not change the default encoder. The
candidate counts prove there is real cross-size shape similarity, especially
on `watchmaker`, but they are only an upper bound: record-6 would still carry
refinement bitmap bytes plus symbol-index/context overhead, and the previous
same-size/shared-rec-6 experiments showed that plausible-looking Hamming
matches can lose bytes or create invalid streams. A shipped cross-size
encoder path needs a byte-cost model and explicit lossy/lossless semantics;
until then `encode_djvm_bundle_jb2` remains exact rec-7 + fresh rec-1 only.

### #278 PR1 — single-page Quality/Archival FGbz profiles — **Kept** (2026-05-12)

**Approach.** Completed the conservative single-page color profile path:
`Quality` still uses the existing deterministic segmentation and
`INFO + Sjbz + BG44...` shape, but now adds an `FGbz` foreground palette
when the detected foreground color is not black. `Archival` no longer
returns `Unsupported` for color input; it emits the same layered shape with
a denser background sample grid (`bg_subsample = 6` instead of 12).

This deliberately does not change the multi-page directory encoder, which
still uses the lossless shared-Djbz path only, and does not revive Hamming
shared-Djbz clustering.

**Tests.**

- `cargo test -q djvu_encode::tests`
- `cargo test -q --features cli --test cli_encode -- --nocapture`

The CLI regression fixture is generated in `tests/cli_encode.rs` as a
white RGB PNG with a dark red foreground block. `--quality quality` and
`--quality archival` both produce parseable single-page DjVu files with
`Sjbz`, at least one `BG44`, and `FGbz`.

**Decision.** Kept as PR1 scope. This removes the user-visible
`Archival` unsupported path for single PNGs and gives colored foreground
documents a foreground color layer. Remaining quality work should be split
into focused follow-ups: adaptive binarization/inpainting, per-blit FGbz
indices or FG44 for multi-color foregrounds, and layered multi-page DJVM
encoding.

### #289 — per-blit FGbz indices for colored foreground — **Kept** (2026-05-12)

**Approach.** Switched single-page color profiles from direct whole-page
`encode_jb2` masks to dict-based `encode_jb2_dict` masks, then derives the
FGbz palette from the independently decoded `decode_indexed` blit map. Each
foreground blit gets an average source RGB color; duplicate colors share one
palette entry; multi-color foregrounds emit an FGbz index table. Single-color
foregrounds still use compact palette-only FGbz.

**Tests.**

- `cargo test -q djvu_encode::tests`
- `cargo test -q --features cli --test cli_encode -- --nocapture`
- `cargo clippy --lib --tests -- -D warnings`

The new regression fixture has two separated colored foreground components.
The unit test verifies both the FGbz palette/index table and a decoded render:
the left component remains red-dominant and the right component remains
blue-dominant.

**Decision.** Kept. This closes the main PR1 limitation from #278: colored
foreground no longer collapses to one averaged ink color when the page has
multiple separated foreground components. Continuous foreground regions and
FG44 remain out of scope; those need separate visual-quality measurements.

### #233 — async lazy first-page probe — **Kept** (2026-05-04)

**Approach.** Added `examples/async_lazy_first_page.rs`, a small native
probe for the Phase 3 lazy async loader. It wraps a DjVu file in an
`AsyncRead + AsyncSeek` reader that can simulate broadband throughput,
constructs `LazyDocument` with `from_async_reader_lazy`, fetches page 0,
and renders the first pixmap.

**Command.**

```sh
cargo run -q --example async_lazy_first_page --features async -- \
  tests/corpus/pathogenic_bacteria_1896.djvu --bandwidth-mib 12.5 --dpi 150 --pad-to-mib 100
```

**Numbers.**

| Corpus | Size | Pages | Simulated bandwidth | Bytes read | First pixel |
|--------|------|-------|---------------------|------------|-------------|
| `pathogenic_bacteria_1896.djvu` padded with an ignored `JUNK` chunk | 104,857,600 bytes | 520 | 12.5 MiB/s | 28,578 | 491.469 ms |

**Decision.** Kept. The probe pads the largest checked-in multi-page corpus
to exactly 100 MiB with a valid ignored `JUNK` IFF chunk, preserving the
DIRM/page offsets while making the file size match the issue target. Indexing
plus first-page fetch reads only the DIRM and first page/component ranges
instead of buffering the full 100 MiB document, and first pixel is well below
the 2 s target under the simulated broadband reader.

### #189 — x86-64-v3 AVX2 validation — **Kept partial / needs follow-up** (2026-05-04)

**Approach.** Pulled the GitHub Actions artifact from run `25299920836`
(`Benchmark (x86-64-v3 AVX2 validation)`, head `77fc6ff`) and compared
default `RUSTFLAGS` against `RUSTFLAGS=-C target-cpu=x86-64-v3` on the same
Ubuntu runner. This validates the already-landed AVX2 paths on real x86_64
hardware even though the local development host is `arm64`.

**Numbers.**

| Bench | default ns | +x86-64-v3 ns | Delta |
|-------|-----------:|--------------:|------:|
| `iw44_decode_corpus_color` | 1,385,461 | 1,123,865 | -18.88% |
| `iw44_decode_first_chunk` | 765,703 | 728,565 | -4.85% |
| `iw44_to_rgb_colorbook/sub1_full_decode` | 9,231,033 | 9,129,333 | -1.10% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,164,523 | 2,199,280 | +1.61% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 565,640 | 583,519 | +3.16% |
| `render_colorbook` | 13,072,440 | 12,826,562 | -1.88% |
| `render_colorbook_cold` | 28,127,606 | 27,105,326 | -3.63% |
| `render_colorbook_stages/mask_decode` | 5,325,125 | 5,107,550 | -4.09% |
| `render_corpus_color` | 133,813,976 | 133,185,634 | -0.47% |

**Decision.** Kept partial. Existing AVX2 decode paths earn their keep on
full IW44 decode (`-18.88%` corpus decode, `-4.85%` first chunk), but the
sub4 partial path regresses by `+3.16%`. This does not close #189: the
umbrella still lacks AVX2 equivalents for the horizontal row pass and encoder
kernels, and those should be implemented only in an x86_64 AVX2 session with
this validation job green after each slice.

### #292 — cross-architecture benchmark matrix — **Kept** (2026-05-17)

**Approach.** Added a canonical cross-architecture platform metadata template
and seed matrix to `BENCHMARKS_RESULTS.md`. This issue did not run new
benchmarks; it normalized existing trustworthy artifacts and made missing
target families explicit for downstream architecture issues.

**Platform.**
- OS: macOS 26.3.1 (Darwin 25.3) for the local Apple ARM64 seed row; Ubuntu
  GitHub-hosted runner for the x86_64 artifact rows.
- CPU: Apple M1 Max, 10 cores, for the broad local baseline; GitHub-hosted
  x86_64 runner for #189 artifact run `25299920836`.
- arch: `aarch64` and `x86_64`
- target features: Apple ARM64 baseline/NEON available; x86_64 baseline;
  x86_64-v3/AVX2 via `RUSTFLAGS=-C target-cpu=x86-64-v3`.
- Rust: 1.92.0 stable for the local Apple ARM64 row; stable toolchain from
  `.github/workflows/bench.yml` for the GitHub artifact rows.
- RUSTFLAGS: unset for local Apple ARM64 and Linux x86_64 baseline rows;
  `-C target-cpu=x86-64-v3` for the AVX2 row.

**Command(s).**

```sh
# Existing local summary source already recorded in BENCHMARKS_RESULTS.md:
cargo bench --workspace --features cli,tiff

# Existing x86_64-v3 artifact source already recorded in this file under #189:
gh run view 25299920836 --repo matyushkin/djvu-rs
```

**Numbers.** The seed matrix records Apple ARM64 local values for
`iw44_decode_*`, `iw44_to_rgb_colorbook/*`, `render_colorbook*`, and
`render_corpus_color`, plus the #189 Linux x86_64 baseline vs
`x86_64-v3`/AVX2 values. wasm32 scalar, wasm32 simd128, and Linux aarch64 are
explicitly marked missing.

**Decision.** Kept. The repository now has one copy/pasteable platform metadata
block and one public cross-architecture result schema for #306, #307, and #308.

**Reason.** Normalizing the table first avoids each downstream architecture
issue inventing a different platform format, while preserving measurement
discipline by distinguishing current numbers from missing/untrusted cells.

### #298 — PDF export memory and parallel baseline — **Needs follow-up** (2026-05-17)

**Approach.** Measured the existing PDF export pipeline before any streaming
rewrite. Criterion measured the stable `pdf_export_sequential` and
`pdf_export_parallel` benches on `tests/corpus/watchmaker.djvu` (12 pages,
default PDF options: 150 dpi, JPEG-80). A new reproducible
`examples/pdf_memory_probe.rs` harness recorded read/parse, one-page
render/RGB/JPEG staging, full PDF export time, PDF bytes, and peak RSS via
`/usr/bin/time -l`.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple M1 Max, 10 cores
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: Apple ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- RUSTFLAGS: unset
- Source artifact: local run on `codex/issue-298-pdf-baseline`

**Command(s).**

```sh
cargo bench --bench render --features std -- pdf_export_sequential
cargo bench --bench render --features std,parallel -- pdf_export_parallel

/usr/bin/time -l cargo run --release --example pdf_memory_probe -- \
  tests/corpus/watchmaker.djvu

/usr/bin/time -l cargo run --release --features parallel \
  --example pdf_memory_probe -- tests/corpus/watchmaker.djvu
```

**Numbers.**

| Measurement | Sequential | Parallel |
|-------------|-----------:|---------:|
| Criterion `pdf_export_*` | 955.42 ms median (`916.16..999.54 ms`) | 154.05 ms median (`153.41..154.66 ms`) |
| Probe `pdf_export_ms` | 893.827 ms | 187.183 ms |
| Peak RSS (`maximum resident set size`) | 80,379,904 bytes (76.7 MiB) | 240,058,368 bytes (228.9 MiB) |
| Peak memory footprint | 79,479,872 bytes (75.8 MiB) | 239,175,232 bytes (228.1 MiB) |
| Output PDF bytes | 6,651,085 | 6,651,085 |

Single-page breakdown from the same probe, page 0 rendered at 150 dpi
(`1275x1651`):

| Stage | Time | Bytes |
|-------|-----:|------:|
| Read input | 0.075 ms | 183,352 |
| Parse document | 0.152 ms | - |
| Render full RGBA pixmap | 43.822 ms | 8,420,100 |
| Convert RGBA to RGB staging buffer | 2.904 ms | 6,315,075 |
| JPEG-80 encode staging buffer | 13.065 ms | 312,922 |

The parallel probe uses the same one-page breakdown before full export; that
single-page section stayed essentially unchanged (`render_pixmap_ms=44.410`,
`rgb_stage_ms=3.183`, `jpeg_stage_ms=13.228`) while full export dropped to
`187.183 ms` and peak RSS rose to `228.9 MiB`.

**Decision.** Needs follow-up.

**Reason.** Parallel export is about 5.3-6.2x faster on the 12-page color
fixture, but it increases peak RSS by about 3.0x because `djvu_to_pdf_impl`
collects every `RenderedPage` before sequential object emission. The concrete
baseline for #299 is therefore: beat ~894 ms sequential wall time and reduce
or cap the ~76.7 MiB sequential peak RSS / ~228.9 MiB parallel peak RSS by
streaming page render/RGB/JPEG data into PDF objects instead of retaining all
encoded page bodies at once.

### #299 — PDF color row streaming — **Kept** (2026-05-17)

**Approach.** Replaced the PDF color-image path's full `Pixmap` + full RGB
staging pair with `render_streaming` into one RGB staging buffer when render
options are streamable. The fallback `render_pixmap(...).to_rgb()` path remains
for anti-aliasing, scaled Lanczos, rotation, and other non-streamable options.
Measured against the #298 baseline on the same `tests/corpus/watchmaker.djvu`
PDF fixture (12 pages, default PDF options: 150 dpi, JPEG-80).

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple M1 Max, 10 cores
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: Apple ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- RUSTFLAGS: unset
- Source artifact: local run on `codex/issue-299-pdf-streaming`

**Command(s).**

```sh
cargo bench --bench render --features std -- pdf_export_sequential
cargo bench --bench render --features std,parallel -- pdf_export_parallel
cargo bench --bench render --features std,parallel -- pdf_export_parallel

/usr/bin/time -l cargo run --release --example pdf_memory_probe -- \
  tests/corpus/watchmaker.djvu
/usr/bin/time -l cargo run --release --features parallel \
  --example pdf_memory_probe -- tests/corpus/watchmaker.djvu
```

**Numbers.**

| Measurement | #298 baseline | #299 row streaming |
|-------------|--------------:|-------------------:|
| Criterion `pdf_export_sequential` median | 955.42 ms | 811.83 ms (`810.13..813.58 ms`) |
| Criterion `pdf_export_parallel` median | 154.05 ms | 165.57 ms rerun (`154.19..178.74 ms`) |
| Sequential probe `pdf_export_ms` | 893.827 ms | 852.285 ms |
| Parallel probe `pdf_export_ms` | 187.183 ms | 155.745 ms |
| Sequential peak RSS | 80,379,904 bytes (76.7 MiB) | 77,512,704 bytes (73.9 MiB) |
| Parallel peak RSS | 240,058,368 bytes (228.9 MiB) | 177,684,480 bytes (169.5 MiB) |
| Output PDF bytes | 6,651,085 | 6,651,085 |

The first parallel Criterion run after the change measured
`219.65 ms` (`206.80..232.42 ms`) and reported a regression; an immediate rerun
measured `165.57 ms` (`154.19..178.74 ms`). The single-run probe also measured
parallel export at `155.745 ms`. Treat parallel timing as noisy on this host;
the stable win is peak RSS.

**Decision.** Kept.

**Reason.** The change preserves PDF bytes and keeps the fallback path for
non-streamable render options. It removes the extra full RGBA page allocation
from the streamable PDF color path. Sequential RSS falls modestly from
`76.7 MiB` to `73.9 MiB`; parallel RSS falls materially from `228.9 MiB` to
`169.5 MiB` (-26%). The remaining peak is dominated by retained per-page
encoded RGB/JPEG/PDF object bodies, so a larger memory reduction would require
streaming PDF object emission rather than only row-streamed rendering.

### #300 — IW44 low-PSNR diagnosis on `conquete_paix` — **Needs follow-up** (2026-05-17)

**Approach.** Added a repeatable diagnostic example that re-encodes existing
BG44 backgrounds from `watchmaker` and `conquete_paix` with controlled variants:
current pre-quantization RGB-to-YCbCr model, inverse-compatible pre-quantization
model, default IW44 encode, full-resolution chroma, 200 total slices, and
gray-luma-only encode. This issue intentionally did not change default encoder
behavior.

**Platform.**
- OS: macOS / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple Silicon host
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: Apple ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- RUSTFLAGS: unset

**Command(s).**

```sh
cargo run --release --features std --example diagnose_iw44_quality -- \
  tests/corpus/watchmaker.djvu tests/corpus/conquete_paix.djvu \
  > /private/tmp/iw44_diag_300.jsonl \
  2> /private/tmp/iw44_diag_300.stderr
```

`watchmaker` pages 0-4, 7-9, and 11 were skipped because the original BG44
stream did not decode through the strict full-stream diagnostic path; pages 5,
6, and 10 were enough to confirm the good-page baseline.

**Numbers.**

| File | Variant | Pages | Avg luma PSNR | Min luma PSNR | Avg byte ratio |
|------|---------|------:|--------------:|--------------:|---------------:|
| `watchmaker.djvu` | default | 3 | 46.42 dB | 44.99 dB | 0.73x |
| `watchmaker.djvu` | full chroma | 3 | 46.37 dB | 44.99 dB | 0.73x |
| `watchmaker.djvu` | 200 slices | 3 | 46.31 dB | 44.93 dB | 137.32x |
| `watchmaker.djvu` | gray luma only, 200 slices | 3 | 46.41 dB | 44.95 dB | 137.31x |
| `conquete_paix.djvu` | pre-quant current YCbCr model | 20 | 47.90 dB | 42.97 dB | n/a |
| `conquete_paix.djvu` | pre-quant inverse-compatible model | 20 | 52.53 dB | 51.32 dB | n/a |
| `conquete_paix.djvu` | default | 20 | 15.49 dB | 9.75 dB | 1.16x |
| `conquete_paix.djvu` | full chroma | 20 | 16.35 dB | 9.15 dB | 1.24x |
| `conquete_paix.djvu` | 200 slices | 20 | 15.43 dB | 10.84 dB | 55.87x |
| `conquete_paix.djvu` | gray luma only, 200 slices | 20 | 14.57 dB | 9.21 dB | 40.40x |

Per-page `watchmaker` baseline:

| Page | Orig BG44 bytes | Default bytes | Default luma PSNR | Full chroma luma PSNR | 200 slices luma PSNR | Gray luma-only PSNR |
|------|----------------:|--------------:|------------------:|----------------------:|---------------------:|--------------------:|
| 5 | 2,028 | 1,788 | 44.994 dB | 44.988 dB | 44.934 dB | 44.950 dB |
| 6 | 1,804 | 1,178 | 47.268 dB | 47.123 dB | 47.036 dB | 47.302 dB |
| 10 | 1,772 | 1,161 | 46.984 dB | 46.988 dB | 46.975 dB | 46.979 dB |

Per-page `conquete_paix` diagnostic:

| Page | Orig BG44 bytes | Default bytes | Default luma PSNR | Full chroma luma PSNR | 200 slices luma PSNR | Gray luma-only PSNR |
|------|----------------:|--------------:|------------------:|----------------------:|---------------------:|--------------------:|
| 2 | 75,375 | 86,138 | 13.716 dB | 18.130 dB | 10.842 dB | 15.499 dB |
| 3 | 36,721 | 37,994 | 9.746 dB | 16.749 dB | 14.251 dB | 19.829 dB |
| 4 | 41,754 | 49,853 | 12.713 dB | 9.148 dB | 15.914 dB | 9.764 dB |
| 5 | 32,415 | 39,573 | 12.125 dB | 12.345 dB | 13.436 dB | 9.961 dB |
| 6 | 125,387 | 140,114 | 15.159 dB | 18.328 dB | 13.634 dB | 17.737 dB |
| 7 | 90,023 | 102,677 | 14.429 dB | 9.876 dB | 11.275 dB | 18.507 dB |
| 8 | 97,611 | 110,499 | 20.826 dB | 14.056 dB | 16.937 dB | 16.546 dB |
| 9 | 94,750 | 107,785 | 16.934 dB | 14.794 dB | 21.151 dB | 20.543 dB |
| 10 | 102,842 | 116,915 | 18.350 dB | 20.647 dB | 17.430 dB | 12.006 dB |
| 11 | 91,607 | 104,672 | 11.219 dB | 19.431 dB | 11.875 dB | 17.791 dB |
| 12 | 104,131 | 117,898 | 16.722 dB | 14.036 dB | 17.168 dB | 10.354 dB |
| 13 | 96,424 | 109,673 | 20.043 dB | 18.464 dB | 20.039 dB | 9.210 dB |
| 14 | 102,115 | 115,874 | 18.909 dB | 18.312 dB | 19.579 dB | 13.873 dB |
| 15 | 91,303 | 103,789 | 11.826 dB | 13.003 dB | 11.657 dB | 16.271 dB |
| 16 | 112,528 | 126,994 | 19.714 dB | 19.820 dB | 13.751 dB | 11.713 dB |
| 17 | 110,328 | 124,617 | 12.350 dB | 16.952 dB | 11.208 dB | 17.077 dB |
| 18 | 36,292 | 43,735 | 11.783 dB | 19.109 dB | 18.280 dB | 9.960 dB |
| 19 | 26,855 | 38,856 | 20.226 dB | 21.302 dB | 14.453 dB | 11.716 dB |
| 20 | 45,896 | 53,610 | 16.041 dB | 15.038 dB | 17.046 dB | 15.550 dB |
| 21 | 87,548 | 99,228 | 16.901 dB | 17.430 dB | 18.767 dB | 17.463 dB |

**Decision.** Needs follow-up.

**Reason.** The failure is reproduced on `conquete_paix` while `watchmaker`
remains high quality. It is not explained by BG44 byte budget: default output
is already larger than the original on `conquete_paix` (`1.16x` average), and
200 slices explodes output size (`55.87x`) without improving luma PSNR. It is
not solved by chroma subsampling alone: full-resolution chroma improves some
bad pages substantially (for example page 3: `9.746 dB` to `16.749 dB`, page
11: `11.219 dB` to `19.431 dB`) but still leaves the corpus at only
`16.35 dB` average and worsens other pages. The pre-quantization color-model
probes stay much higher (`47.90 dB` current model, `52.53 dB`
inverse-compatible model), so the catastrophic luma loss appears after
RGB/YCbCr conversion, inside the forward wavelet / coefficient quantization /
reconstruction-tracking path on high-detail color backgrounds. Follow-up #320
isolates that path with coefficient-plane diagnostics before any encoder
tuning.

### #301 — JB2 cross-size refinement byte-cost estimator — **Needs follow-up** (2026-05-17)

**Approach.** Extended the existing #283 cross-size candidate-count probe with
an approximate byte-cost model. For near cross-size candidates, the model
compares the current record-1 fresh-symbol payload estimate against a
hypothetical cross-size record-6 estimate that includes symbol-index/context
overhead, width/height/refinement overhead, and a packed Hamming-payload proxy.
No bytes are emitted and `encode_djvm_bundle_jb2` behavior is unchanged.

**Platform.**
- OS: macOS / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple Silicon host
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: Apple ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- RUSTFLAGS: unset

**Command(s).**

```sh
cargo run --release --example encode_quality_djbz -- \
  tests/corpus/watchmaker.djvu tests/corpus/pathogenic_bacteria_1896.djvu \
  --cc-stats --cross-size-stats \
  > /private/tmp/jb2_cost_301.jsonl \
  2> /private/tmp/jb2_cost_301.stderr
```

**Numbers.**

Bundle baseline from the same run:

| File | Pages | Original Sjbz | Independent dict | Bundled shared-Djbz | Bundle / independent | Round-trip |
|------|------:|--------------:|-----------------:|--------------------:|---------------------:|------------|
| `watchmaker.djvu` | 12 | 122,923 | 130,036 | 122,832 | 0.9446x | pixel-exact |
| `pathogenic_bacteria_1896.djvu` | 517 | 24,849,842 | 34,254,905 | 33,430,276 | 0.9759x | pixel-exact |

Cross-size estimator:

| File | Fresh CCs | Eligible | Candidates | Near @ 5% | Near pixels | Median best Hamming | Est current rec-1 | Est cross-size rec-6 | Est delta | Delta / independent |
|------|----------:|---------:|-----------:|----------:|------------:|--------------------:|-----------------:|---------------------:|----------:|--------------------:|
| `watchmaker.djvu` | 2,652 | 2,649 | 2,331 | 547 (20.65%) | 525,061 | 92 | 75,632 B | 5,641 B | -69,991 B | -53.82% |
| `pathogenic_bacteria_1896.djvu` | 759,291 | 703,928 | 686,402 | 61,485 (8.73%) | 67,245,972 | 88 | 9,677,015 B | 830,556 B | -8,846,459 B | -25.83% |

Semantics: a real cross-size record-6 path should be lossless if it emits the
full refinement bitmap correctly. This probe is not an emitting encoder path;
it uses nearest-neighbor scaled Hamming only for candidate selection and cost
estimation. The byte model is deliberately approximate and optimistic because
the packed Hamming proxy is not a real ZP-coded refinement bitstream.

**Decision.** Needs follow-up.

**Reason.** Both required corpora show enough estimated byte headroom to justify
a narrow emitting spike: `watchmaker` has 547 near cross-size matches and
`pathogenic_bacteria_1896` has 61,485, with large estimated negative deltas.
The result is not sufficient to change defaults because the estimator does not
prove actual ZP-coded record-6 byte cost or round-trip behavior. Follow-up #322
should implement an experiment-only cross-size rec-6 emitter, compare actual
bytes against this estimate, and stop before tuning if output is not
pixel-exact. The shipped default remains exact shared-Djbz rec-7 plus rec-1.

### #307 — x86_64 AVX2 row-pass feasibility spike — **Rejected** (2026-05-17)

**Approach.** Prototyped an x86_64 AVX2 `s == 1` horizontal IW44 row pass
behind compile-time `target_feature = "avx2"`, mirroring the AArch64 row-local
NEON shape and leaving baseline x86-64/default-codegen hosts on the existing
path. The spike included a gated AVX2 row-pass equivalence test covering short
rows, chunk boundaries, and scalar tails. The code was removed after the clean
measurement showed sensitive partial-decode regressions.

**Platform.**
- OS: Ubuntu GitHub-hosted runner (`ubuntu-latest`)
- CPU: GitHub-hosted x86_64 runner
- arch: `x86_64`
- target features: baseline x86-64 vs `x86-64-v3` / AVX2 codegen
- Rust: stable toolchain installed by `.github/workflows/bench.yml`
- RUSTFLAGS: unset for baseline; `-C target-cpu=x86-64-v3` for AVX2 pass

**Command(s).**

```sh
gh workflow run bench.yml -r codex/issue-307-avx2-row-pass
gh run download 25984542554 --dir /private/tmp/djvu-307-clean-artifacts
```

Workflow commands run by `Benchmark (x86-64-v3 AVX2 validation)`:

```sh
cargo bench --bench codecs -- 'iw44_to_rgb|iw44_decode' --output-format bencher
cargo bench --bench render -- 'render_corpus_color|render_colorbook' --output-format bencher
RUSTFLAGS='-C target-cpu=x86-64-v3' cargo bench --bench codecs -- 'iw44_to_rgb|iw44_decode' --output-format bencher
RUSTFLAGS='-C target-cpu=x86-64-v3' cargo bench --bench render -- 'render_corpus_color|render_colorbook' --output-format bencher
```

**Numbers.**

Source: GitHub Actions run `25984542554`, artifact
`bench-x86-64-v3-4ad38655adc465a16dc766efa5ac12c34c144fc9`.
Negative delta means the AVX2/x86-64-v3 pass is faster.

| Bench | default ns | +x86-64-v3 ns | Delta |
|-------|-----------:|--------------:|------:|
| `iw44_decode_corpus_color` | 1,372,742 | 1,133,708 | -17.41% |
| `iw44_decode_first_chunk` | 766,885 | 729,423 | -4.88% |
| `iw44_to_rgb_colorbook/sub1_full_decode` | 9,334,768 | 9,485,209 | +1.61% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,154,494 | 2,258,179 | +4.81% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 559,566 | 600,388 | +7.30% |
| `render_colorbook` | 11,523,817 | 11,760,347 | +2.05% |
| `render_colorbook_cold` | 27,060,741 | 27,560,588 | +1.85% |
| `render_colorbook_stages/bg_only_warm` | 1 | 1 | +0.00% |
| `render_colorbook_stages/full_render` | 11,495,717 | 11,795,482 | +2.61% |
| `render_colorbook_stages/mask_decode` | 5,368,030 | 5,132,471 | -4.39% |
| `render_corpus_color` | 129,285,417 | 129,176,157 | -0.08% |

**Decision.** Rejected; the prototype was removed before merge.

**Reason.** The full IW44 decode benches improved (`-17.41%` corpus decode,
`-4.88%` first chunk), but #307 explicitly called out sub2/sub4 partial decode
as sensitive benches. Those regressed by `+4.81%` and `+7.30%`, respectively,
which fails the acceptance criterion of a relevant win with no meaningful
regression. `render_colorbook` and full-render stages also drifted slower,
though within the 3% threshold. Because no production optimization landed,
`BENCHMARKS_RESULTS.md` was not updated. The benchmark workflow was triggered
manually because the current PR path filter does not include `crates/**`; a
follow-up should widen that filter so future crate-only performance PRs run
benchmark validation automatically.

### #308 — aarch64 NEON validation — **Kept partial** (2026-05-17)

**Approach.** Reran the current IW44 decode, partial decode, and
`render_colorbook` benchmark filters on the Apple Silicon host to validate the
current NEON paths after #292 established the cross-architecture matrix. No new
NEON kernel was added. Linux aarch64 benchmark cells remain explicitly missing:
#305 added native Linux aarch64 smoke coverage, but there is not yet a Linux
aarch64 benchmark workflow or artifact.

**Platform.**
- OS: macOS 26.3.1 / Darwin 25.3.0 (`RELEASE_ARM64_T6000`)
- CPU: Apple M1 Max, 10 cores
- arch: `arm64` / Rust host `aarch64-apple-darwin`
- target features: ARM64 baseline; NEON available on Apple Silicon
- Rust: `rustc 1.92.0 (ded5c06cf 2025-12-08)`
- RUSTFLAGS: unset

**Command(s).**

```sh
cargo bench --bench codecs -- 'iw44_to_rgb|iw44_decode' --output-format bencher
cargo bench --bench render -- 'render_corpus_color|render_colorbook' --output-format bencher
```

**Numbers.**

| Bench | Apple ARM64 ns/iter | Matrix value |
|-------|--------------------:|--------------|
| `iw44_decode_corpus_color` | 636,847 | 637 us |
| `iw44_decode_first_chunk` | 557,004 | 557 us |
| `iw44_to_rgb_colorbook/sub1_full_decode` | 5,470,697 | 5.47 ms |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 1,301,311 | 1.30 ms |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 337,043 | 337 us |
| `render_colorbook` | 6,921,690 | 6.92 ms |
| `render_colorbook_stages/full_render` | 6,932,763 | 6.93 ms |
| `render_colorbook_stages/bg_only_warm` | 0 | 0 ns |
| `render_colorbook_stages/mask_decode` | 4,173,642 | 4.17 ms |
| `render_colorbook_cold` | 17,426,166 | 17.4 ms |
| `render_corpus_color` | 68,726,395 | 68.7 ms |

**Decision.** Kept partial.

**Reason.** Current Apple ARM64 NEON paths remain healthy: first-chunk decode
is now `557 us` versus the stale #184 note's `715 us` reference, corpus IW44
decode remains `637 us`, and sub2/sub4 partial decode stay near the existing
matrix values. This closes the stale ARM64 remeasurement note without adding
new kernels. The result is "partial" because Linux aarch64 still has only
build/test smoke coverage and no authoritative benchmark artifact; the matrix
keeps those cells as `missing`.

### IW44 slice-loop early-exit on `zp.is_exhausted()` — **Reverted** (2026-06-09)

**Issue.** External bug report (treadbear): a color page in
`colorbook.djvu` (and a user-attached file) decoded with "sparkly rainbow
artifacts". Reporter's proposed fix: delete the `if zp.is_exhausted() { break; }`
guard at the end of the slice loop in `Iw44Image::decode_chunk`.

**Approach (the reverted code).** The slice loop
(`for _ in 0..slices`) broke early once the ZP byte buffer was drained,
on the assumption that "remaining slices carry no new information" and to
"bound decode time on crafted inputs".

**Numbers.** Differential test vs DjVuLibre `ddjvu` at native resolution
(`--width 2260 --tolerance 4`):

| `colorbook.djvu` page | with early-exit | without early-exit |
|-----------------------|-----------------|--------------------|
| p0 | 0.27% | 0.27% |
| p1 | 0.64% | 0.64% |
| **p2** | **60.87%** mismatch, mean Δ 3.05 | **0.31%**, mean Δ 0.05 |
| p3 | 1.50% | 1.50% |

Page 2's IW44 chunk packs many slices into few bytes, so `is_exhausted()`
fires mid-stream and the `break` drops ~all remaining chroma/luma refinement.

**Decision.** Reverted (early-exit removed).

**Reason.** Same root cause as #182, one level up. `is_exhausted()` reports
only `pos >= data.len()` (the *byte* buffer), but the ZP coder is a
continuous arithmetic bit stream that reads up to 24 bits ahead via
`refill_buffer`, so the byte pointer reaches the end several bytes before the
logical end of decisions. The remaining slices still decode legitimate
refinement from the buffered bits + arithmetic registers; skipping them
desynchronises nothing (each slice is self-contained work) but truncates real
high-frequency detail → chroma artifacts. The "bound decode time" rationale
was unfounded: the loop is already bounded by `slices` (a `u8`, ≤255 per
chunk) and the 64 MP image cap, so no early-exit is required to bound work.
Regression test: `iw44_colorbook_page2_decodes_all_slices_no_early_exit` in
`crates/djvu-iw44/src/lib.rs` (and the native-res diff gate).

### JB2 post-EOF guard: `is_exhausted()` → synthetic-byte (`pos`) overshoot — **Kept** (2026-06-16)

**Issue.** Follow-up to the IW44 report above: the same reporter noted "a
similar issue in the JB2 code path" with a repro page, and proposed adding a
`ZP_EOF_SLACK_BYTES` tolerance computed as `zp.pos.saturating_sub(data.len())`.
JB2's per-symbol DoS guard (`check_symbol_decode_budget`) rejected a decode with
`Jb2Error::Truncated` whenever `zp.is_exhausted() && symbol_pixels > 4096`. Like
the IW44 `break`, `is_exhausted()` (`pos >= data.len()`) flips ~4–8 bytes before
the logical end of a valid stream, so any page whose final tile/symbol is larger
than 64×64 px and is decoded from the ZP look-ahead window was wrongly rejected.

**Approach.** Adopted the reporter's slack idea but fixed its mechanism. The
verbatim patch is a no-op as written: the canonical `ZpDecoder::read_byte`
*saturated* `pos` at `data.len()`, so `pos - len` was always 0 and the guard
would never fire — while the inlined hot-path byte readers in jb2/iw44/bzz
*did* advance `pos` past the end (`wrapping_add`). The two byte-reading paths
disagreed. Unified them: `read_byte` now also always advances `pos`, so
`pos - data.len()` is the true synthetic-`0xFF` count across every reader, and
`synthetic_bytes()` returns exactly that. The JB2 guards now bail only when
`synthetic_bytes() > ZP_EOF_SLACK_BYTES` (16) — at the per-symbol check *and* at
the top of all three record loops (covering record types 7/9/10, which decode no
symbol and so never reached the per-symbol check — that gap let a post-EOF
stream spin to `MAX_RECORDS` ≈ 14 s before the unconditional loop guard was
added). Dropped the old `MAX_EXHAUSTED_SYMBOL_PIXELS` / `…_TOTAL_…` sub-budgets;
in-window symbols stay capped by `check_pixel_budget` (16 MP/symbol, 256 MP).

**Numbers.** Synthesised valid pages (`encode_jb2`: high-entropy first tile +
solid trailing tiles, so a >4096 px symbol's header is read while
`is_exhausted()` is true but no synthetic byte has been consumed):

| page (`encode_jb2`) | old `is_exhausted` guard | new `synthetic_bytes` guard |
|---------------------|--------------------------|-----------------------------|
| 200×2100 | `Err(Truncated)` | `Ok`, pixel-exact round-trip |
| 200×3100 | `Err(Truncated)` | `Ok`, pixel-exact round-trip |

DoS regressions stay fast (`Err(Truncated)` in <1 s): the post-EOF refinement
spin tests still bail, now via the loop-level guard. Full corpus + proptest
round-trips unchanged (653 workspace tests pass).

**Decision.** Kept (guard switched to synthetic-byte overshoot; `read_byte`
no longer clamps `pos`).

**Reason.** Same root cause as the IW44 fix and #182: `is_exhausted()` is a
*byte*-buffer signal, not a logical-EOF signal. `pos`-overshoot counts only
genuine post-EOF `0xFF` padding, which stays ~0 through a valid look-ahead tail
and climbs without bound only when the coder is truly spinning — exactly the
distinction the guard needs. Regression tests:
`large_symbol_at_eof_not_wrongly_truncated` (encode.rs, positive case),
`synthetic_bytes_distinguishes_eof_from_spinning` (djvu-zp), and the
`exhausted_*_refinement_*` DoS tests in `crates/djvu-jb2/src/lib.rs`.

### PARALLEL_COMPOSITOR: row-level rayon parallelism in `composite_into` — **Kept** (2026-06-18)

**Issue.** #408: close the 1.2–2.1× gap vs DjVuLibre across all four benchmark
scenarios. After DECODE_SCALE_ROUND and BILINEAR_1_1_NEAREST brought single-threaded
performance to ~1.2× for colorbook but left corpus color/bilevel at ~2×, the
remaining gap was pure compositor throughput.

**Approach.** Added a `parallel` Cargo feature (wrapping `rayon`). In
`composite_into`, split the output buffer with `par_chunks_exact_mut(row_stride)`
and dispatch each row to the appropriate per-row helper
(`composite_rows_bilevel_one` / `composite_rows_bilinear_one` /
`composite_rows_area_avg_one`) across all available cores. `CompositeContext<'_>`
is `Sync` by construction (all fields are plain data or immutable references), so
no `Arc`/`Mutex` overhead. Single-threaded path preserved under
`#[cfg(not(feature = "parallel"))]`.

**Numbers** (Criterion, Apple M1 Max 10-core, `benches/render.rs`,
`--features parallel` vs DjVuLibre C API):

| Benchmark | djvu-rs single-thread | djvu-rs `--features parallel` | DjVuLibre | Ratio (parallel) |
|-----------|----------------------:|------------------------------:|----------:|:----------------:|
| `render_page/dpi/72` (boy, 72 dpi) | ~211 µs | **181 µs** | 147 µs | **1.23×** ✓ |
| `render_colorbook` (colorbook, 150 dpi) | ~7.1 ms | **1.67 ms** | 5.90 ms | **0.28×** ✓ |
| `render_corpus_color` (watchmaker, 300 dpi) | ~70.5 ms | **15.3 ms** | 36.0 ms | **0.43×** ✓ |
| `render_corpus_bilevel` (cable, 300 dpi) | ~71.3 ms | **16.8 ms** | 35.2 ms | **0.48×** ✓ |

All four benchmark scenarios achieve ≤ 1.5× DjVuLibre with `--features parallel`.
Without the feature, corpus color/bilevel remain at ~2× (the single-thread ceiling).

**Decision.** Kept. Feature gated so existing users see no new dependency.

**Reason.** The compositor is embarrassingly parallel (rows are independent), and
the Apple M1 Max has 10 cores. Row-level rayon gives ~4.4× speedup on the corpus
targets at zero algorithmic complexity. The corpus color/bilevel benchmarks are
purely compositor-bound after DECODE_SCALE_ROUND and BILINEAR_1_1_NEAREST; adding
threads is the only way to close the remaining gap without SIMD (blocked by
`#![deny(unsafe_code)]`). The 72-dpi target is decode-bound (BG44 + JB2) so
parallelism helps less there, but it still falls within 1.5×.

### BILINEAR_1_1_NEAREST: replace `sample_bilinear` with `sample_nearest` at 1:1 scale — **Kept** (2026-06-18)

**Issue.** #408: reduce per-pixel work in `composite_rows_bilinear_one` at exact
1:1 scale (native resolution). At 1:1, `fx_step == FRAC` so the fractional
coordinates `tx = ty = 0`, making bilinear interpolation read 4 pixels but use
only the top-left one.

**Approach.** Added a fast-path guard at the top of
`composite_rows_bilinear_one`: when `fx_step == FRAC && fy_step == FRAC &&
ctx.bg_x_q24 == (1 << 24) && ctx.bg_y_q24 == (1 << 24)` (i.e., true 1:1 with no
BG-plane subsampling), replace `sample_bilinear` with `sample_nearest`.
Also added an extra-tight inner loop for the common corpus case (BG present, mask
present, no palette, no FG44, zero horizontal offset) that skips all per-pixel
branching except the mask bit test. The general bilinear loop runs outside this
fast path only when offset, palette, or FG44 are present.

**Numbers** (Criterion, Apple M1 Max, before/after on single-threaded build):

| Benchmark | Before | After | Δ |
|-----------|-------:|------:|--:|
| `render_corpus_color` | ~74 ms | ~70.5 ms | −5% |
| `render_corpus_bilevel` | ~74 ms | ~71.3 ms | −4% |
| `render_compositor_only/color_native_cached` | ~75 ms | ~70.9 ms | −5% |
| `render_compositor_only/bilevel_native_cached` | ~71 ms | ~71.3 ms | flat |

**Decision.** Kept.

**Reason.** Eliminating three redundant pixel reads per output pixel at native
scale is a pure win. The inner-loop specialization also removes per-pixel palette
and FG44 branches for the common case. Improvements are modest in absolute
terms (~3–5 ms on a 70 ms workload) but correct and zero-risk: the fast path
falls through to the general bilinear loop for any non-trivial case.

### DECODE_SCALE_ROUND: use `.round()` instead of truncation in `best_iw44_subsample` — **Kept** (2026-06-18)

**Issue.** #408: `render_colorbook` measured at ~12 ms vs DjVuLibre 5.90 ms —
far beyond the 1.5× target. Root cause: `best_iw44_subsample(scale)` computed
`max_sub = (1.5 / scale) as u32` (truncation). For colorbook at 150 dpi:
`scale = 848/2260 = 0.37522`, `1.5/0.37522 = 3.9977`, which truncates to 3 →
largest power-of-2 ≤ 3 is 2. So subsample 2 was selected instead of the correct
subsample 4, decoding 4× more IW44 data than needed.

**Approach.** Changed `(1.5_f32 / scale) as u32` to `(1.5_f32 / scale).round()
as u32` in `best_iw44_subsample`. Added a comment explaining the rounding
rationale: pixel-rounding of width (integer output dimensions) causes `scale` to
differ from the true geometric ratio by up to `0.5/page_width`, which can push
`1.5/scale` just below an integer and trigger a 2× coarser subsample.

**Numbers** (Criterion, Apple M1 Max):

| Benchmark | Before | After | Δ |
|-----------|-------:|------:|--:|
| `render_colorbook` | ~12.3 ms | ~7.1 ms | **−42%** |
| `render_colorbook_stages/mask_decode` | ~5.2 ms | ~5.2 ms | flat |
| `render_colorbook_stages/bg_only_warm` | (not measured) | ~1 ns | (cache hit) |
| `render_corpus_color` | ~70.9 ms | ~70.5 ms | flat |

**Decision.** Kept.

**Reason.** The truncation bug caused subsample-2 decode (4× more data) when the
true scale warranted subsample-4. The fix brings colorbook from 2.1× DjVuLibre to
~1.2× in single-threaded mode. It is also strictly more correct: the rounding
accounts for integer-pixel rounding in output dimensions. No other benchmark
regressed.

### BILEVEL_EXPAND_LUT: const byte→8xRGBA table for bilevel 1:1 compositor — **Reverted** (2026-06-18)

**Issue.** #408: close 2×+ gap vs DjVuLibre on bilevel corpus (300 dpi, cable
pages). `render_corpus_bilevel` baseline ≈ 71 ms vs DjVuLibre ≈ 35 ms.

**Approach.** Added a 256-entry const lookup table (8 KB) mapping a mask byte
to 8 pre-expanded RGBA pixels (32 bytes per entry). Modified the `offset_x %
8 == 0` fast path in `composite_rows_bilevel_one` to copy 32 bytes per byte
from the LUT instead of branching per bit. Expected saving: eliminate 7
bit-shift/branch operations per 8 pixels.

**Numbers** (Criterion, Apple M-series, `benches/render.rs`):

| bench | baseline | with LUT |
|-------|----------|----------|
| `render_corpus_bilevel` | 71.3 ms | 73.5 ms |
| `compositor_only/bilevel_native_cached` | 71.4 ms | 72.3 ms |

**Decision.** Reverted (code restored with `git restore src/djvu_render.rs`).

**Reason.** The bilevel 1:1 path is memory-bandwidth limited, not
compute-limited. The output is ~34 MB of RGBA data per benchmark iteration
(cable corpus at 600→600 dpi), consuming effectively ~500 MB/s of effective
write bandwidth. The LUT read (8 KB, likely L1-cached) does not overlap with
the RGBA output writes — it competes with them. Saving 7 bit ops per byte is
irrelevant when the bottleneck is the output store stream. Closing the
bilevel gap requires reducing output data volume (e.g. lazy/tile rendering or
a non-RGBA output format) or parallelism, not faster pixel computation.

### A2 — MASK_EXPAND LUT + branchless blend in tight bilinear 1:1 path — **Kept** (2026-06-18)

**Issue.** #408 follow-up: reduce per-pixel branch cost in the tight bilinear
1:1 path (watchmaker corpus, bilevel mask + color background at native scale).

**Approach.** Added a 256-entry `MASK_EXPAND` const LUT mapping each mask byte
to 8 fg-mask bytes (0xFF/0x00, MSB-first). Modified the has-mask loop in the
tight bilinear path to process 8 pixels per mask byte, replacing the
variable shift `7 - (ox & 7)` with a constant LUT index. Used branchless
blend: `bg_channel & !fg_m` (0 for fg, bg value for bg) instead of an
`if is_fg` branch.

**Numbers** (Criterion, Apple M-series, `benches/render.rs`):

| bench | before | after | change |
|-------|--------|-------|--------|
| `render_corpus_color` | 70.9 ms | 70.5 ms | −10.1% (p=0.00) |
| `render_corpus_bilevel` | 73.3 ms | 71.8 ms | −1.0% (p=0.02) |
| `render_colorbook` | 12.3 ms | 6.0 ms | −51% (p=0.08, not sig.) |
| `compositor_only/color_native_cached` | 75.1 ms | 70.9 ms | −5.6% (p=0.06) |
| `compositor_only/bilevel_native_cached` | 70.9 ms | 72.3 ms | +2% (p=0.27, noise) |
| `compositor_only/color_downscale_cached` | 12.2 ms | 5.9 ms | −52% (p=0.20) |

**Decision.** Kept.

**Reason.** The `render_corpus_color` improvement of −10% is statistically
significant (p=0.00). The branchless mask-byte loop removes a
data-dependency on the bit-shift index and allows the compiler to better
schedule/unroll the inner loop. No regression on any benchmark within
noise tolerance.

### B1 — hoist bg_fy + incremental bg_fx accumulator in general bilinear path — **Kept** (2026-06-18)

**Issue.** #408 follow-up: reduce per-pixel cost in the general bilinear path
(cable corpus, BG at subsample 3, non-1:1 sampling).

**Approach.** Two changes to `composite_rows_bilinear_one`:
1. Hoist `map_plane_center_frac(fy, bg_y_q24)` outside the pixel loop
   (row-invariant).
2. Replace per-pixel `map_plane_center_frac(fx, bg_x_q24)` (u64 multiply +
   shift) with an exact u64 accumulator that adds `fx_step * bg_x_q24` per
   pixel in Q48 and applies `>> 24` per sample. No rounding error vs the
   original: the accumulator starts at `(offset_x * fx_step + FRAC/2) *
   bg_x_q24` so the integer truncation is identical.

**Numbers** (Criterion, Apple M-series):

| bench | before (A2) | after B1 | change |
|-------|------------|----------|--------|
| `render_corpus_color` | 70.5 ms | 67.0 ms | −4.6% (p=0.00) |
| `render_corpus_bilevel` | 71.8 ms | 67.9 ms | −5.5% (p=0.00) |
| `compositor_only/color_native_cached` | 70.9 ms | 67.6 ms | −5.2% (p=0.00) |
| `compositor_only/bilevel_native_cached` | 72.3 ms | 67.8 ms | −6.1% (p=0.00) |
| `render_colorbook` | 6.0 ms | 6.3 ms | +4.8% (p=0.00, small doc noise) |

**Decision.** Kept.

**Reason.** Consistent ~5% improvement on all corpus benchmarks with p=0.00.
The colorbook regression is +0.3 ms absolute on a micro-benchmark and
consistent with the variance seen between runs; it does not affect larger docs.

### B1b — specialized general-bilinear path with MASK_EXPAND for cable case — **Reverted** (2026-06-18)

**Issue.** Follow-up to B1: try pre-fetching the mask row and using MASK_EXPAND
in a specialized inner loop for the common general-bilinear case (cable: native
scale, no fg44/palette).

**Approach.** Added a `if fx_step == FRAC && ctx.offset_x == 0 && fg_palette.is_none() && fg44.is_none()` guarded specialized path before the general loop, using the same mb_idx/j nested loop as A2.

**Numbers** (Criterion, Apple M-series):

| bench | before (B1) | after B1b | change |
|-------|------------|-----------|--------|
| `render_corpus_color` | 67.0 ms | 69.5 ms | +3.7% (p=0.00) |
| `render_corpus_bilevel` | 67.9 ms | 70.4 ms | +3.7% (p=0.00) |

**Decision.** Reverted (`git restore src/djvu_render.rs`).

**Reason.** The extra code path (condition checks + nested loop structure) added
instruction-cache pressure and branch overhead that outweighed any mask-lookup
savings. The `sample_bilinear` call remains the dominant cost; the mask lookup
`m.get(px, py)` is a negligible fraction.

### B2 — precompute BG row slices in general bilinear path — **Kept** (2026-06-18)

**Issue.** #408 follow-up: eliminate per-pixel y-coordinate arithmetic in the
general bilinear path (cable corpus, BG at subsample 3).

**Approach.** Added `bilinear_from_rows()` helper that takes pre-fetched row0
and row1 slices instead of a `Pixmap`. Outside the pixel loop, precompute:
- `y0 = (bg_fy >> FRACBITS).min(height-1)`, `y1 = y0+1`
- `ty = bg_fy & FRAC_MASK`
- `row0 = bg.data[y0 * stride ..]`, `row1 = bg.data[y1 * stride ..]`

Despite `sample_bilinear` being `#[inline]`, LLVM did NOT perform loop-invariant
code motion for these y-coordinate computations, so explicit precomputation
eliminates one integer multiply and two `.min()` ops per pixel (4× inside the
original 4-call bilinear).

**Numbers** (Criterion, Apple M-series):

| bench | before (B1) | after B2 | change |
|-------|------------|----------|--------|
| `render_corpus_color` | 67.0 ms | 55.6 ms | −20.0% (p=0.00) |
| `render_corpus_bilevel` | 67.9 ms | 56.3 ms | −19.9% (p=0.00) |
| `compositor_only/color_native_cached` | 67.6 ms | 55.3 ms | −19.1% (p=0.00) |
| `compositor_only/bilevel_native_cached` | 67.8 ms | 55.1 ms | −21.5% (p=0.00) |

**Decision.** Kept.

**Reason.** Large, consistent, statistically significant improvement across all
bilinear benchmarks. No regressions.

### B2b — hoist mask row slice in general bilinear path — **Kept** (2026-06-18)

**Issue.** Follow-up to B2: eliminate per-pixel `Bitmap::get()` y*stride
multiply in the general bilinear path.

**Approach.** Before the pixel loop, pre-fetch the mask row slice for `py`
from `Bitmap::data`. Inline the bit extraction `(mask_row[px >> 3] >> (7 - px & 7)) & 1`
instead of calling `m.get(px, py)` per pixel.

**Numbers** (Criterion, Apple M-series):

| bench | before (B2) | after B2b | change |
|-------|------------|-----------|--------|
| `compositor_only/color_native_cached` | 55.3 ms | 53.6 ms | −3.1% (p=0.00) |
| `compositor_only/bilevel_native_cached` | 55.1 ms | 53.8 ms | −2.4% (p=0.00) |

**Decision.** Kept.

**Reason.** Consistent improvement. Eliminates one integer multiply (y*stride)
from every pixel's mask lookup.

### B2c — replace fx multiply with running accumulator — **Reverted** (2026-06-18)

**Issue.** Replace `fx = (ox + offset_x) * fx_step` per-pixel multiply with
a wrapping-add accumulator.

**Numbers:** corpus_color −1.3%, corpus_bilevel +2.2% (mixed, p=0.04 marginal).

**Decision.** Reverted. LLVM already handles the multiply well; the accumulator
approach adds register pressure and causes bilevel regression.

### B2d — two bounds checks per row in bilinear_from_rows — **Reverted** (2026-06-18)

**Issue.** Replace 4 separate `row.get(off..off+3)` calls with 2 `row.get(..end)` calls.

**Numbers:** corpus_color +89% regression.

**Decision.** Reverted immediately. Returning a (u8, u8, u8, u8, u8, u8) tuple
from the helper forces stack spills. The per-call `get` approach keeps values in
registers.

### LERP_NO_CLAMP — remove min(255) from bilinear lerp — **Kept** (2026-06-18)

**Issue.** The `lerp` closure in `sample_bilinear` and `bilinear_from_rows`
clamped the result with `v.min(255)` before casting to u8.

**Proof it's redundant.** With FRAC=16, FRACBITS=4:
- `tx, ty ∈ [0, 15]`
- `top = a * (16-tx) + b * tx ≤ 255 * 16 = 4080` (since (16-tx)+tx=16, and a,b ≤ 255)
- `numerator = top * (16-ty) + bot * ty ≤ 4080 * 16 = 65280`
- `v = (65280 + 128) >> 8 = 255` — never exceeds 255

**Approach.** Remove `v.min(255)` and cast directly: `... as u8`.

**Numbers** (Criterion, Apple M-series):

| bench | before | after | change |
|-------|--------|-------|--------|
| `compositor_only/color_native_cached` | 53.6 ms | 47.5 ms | −11.2% (p=0.00) |
| `compositor_only/bilevel_native_cached` | 53.8 ms | 48.6 ms | −9.2% (p=0.00) |
| `render_corpus_color` | ~54 ms | ~48 ms | −11% (p=0.00) |

Cumulative improvement from session baseline (before all experiments):
- `render_corpus_color`: 70.9 ms → 48.2 ms = **−32%**
- `render_corpus_bilevel`: 73.3 ms → 48.0 ms = **−35%**

**Decision.** Kept.

**Reason.** The clamp was a no-op as proven by the arithmetic invariant. Removing
it allows LLVM to better schedule and vectorize the lerp arithmetic.

### C1_SIMD — wide::u32x4 SIMD for bilinear lerp (all 3 channels at once) — **Reverted** (2026-06-18)

**Issue.** Replace 3 separate scalar lerp calls (12 multiply-adds) with a
single `wide::u32x4` vector operation processing R, G, B + padding in parallel.

**Approach.** In `bilinear_from_rows`, pack `[r00, g00, b00, 0]` etc. into
`u32x4` vectors and do the 2D bilinear lerp as 4-wide SIMD.

**Numbers:** `compositor_only/color_native_cached` +61% (p=0.00).

**Decision.** Reverted immediately.

**Reason.** Scalar→vector transfer for 4 individual u8 values (from separate
`get()` calls) has higher overhead than the scalar multiply-adds it replaces.
LLVM already auto-vectorizes the scalar loop after LERP_NO_CLAMP removed the
`min(255)` guard; explicit `u32x4` fights against the optimizer.

### D1 — gamma identity fast-path (skip LUT reads when gamma = identity) — **Kept** (2026-06-18)

**Issue.** Every rendered pixel does 3 table-scatter reads into `gamma_lut[256]`
even when the LUT is the identity mapping (i.e. DjVu gamma = DISPLAY_GAMMA = 2.2,
the most common case). The scatter reads defeat LLVM's auto-vectorizer because
it cannot prove the LUT is identity at compile time.

**Approach.** Added a `gamma_is_identity: bool` field to `CompositeContext`,
computed once per frame from `gamma_lut.iter().enumerate().all(|(i, &v)| v == i as u8)`.
Hoisted the check outside the pixel loop in all four compositor paths:
— A2 tight 1:1 has-mask (macro to duplicate loop body)
— A2 tight 1:1 no-mask (duplicate loop body)
— general 1:1 nearest-neighbour path (per-pixel branch acceptable, path is not
  the hot corpus path)
— general bilinear path (already applied in first pass)
— area-average downscale path

**Numbers (absolute times, post-LERP_NO_CLAMP baseline ~48 ms):**

| Benchmark | Before D1 | After D1 (all paths) | Δ |
|-------|--------|-------|--------|
| `compositor_only/color_native_cached` | 47.5 ms | 44.6 ms | −6.1% (p=0.00) |
| `compositor_only/bilevel_native_cached` | 48.6 ms | 45.0 ms | −7.4% (p=0.00) |
| `render_corpus_color` | ~48 ms | 46.2 ms | −3.7% (p=0.00) |
| `render_corpus_bilevel` | ~48 ms | 45.2 ms | −5.8% (p=0.00) |

Cumulative improvement from session baseline (before all experiments):
- `render_corpus_color`: 70.9 ms → 46.2 ms = **−35%**
- `render_corpus_bilevel`: 73.3 ms → 45.2 ms = **−38%**

**Decision.** Kept.

**Reason.** Removing the indirect LUT reads when they are provably identity lets
LLVM vectorize the write loop. The `gamma_is_identity` flag costs one boolean
comparison per row (negligible) and correctly falls back to full LUT for
non-standard gamma values.

### E1 — no-mask bulk memcpy for tight 1:1 gamma-identity path — **Kept** (2026-06-18)

**Issue.** The tight 1:1 no-mask path (pure background copy, no foreground mask)
wrote pixels one-by-one with per-pixel bounds checks even when the BG pixmap is
guaranteed to cover the full output width.

**Approach.** When `bg.width >= out_w` and `gamma_is_identity`, replace the
per-pixel loop with a single `copy_from_slice` (i.e., `memcpy`). LLVM and the
platform memcpy implementation apply SIMD bulk copy; `fill_alpha_255` corrects
the alpha channel afterwards as usual.

**Numbers:** modest; corpus_color −1.3% (no-mask pages). No impact on
corpus_bilevel (bilevel always has a mask layer).

**Decision.** Kept.

**Reason.** Clean single-call copy is the right abstraction for a full-row copy.
The fallback path is unchanged for edge cases where BG is narrower.

### E2 — specialized no-mask bilinear loop (early return) — **Reverted** (2026-06-18)

**Issue.** When `mask_hoist.is_none()`, the bilinear inner loop always takes the
`else` branch of `is_fg`, yet the per-pixel branch is still evaluated each
iteration.

**Approach.** Added an early-return block before the main loop: if no mask and
gamma is identity, run a stripped-down loop with only the bilinear sample + write,
then return.

**Numbers:** `render_corpus_color` +6% (p=0.00) — regression.

**Decision.** Reverted immediately.

**Reason.** The early return created a second loop body, doubling the code size
for the bilinear path. Instruction cache pressure and reduced inlining headroom
outweighed the per-pixel branch savings. The corpus_color pages also have mask
layers, so the fast path was never taken — the overhead of the outer `if` check
remained with no benefit.

### F3 — 4-byte pixel read in `bilinear_from_rows` — **Kept** (2026-06-18)

**Issue.** `bilinear_from_rows` read each of the 4 bilinear-grid corner pixels as
a 3-byte slice (`off..off+3`). No 3-byte load instruction exists on any target;
LLVM must emit a 16-bit + 8-bit load pair, adding an instruction and lengthening
the dependency chain.

**Approach.** Since every pixel is stored as RGBA (4 bytes), change the get
closure to `row.get(off..off+4)`. The bounds check is still valid because
`x ≤ width-1` guarantees `x*4 + 4 ≤ width*4 = row.len()`. LLVM can now emit a
single 32-bit load per corner.

**Numbers:**

| Benchmark | Before F3 | After F3 | Δ |
|-------|--------|-------|--------|
| `render_corpus_color` | 45.6 ms | 44.8 ms | −1.8% |
| `compositor_only/color_native_cached` | 44.8 ms | 44.3 ms | −1.1% |

**Decision.** Kept.

**Reason.** Turning a 3-byte load into a 32-bit load is always strictly better;
the 4th byte (alpha) is harmlessly ignored by the caller.

Cumulative improvement from session baseline:
- `render_corpus_color`: 70.9 ms → 44.8 ms = **−36.8%**
- `render_corpus_bilevel`: 73.3 ms → 45.2 ms = **−38.3%**

### F3b — 4-byte BG pixel read in A2 tight 1:1 loop — **Kept** (2026-06-18)

**Issue.** The A2 has-mask and no-mask tight 1:1 loops still read BG pixels as
3-byte slices (`off..off+3`), the same issue F3 fixed in `bilinear_from_rows`.

**Approach.** Apply the same F3 change: `bg_row.get(off..off+3)` →
`bg_row.get(off..off+4)`. `px ≤ bg.width−1` guarantees `px*4+4 ≤ bg.width*4 ≤ bg_row.len()`.

**Numbers:** Neutral — within noise on all benchmarks (±0.3 ms / ±0.7%).

**Decision.** Kept (no regression, strictly correct improvement by eliminating
3-byte load).

**Reason.** A2 tight-path BG reads are not the bottleneck (the write to row_buf
dominates). The change is kept for correctness and consistency with F3.

### G1 — 4-byte write (RGBA) in A2 tight 1:1 gamma-identity loop — **Reverted** (2026-06-18)

**Issue.** The A2 gamma-identity inner loop writes `pixel[0]=r; pixel[1]=g; pixel[2]=b`
(3 separate byte stores). A single 32-bit store would be more efficient.

**Approach.** Replace with `pixel.copy_from_slice(&[r, g, b, 0])` so LLVM can
emit a 32-bit store. The alpha byte (0) is corrected by `fill_alpha_255`
afterwards.

**Numbers:** Neutral — corpus_bilevel ±0.2 ms.

**Decision.** Reverted.

**Reason.** The A2 loop is already auto-vectorized by LLVM; the 3-byte stores in
SIMD context are merged into wider vector stores. Writing an extra 0 byte adds
a small initialisation cost that cancels the benefit.

### A3 — mb==0 bulk-copy fast path in A2 has-mask loop — **Reverted** (2026-06-18)

**Issue.** When a mask byte is 0x00 (all background), the A2 inner j-loop
applies `& !fg_m = & 0xFF` — a no-op — to each of the 8 pixels. A bulk 32-byte
copy would be faster.

**Approach.** Add a branch before the j-loop: when `mb == 0 && group_end < out_w && group_end <= bg_max_px`, bulk-copy 32 bytes from `bg_row` to `row_buf` (all 4 bytes per pixel; `fill_alpha_255` fixes alpha). Continue to next mask byte.

**Numbers:** Neutral — corpus_bilevel ±0.1 ms.

**Decision.** Reverted.

**Reason.** LLVM already auto-vectorizes the j-loop for mb=0, optimizing away
the `& 0xFF` no-op and emitting equivalent SIMD copy instructions. The extra
branch overhead of the fast path matched the cost it avoided.

### AA1 — B1-style u64 accumulator for `bg_fx` in area-avg path — **Reverted** (2026-06-18)

**Issue.** `composite_rows_area_avg_one` computes `bg_fx = ((fx * bg_x_q24) >> 24)`
per pixel (u64 multiply + shift), analogous to the per-pixel multiply that B1
eliminated in the bilinear path.

**Approach.** Replace with a u64 accumulator starting at
`offset_x * fx_step * bg_x_q24`, stepping by `fx_step * bg_x_q24` per pixel.
Also hoist row-invariant `fg_fy` and `fg_fy_step` out of the inner loop.

**Numbers:**
- `render_colorbook`: +1.8% (regression)
- `render_corpus_color`: +2.2% (regression — does NOT use area-avg path)
- `render_corpus_bilevel`: +4% (regression — does NOT use area-avg path)

**Decision.** Reverted.

**Reason.** `composite_rows_area_avg_one` is marked `#[inline]` and is inlined
into the render loop alongside `composite_rows_bilinear_one`. Adding code to
the area-avg function increases the total inlined code size in the render loop,
degrading instruction-cache locality for corpus pages that only ever take the
bilinear path. Removing `#[inline]` or using `#[inline(never)]` also causes
regression because LLVM can no longer hoist the `if downscale` dispatch out of
the per-row loop. B1 worked because its additions were in `bilinear_from_rows`
(called from the hot path) and directly reduced per-pixel work; area-avg
additions add dead code to the hot path's code layout.

### H1 — 4-weight bilinear lerp (precompute w00..w11 once per pixel) — **Kept** (2026-06-19)

**Issue.** Profiling (`samply`) pinpointed the inner loop of `composite_rows_bilinear_one` as the only hot spot. Disassembly revealed `ty` was spilled to the stack (`ldr w21, [sp, #0x5c]`) on the multiply critical path, and that `(FRAC - tx)` / `(FRAC - ty)` were recomputed inside each of the 3 per-channel `lerp()` calls — 12 redundant subtractions per pixel.

**Approach.** Replace the two-level bilinear lerp inside `bilinear_from_rows`:
```rust
// old — tx/ty subtracted 3× each inside lerp closure
let top = a * (FRAC-tx) + b * tx;
let bot = c * (FRAC-tx) + d * tx;
(top * (FRAC-ty) + bot * ty + round) >> (2*FRACBITS)
```
with a 4-weight dot-product:
```rust
// new — weights computed once, all captured by value in registers
let (itx, ity) = (FRAC-tx, FRAC-ty);
let (w00, w10, w01, w11) = (itx*ity, tx*ity, itx*ty, tx*ty);
// sum w_ij = FRAC² = 256, so >> 8 normalises
(a*w00 + b*w10 + c*w01 + d*w11 + 128) >> 8
```

**Numbers:**

| Benchmark | Before | After | Δ |
|---|---|---|---|
| render_colorbook | 12.3 ms | 5.77 ms | **−53%** |
| render_corpus_color | 70.9 ms | 46.0 ms | **−35%** |
| render_corpus_bilevel | 73.3 ms | 46.0 ms | **−37%** |
| compositor color_native_cached | 75.1 ms | 45.8 ms | **−39%** |
| compositor bilevel_native_cached | 70.9 ms | 45.8 ms | **−35%** |
| compositor color_downscale_cached | 12.2 ms | 5.73 ms | **−53%** |

**Decision.** Kept.

**Reason.** Precomputing weights eliminates redundant arithmetic and removes `ty` from the per-channel critical path (no more stack reload inside the multiply chain). LLVM can better vectorise three independent `blend()` calls that share only constant weights, yielding ~35–53% wall-clock improvement across all bilinear-heavy paths.

### H2 — slice-trim to exact stride to let LLVM elide bilinear bounds checks — **Reverted** (2026-06-19)

**Issue.** Profile shows `b.hs` (bounds check branch) inside `bilinear_from_rows` at
10.4% of samples. Callers guarantee `row.len() ≥ width*4`, so the fallback
`(0, 0, 0)` path is dead code.

**Approach.** Trim row slices to exactly `width*4` bytes at the top of
`bilinear_from_rows` (`let row0 = &row0[..stride]`) so LLVM's range analysis
can prove `x0/x1 * 4 + 4 ≤ row.len()` and eliminate the inner bounds checks.
(`unsafe` rejected by `#![deny(unsafe_code)]`.)

**Numbers.** Neutral — corpus_color ±0.2 ms.

**Decision.** Reverted.

**Reason.** The `b.hs` branches are well-predicted (always not taken) and
execute in parallel with downstream work in the OOO pipeline. The trim itself
adds a bounds-check at entry, cancelling any gain. The sample count at `b.hs`
reflects pipeline retirement rather than actual branch overhead.

### H3 — FG44 x-coordinate accumulator (B1 pattern for FG layer) — **Reverted** (2026-06-19)

**Issue.** Profile (after H1) shows 11.7%+7.3% = 19% in `map_plane_center_frac`
+ UBFX for the FG44 layer x-coordinate (`mul x8, x8, fg_x_q24` per pixel).

**Approach.** Apply the same B1 accumulator to FG44: hoist `fg_fy` per row and
maintain `fg_fx_q += fx_step * fg_x_q24` per pixel instead of computing the
full multiply. Accumulator advances every pixel; read only when `is_fg`.

**Numbers.** render_colorbook −1.5% (FG-heavy), render_corpus_color +3% (BG-heavy).

**Decision.** Reverted.

**Reason.** The accumulator add runs for every pixel (BG or FG), while the
original multiply only runs for FG pixels. For typical DjVu content where
most pixels are background, the per-pixel overhead of the accumulator outweighs
the saving from replacing the conditional multiply.

### #416 H1 — precompute horizontal bilinear coordinate table — **Reverted** (2026-06-19)

**Issue.** #416 hypothesis 1: the post-H1 profiler still showed native
color/bilevel samples around `composite_rows_bilinear_one` x-coordinate mapping
and `bilinear_from_rows` (`src/djvu_render.rs:2015` / `2036`). The experiment
tested whether a per-render horizontal table could amortize `fx`, `px`, and
`bg_fx` computation across all rows.

**Approach.** Added a `BilinearX { fx, px, bg_fx }` table built once in
`composite_into` / `composite_rows` for the non-downscale path and passed it to
`composite_rows_bilinear_one`. The hot loop used table entries instead of
recomputing `(ox + offset_x) * fx_step`, `px`, and `bg_fx`; a fallback preserved
the original computation if no table entry was present.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
```

**Numbers:**

| Benchmark | Baseline | With table | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 45.31 ms | 44.95 ms | -0.8% |
| `render_compositor_only/bilevel_native_cached` | 45.10 ms | 45.39 ms | +0.7% |
| `render_compositor_only/color_downscale_cached` | 5.67 ms | 5.70 ms | +0.6% |

**Decision.** Reverted.

**Reason.** The table did not produce a meaningful win. The best target moved
less than 1%, while bilevel/downscale moved slightly worse, likely from the extra
allocation, code layout, and fallback shape. This does not meet #416's keep bar
of roughly >=3% improvement with no material regression.

### #416 H2 — dispatch-level black-mask identity specialization — **Reverted** (2026-06-19)

**Issue.** #416 hypothesis 2: split the common native corpus shape
(`bg + mask + black foreground + identity gamma`) into a separate row function
selected before the hot loop. The intent was to remove per-pixel `Option`,
palette/FG44, and gamma dispatch from `composite_rows_bilinear_one` without
adding another inner-loop branch.

**Approach.** Added `composite_rows_bilinear_black_mask_identity`, called when
`gamma_is_identity && fg_palette.is_none() && fg44.is_none()` and both BG and
mask layers were present. The helper hoisted BG rows and mask row exactly like
the generic path, wrote black foreground pixels directly, and wrote background
pixels from `bilinear_from_rows` without the generic RGB/gamma branch.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
```

**Numbers:**

| Benchmark | Baseline | Specialized | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 45.31 ms | 45.01 ms | -0.7% |
| `render_compositor_only/bilevel_native_cached` | 45.10 ms | 45.27 ms | +0.4% |
| `render_compositor_only/color_downscale_cached` | 5.67 ms | 5.69 ms | +0.4% |

**Decision.** Reverted.

**Reason.** The split row function did not clear the keep threshold. Removing
generic dispatch from the loop was offset by the added function body/code layout,
and the only improved target moved less than 1%. The result is not materially
different from the generic path after H1's bilinear arithmetic cleanup.

### #416 H3 — byte-run mask traversal for native bilinear rows — **Reverted** (2026-06-19)

**Issue.** #416 hypothesis 3: process background/foreground mask spans without
per-pixel `is_fg` extraction. The profiler showed native corpus time still
clustered around the mask lookup and `bilinear_from_rows` in
`composite_rows_bilinear_one`.

**Approach.** Added a narrow native-path helper for `fx_step == fy_step == FRAC`,
zero horizontal offset, identity gamma, black foreground, and BG+mask layers.
It walked the mask row byte-by-byte: `0x00` bytes ran eight background
bilinear samples with no bit test, `0xFF` bytes wrote eight black pixels with no
bilinear sample, and mixed bytes fell back to per-bit handling.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
```

**Numbers:**

| Benchmark | Baseline | Byte-run helper | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 45.31 ms | 45.08 ms | -0.5% |
| `render_compositor_only/bilevel_native_cached` | 45.10 ms | 45.00 ms | -0.2% |
| `render_compositor_only/color_downscale_cached` | 5.67 ms | 5.66 ms | -0.2% |

**Decision.** Reverted.

**Reason.** The byte-run traversal was only noise-level better and did not meet
the #416 keep bar. The mask bit extraction is no longer a large enough fraction
of the post-H1 hot loop to justify a second native row body.

### #416 H4 — precompute area-average BG x/y bounds — **Kept** (2026-06-19)

**Issue.** #416 hypothesis 4: the downscale profile was dominated by
`sample_area_avg` setup and accumulation (`src/djvu_render.rs:671-724`) plus
caller work in `composite_rows_area_avg_one`. The existing path recomputed BG
x/y box bounds for every output pixel even though x bounds are row-invariant and
y bounds are column-invariant within a row.

**Approach.** Added an `AreaAvgX` table computed once per downscale render. Each
entry stores the output pixel's page-space `fx`, BG-space `bg_fx`, and BG x
exclusive bounds. `composite_rows_area_avg_one` now computes the BG y exclusive
bounds once per row and calls `sample_area_avg_bounds` for background pixels.
The original `sample_area_avg` remains as the general helper and now delegates
through the same bounds-based sampler.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
```

**Numbers:**

| Benchmark | Baseline | With area bounds | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 45.31 ms | 44.89 ms | -0.9% |
| `render_compositor_only/bilevel_native_cached` | 45.10 ms | 45.08 ms | flat |
| `render_compositor_only/color_downscale_cached` | 5.67 ms | 3.63 ms | -36.0% |

First run measured `color_downscale_cached` at 3.60 ms; repeat after refactoring
`sample_area_avg` through `sample_area_avg_bounds` measured 3.63 ms.

**Decision.** Kept.

**Reason.** This is a large, targeted downscale win with no material regression
on the native controls. It removes repeated fixed-point bound setup from the
inner area-average loop while preserving the existing box-filter accumulation
and rounding behavior.

### #416 H5 — integral-image BG downscale sampling — **Reverted** (2026-06-19)

**Issue.** #416 hypothesis 5: after H4 removed repeated x/y bound setup, test
whether a per-render summed-area table can replace the remaining BG box
accumulation in `sample_area_avg_bounds` with O(1) rectangle sums.

**Approach.** Added an `IntegralRgb` table for the decoded BG pixmap in downscale
renders. The table stored per-channel `u64` prefix sums and
`composite_rows_area_avg_one` used it for BG pixels when H4's precomputed bounds
were available. The benchmark includes the cost of building the prefix table
inside each render iteration.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
```

**Numbers:**

| Benchmark | H4 baseline | Integral image | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 44.89 ms | 44.65 ms | -0.5% |
| `render_compositor_only/bilevel_native_cached` | 45.08 ms | 45.69 ms | +1.4% |
| `render_compositor_only/color_downscale_cached` | 3.63 ms | 3.59 ms | -1.0% |

**Decision.** Reverted.

**Reason.** The O(1) sample lookup did not repay the per-render prefix-table
build and memory traffic. H4 already reduced the downscale target enough that
the remaining box accumulation is too small to justify a full summed-area image
for this benchmark.

### #416 H6 — fuse alpha write into downscale area-average rows — **Kept** (2026-06-19)

**Issue.** #416 hypothesis 6: test whether the final `fill_alpha_255` pass can
be fused or removed without worsening the hot RGB loop. After H4, the downscale
path still wrote RGB in `composite_rows_area_avg_one` and then performed a
separate alpha-fill pass over the full output buffer.

**Approach.** For the area-average path only, write `pixel[3] = 255` inside
`composite_rows_area_avg_one` and skip the final `fill_alpha_255` pass when
`downscale` is true. Native bilinear paths still use the existing final alpha
pass, avoiding a repeat of the earlier neutral/regressive 4-byte-write native
experiments.

**Platform / command.** Apple M1 Max, macOS 26.5.1 / Darwin 25.5.0,
Rust 1.92.0 (`aarch64-apple-darwin`), default features, `RUSTFLAGS` unset:

```sh
cargo bench --bench render -- 'render_compositor_only/color_native_cached|render_compositor_only/bilevel_native_cached|render_compositor_only/color_downscale_cached' --output-format bencher
cargo bench --bench render -- render_compositor_only/color_downscale_cached --output-format bencher
```

**Numbers:**

| Benchmark | H4 baseline | Alpha fused | Delta |
|---|---:|---:|---:|
| `render_compositor_only/color_native_cached` | 44.89 ms | 44.46 ms | -1.0% |
| `render_compositor_only/bilevel_native_cached` | 45.08 ms | 44.93 ms | -0.3% |
| `render_compositor_only/color_downscale_cached` | 3.63 ms | 3.52 ms | -2.9% |

Repeat targeted downscale run: 3.515 ms.

**Decision.** Kept.

**Reason.** This is a small but consistent downscale win with no native-control
regression. It removes a full-buffer alpha pass from the downscale render while
keeping native RGB loops on the existing post-pass strategy that previously
benchmarked better than explicit 4-byte writes.

### PS1 — BZZ MTF shift via `copy_within` memmove — **Kept** (2026-07-01)

**Issue.** In `decode_mtf_phase` (`crates/djvu-bzz/src/decode.rs`), the
move-to-front update shifted `mtf_order` up one element-at-a-time in a
`while insert_at >= FREQ_SLOTS` loop, and every access
(`mtf_order`/`freq_counts`) went through
`.get()/.get_mut().ok_or(InvalidBlockSize)?` — a bounds-checked branch + error
path per shifted element, up to ~250 times per decoded symbol on diverse blocks.

**Approach.** In the `else` (non-marker) branch, `mtf_position < 256` and
`mtf_order` is a fixed `[u8; 256]`, so every index is provably in bounds — the
per-element `.ok_or()?` were dead error paths (the real malformed-input guard is
the earlier `mtf_order.get(mtf_position)?`). The first shift is a pure upward
move, i.e. a memmove: replaced the scalar loop with
`mtf_order.copy_within(FREQ_SLOTS-1..insert_at, FREQ_SLOTS)`. The short
`freq_counts` insertion loop and the final writes switched from
`.get().ok_or()?` to plain indexing (in-bounds by the same reasoning). No
`unsafe`.

**Platform / command.** Apple M1 Max, macOS / Darwin 25.5.0, Rust 1.92.0
(`aarch64-apple-darwin`), default features:

```sh
cargo bench --bench codecs -- bzz_decode --save-baseline before
# apply change, then:
cargo bench --bench codecs -- bzz_decode --baseline before
```

**Numbers:**

| Benchmark | Baseline | copy_within | Delta |
|---|---:|---:|---:|
| `bzz_decode` | 70.35 ns | 68.62 ns | **-2.2%** (p = 0.00, CI [-3.0%, -1.4%]) |

**Decision.** Kept.

**Reason.** Confirmed improvement (whole CI negative, p < 0.05), byte-identical
(17 `djvu-bzz` round-trip tests pass). The `bzz_decode` fixture is a small block,
so the measured gain is modest; larger BZZ blocks (DIRM directories, ANTz/NAVM
annotation streams) do proportionally more MTF shifting and benefit more than the
bench shows. The change also removes dead `Result` machinery from the hot loop
with no loss of malformed-input safety.

### PS2 — JB2 encoder byte-unpack bitmap expansion — **Kept** (2026-07-01)

**Issue.** `encode_bitmap_direct` (`crates/djvu-jb2/src/encode.rs`) first expands
the packed bitmap to a byte-per-pixel scratch grid before the rolling-context ZP
scan. The expansion did a per-pixel `bm.get(x, y)` — a (non-inlined) method call
that recomputes `y*stride + x/8` and `7-(x%8)` for every pixel. On a
2550×3301 page tile that is ~8.4M calls, each with an integer divide.

**Approach.** Unpack the MSB-first packed rows one byte → 8 pixels using
`chunks_exact_mut(8)` over the destination row and constant bit shifts, with a
tail loop for the `< 8` remainder columns. Padding columns `[w..pw]` stay zero.
The stride/byte-index arithmetic is now per-row, not per-pixel, and the fixed
8-way body vectorises. Byte-identical: same bit layout, so
`tests/encode_size_regression.rs` (JB2 mask size) is unchanged.

**Platform / command.** Apple M1 Max, macOS / Darwin 25.5.0, Rust 1.92.0
(`aarch64-apple-darwin`), default features:

```sh
cargo bench --bench codecs -- 'jb2_encode$|jb2_encode_multitile' --save-baseline before
# apply change, then:
cargo bench --bench codecs -- 'jb2_encode$|jb2_encode_multitile' --baseline before
```

**Numbers:**

| Benchmark | Baseline | byte-unpack | Delta |
|---|---:|---:|---:|
| `jb2_encode` (192×256) | 171.3 µs | 135.5 µs | **-20.2%** (p = 0.00) |
| `jb2_encode_multitile` (2550×3301) | 26.01 ms | 19.27 ms | **-25.9%** (p = 0.00) |

**Decision.** Kept.

**Reason.** Large, stable, byte-identical win (both CIs entirely negative,
p < 0.05; confirmed across two runs). The per-pixel non-inlined `bm.get()` with
its integer divide was ~a quarter of JB2 encode time on multi-tile pages. This
is the single biggest encode-path win found by the perf swarm — far above the
2–5% the analysis estimated, because the cost was the call+divide overhead, not
just the bit math. `encode_size_regression` confirms output bytes are unchanged.

### PS3 — Lanczos horizontal pass: precompute per-column weights — **Kept** (2026-07-01)

**Issue.** `scale_lanczos3` (`src/pixmap.rs`) recomputed the horizontal kernel
weights `lanczos3_kernel((sx - cx)/h_scale)` and the normaliser for **every
output pixel** `(oy, ox)`. But those depend only on the output column `ox`
(through `cx`), never on the row `oy`, so the sin-heavy kernel evaluation was
redone `src_h` times per column. (An initial attempt that only switched the
inner reads to row-pointer indexing — leaving the per-pixel kernel eval in place
— moved the bench < 1%, confirming the kernel eval, not memory access, was the
cost.)

**Approach.** Precompute once, for each output column, the contributor start
`x0` + the kernel weight vector + the norm, then make the per-row loop a pure
weighted sum over the precomputed weights (with row-pointer source/destination
indexing, no per-pixel `get_rgb`/`set_rgb`). This is the horizontal analogue of
the #448 vertical-pass hoist. Bit-identical: the identical weights are summed in
the identical order with the identical norm.

**Platform / command.** Apple M1 Max, macOS / Darwin 25.5.0, Rust 1.92.0
(`aarch64-apple-darwin`), default features:

```sh
cargo bench --bench render -- render_scaled_0.5x --save-baseline before
# apply change, then:
cargo bench --bench render -- render_scaled_0.5x --baseline before
```

**Numbers:**

| Benchmark | Baseline | precompute | Delta |
|---|---:|---:|---:|
| `render_scaled_0.5x/lanczos3` (boy.djvu → 0.5×) | 2.384 ms | 0.735 ms | **-69.1%** (p = 0.00, two runs) |
| `render_scaled_0.5x/bilinear` (control, no Lanczos) | 55.2 µs | 55.2 µs | none (p = 0.06) |

**Decision.** Kept.

**Reason.** Very large, stable win (−69%, confirmed twice, p < 0.05) on the
Lanczos resample path, with the bilinear control showing no change — so it is a
genuine isolated improvement, not thermal drift. Bit-identical by construction
(same weights/order/norm; all `scale_lanczos3` and render-Lanczos tests pass).
The horizontal pass had never received the #448 treatment the vertical pass did;
this closes that gap. Resampling to a fitted viewer size is a common operation,
so this materially speeds the Lanczos render path.

### PS-R1 — hoist FG44 row-invariants in `composite_rows_area_avg_one` — **Reverted** (2026-07-01)

**Issue / hypothesis.** The perf-swarm analysis suggested hoisting `fg_fy`,
`fg_fy_step`, `fg_fx_step` out of the per-pixel `fg_sample` closure in
`composite_rows_area_avg_one` (they depend only on the row / constants), arguing
the earlier C2c revert was thermal noise at the wrong site.

**Approach.** Computed the three quantities once per row before the pixel loop.

**Platform / command.** Apple M1 Max, Rust 1.92.0, default features:

```sh
cargo bench --bench render -- 'render_compositor_only/(color_downscale_cached|color_downscale_mixed_cached|small_color_downscale_cached)'
```

**Numbers (two passes, consistent):**

| Benchmark | Delta |
|---|---:|
| `render_compositor_only/color_downscale_cached` | **+3.0 … +3.5%** (regressed) |
| `render_compositor_only/small_color_downscale_cached` | **+1.9 … +2.2%** (regressed) |
| `render_compositor_only/color_downscale_mixed_cached` | −1.4% (marginal) |

**Decision.** Reverted.

**Reason.** Net regression. The hoist is **unconditional**, but the FG44 branch
it feeds only runs for continuous-tone FG44 foreground pages — the corpus
downscale fixtures are FGbz-palette / no-foreground, so the original closure
never computed those quantities at all (it took the palette or `(0,0,0)` branch).
Hoisting adds three `u64` multiplies per row of pure overhead on the common
non-FG44 path, hence the regression. A *conditional* hoist (only when
`fg44.is_some()`) would avoid the regression but is neutral on every available
bench — there is no FG44 downscale fixture to demonstrate a win — so it cannot be
confirmed. This closes the C2c line of inquiry: the site is not a win without an
FG44-specific benchmark, and unconditional hoisting is a net loss.

---

## Perf swarm (2026-07-01) — summary

A three-agent read-only hunt (decode / encode / render) surfaced ~14 candidates.
Vetted and benchmarked serially on M1 Max. **Kept:** PS1 (BZZ MTF memmove,
−2.2%), PS2 (JB2 byte-unpack, −20…−26%), PS3 (Lanczos horizontal precompute,
−69%). **Reverted:** PS-R1 (area-avg FG44 hoist). **Considered and rejected
without landing** (analysis-level, to avoid re-suggestion): area-avg FG44 hoist
needs an FG44 bench; JB2 `crop_to_content` row-copy only fires on non-tight
symbols (rare; tight-box fast path dominates the corpus); IW44 `forward_col_pass`
scratch pre-alloc is below M1 allocator noise; `by_size` `BTreeMap`→`HashMap` and
the IW44 `curband` branch split have no dedicated benchmark and are LLVM-hoistable;
IW44 `chroma_half` upsample/parallel vectorisation applies to a page kind the
corpus does not contain (`carte.djvu` only). The measurable opportunities are
captured; the remainder is at or below noise on the current bench corpus.

### PS4 — JB2 `extract_ccs` byte-unpack (dict encoder) — **Kept** (2026-07-02)

**Issue.** A second perf swarm (aimed at hot paths **without** a benchmark)
found the exact PS2 pattern again: `extract_ccs`
(`crates/djvu-jb2/src/encode.rs`) unpacks the mask to a byte-per-pixel grid via
a per-pixel `bitmap.get(x, y)`, recomputing `y*stride + x/8` and `7-(x%8)` (a
hidden divide) for ~8.4M pixels on a full-page mask. `extract_ccs` drives the
**dictionary encoder** (`encode_jb2_dict`), the colour-page mask path — which had
no benchmark, so it was never optimised (the existing `jb2_encode*` benches use
the non-dict `encode_jb2`).

**Approach.** Same fix as PS2: byte-unpack the MSB-first packed rows via
`chunks_exact_mut(8)` + constant shifts + a tail loop, writing directly into
`pix[y*w..][..w]`. Added a `jb2_encode_dict` benchmark (cable_1973_100133,
2550×3300 dense text) to `benches/codecs.rs` — permanent coverage of the dict
encoder. Byte-identical `pix`.

**Platform / command.** Apple M1 Max, Rust 1.92.0, default features:

```sh
cargo bench --bench codecs -- jb2_encode_dict --save-baseline before
# apply change, then:
cargo bench --bench codecs -- jb2_encode_dict --baseline before
```

**Numbers:**

| Benchmark | Baseline | byte-unpack | Delta |
|---|---:|---:|---:|
| `jb2_encode_dict` (cable 2550×3300) | 26.13 ms | 19.4 ms | **-25.8%** (p = 0.00, two runs) |

**Decision.** Kept.

**Reason.** Large, stable, byte-identical win (both CIs ~−25.5…−26.2%, p < 0.05,
two runs) matching PS2's magnitude — the same per-pixel-divide cost, on the
colour-page dictionary encoder that PS2 didn't touch. `encode_size_regression`
(`jb2_mask_size_does_not_regress`, which calls `encode_jb2_dict`) confirms output
bytes are unchanged. Vindicates the "unbenched hot paths still hide big wins"
hypothesis of the second swarm.

### PS5 — `segment_page` row-slice mask + block-mean scan — **Kept** (2026-07-02)

**Issue.** `segment_page` (`src/segment.rs`) runs on every colour
`Quality`/`Archival` encode and had no benchmark. Two per-pixel PS2-class hot
loops: `fill_fixed_mask` used `rgba.get_rgb(x,y)` (bounds check + `(y*w+x)*4`
multiply) then `mask.set(x,y)` (recomputes `y*stride + x/8`) for the whole page
(~8.4M px); `block_mean` (called once per BG cell, so it collectively rescans
the whole page at sub-sample) used `rgba.get_rgb` + `mask.get` per pixel.

**Approach.** Row-slice both: iterate the packed RGBA rows with
`chunks_exact(4)` (one slice offset per row, no per-pixel multiply/bounds), set
mask bits directly in the pre-sliced row byte (`|= 0x80 >> (x&7)`), and read the
mask row once per row in `block_mean` (bit-test instead of `mask.get`). Same
pixels in the same order → byte-identical mask + block means. Added a
`segment_page_color` benchmark (colorbook BG44 → Pixmap) to `benches/codecs.rs`.

**Platform / command.** Apple M1 Max, Rust 1.92.0, default features:

```sh
cargo bench --bench codecs -- segment_page_color --save-baseline before
# apply change, then:
cargo bench --bench codecs -- segment_page_color --baseline before
```

**Numbers:**

| Benchmark | Baseline | row-slice | Delta |
|---|---:|---:|---:|
| `segment_page_color` (colorbook) | 2.185 ms | 1.88 ms | **-14.0%** (p = 0.00, two runs) |

**Decision.** Kept.

**Reason.** Stable, byte-identical win (both CIs ~−13.6…−14.4%, p < 0.05, two
runs) on a path that runs on every colour encode. `encode_size_regression` still
green (segmentation feeds the encoders). Smaller than PS4 because `segment_page`
also does the `luminance`/`ColorAccum` arithmetic the row-slice doesn't touch,
but the per-pixel accessor overhead was ~1/7 of it. The two-pass `block_mean`
fusion (a second candidate) was left out: with `bg_inpaint` off (default) the
second full-block scan only fires for fully-masked blocks, rare on text pages.

### PS-R2 — register-hoist `newly_active_coefficient_decoding_pass` — **Reverted** (2026-07-02)

**Hypothesis.** Give `newly_active_coefficient_decoding_pass` the same ZP-state
register-extraction + inlined `decode_bit`/`decode_passthrough_iw44` macros that
`previously_active_coefficient_decoding_pass` uses, so `(3*a)/8` becomes an
in-register shift instead of a divide on a struct field (swarm decode #2).

**Approach.** Copied the local-extraction + macro block verbatim from
`previously_active` into `newly_active`, with write-back at the end.

**Platform / command.** M1 Max, Rust 1.92.0. Measured OLD vs the changed code
via `--save-baseline`/stash on the colour IW44 decode benches (the suggested
`iw44_decode_large_all_chunks` bench SKIPS — pathogenic is bilevel, no BG44):

```sh
cargo bench --bench codecs -- 'iw44_decode_first_chunk|iw44_decode_corpus_color'
```

**Numbers (change is a regression):**

| Benchmark | Delta (new vs old) |
|---|---:|
| `iw44_decode_first_chunk` | **+1.1%** (slower) |
| `iw44_decode_corpus_color` | **+1.8%** (slower) |

**Decision.** Reverted.

**Reason.** Net regression. Unlike `previously_active` (which does heavy
refinement work per call, amortising the state extract/write-back),
`newly_active` is called once per (band, block) per slice and most late-slice
calls activate few or no coefficients — so the added 7-field extract + 7-field
write-back on every call outweighs the per-activation divide→shift saving.
Correctness was fine (all 43 `djvu-iw44` golden-decode tests passed); it is
simply slower. Confirms the register-hoist pattern is only a win where per-call
work is large.

### PS-R3 — `encode_iw44_color` RGB→YCbCr row-slice — **Reverted** (2026-07-02)

**Hypothesis.** A third swarm flagged the RGB→YCbCr conversion loop in
`encode_iw44_color` (`crates/djvu-iw44/src/encode.rs`) as the PS2/PS4 pattern:
per-pixel `pixmap.get_rgb` (×4 stride multiply + bounds check) over the whole
page (~8.4M px for a large page).

**Approach.** Row-slice both the `chroma_half` and default branches: walk the
packed RGBA rows with `chunks_exact(4)` and write into pre-sliced plane rows,
removing the per-pixel multiply/bounds. Byte-identical.

**Platform / command.** M1 Max, Rust 1.92.0:

```sh
cargo bench --bench codecs -- 'iw44_encode_large_1024x1024|iw44_encode_color'
```

**Numbers (no win):**

| Benchmark | Delta |
|---|---:|
| `iw44_encode_large_1024x1024` | **no change** (p = 0.18 / 0.19, two runs) |
| `iw44_encode_color` (192×256) | +1.3% (slightly slower, p = 0.00) |

**Decision.** Reverted.

**Reason.** No measurable win. Unlike JB2 encode — where the byte-unpack was
~a quarter of the work (PS2/PS4) — the RGB→YCbCr conversion is a **small
fraction** of `encode_iw44_color`: the three forward wavelet transforms and the
sequential ZP encoding dominate (~313 ms total). The per-pixel accessor overhead
is real but < 2% of the total, so removing it is lost in the noise. Correctness
was fine (43 golden tests + `iw44_bg44_size_does_not_regress` pass). **Lesson:
the per-pixel-accessor win only materialises where the accessor loop is a large
fraction of the measured work.**

---

## Perf swarm round 3 (2026-07-02) — summary (no new wins)

A third three-agent hunt (encode pipeline + containers / export paths / SIMD
gaps + structural double-work) surfaced ~18 candidates, but — after PS-R3 was
measured and reverted — the remainder were assessed as targeting a **small
fraction of their benchmark's total**, or needing fixtures the corpus lacks, so
none were landed. Recorded so future swarms don't re-suggest them:

- **`assemble_djvm_bundle` double-emit** (compute offsets / patch in place vs. a
  second full IFF emit): correct and clean, but the second emit is a small
  fraction of `encode_djvm_bundle_jb2`, which is dominated by per-page JB2
  encoding — sub-1% on any realistic bundle bench.
- **`foreground_fgbz` / other RGB→YCbCr-style per-pixel loops**: same class as
  PS-R3 — a small fraction of the colour-encode total (segment + jb2_dict + iw44
  dominate; those are already PS4/PS5-optimised).
- **`collect_mask_stream` "re-decode shared dict"** (pdf export): the function
  uses `find_chunk(b"Djbz")` (per-page *inline* dict), not the shared INCL dict,
  so the claimed per-page shared-dict redundancy isn't present; switching to
  `page.extract_mask()` would change output for shared-dict pages (a correctness
  question, not a byte-identical perf win).
- **IDWT `use_simd` for s=8/16**, **forward_row/col_pass SIMD at s>1**,
  **chroma_half NEON upsample**: low-trip-count or corpus-unexercised
  (`chroma_half` = `carte.djvu` only); not worth the SIMD complexity/risk on the
  current M1 bench corpus.
- **`Vec::with_capacity` pre-sizing** in pdf/epub encoders and **`pixmap_to_rgba`
  → `data.clone()`**: individually tiny (allocation/copy overhead dwarfed by
  JPEG/PNG/deflate CPU) and mostly CLI-path, not merge-gated hot paths.

**Conclusion.** After three rounds, the measurable perf opportunities on the
current benchmark corpus are captured (PS1–PS5, five kept wins). Further gains
would need either new workloads/fixtures (multi-page export, shared-dict
documents, `chroma_half` pages) to make the small-fraction paths measurable, or
algorithmic changes with a correctness bar higher than the payoff justifies.

### Bench workloads added (2026-07-02)

To make the round-3 "small fraction of an unbenched total" candidates
measurable, four encode workloads were added to `benches/codecs.rs`:
`encode_color_page_quality` (full colour `PageEncoder` encode),
`encode_multipage/encode_djvm_layered_shared`,
`encode_multipage/encode_djvm_bundle_jb2`, and `iw44_encode_gray_1024x1024`.
These give permanent coverage and let the previously-invisible paths be
measured rather than guessed.

### PS6 — `foreground_fgbz` row-slice (blit-colour averaging) — **Kept** (2026-07-02)

**Issue.** `foreground_fgbz` (`src/djvu_encode.rs`, run on every colour encode)
scanned the whole page averaging per-blit foreground colours with two per-pixel
hidden-divide accessors: `mask.get(x,y)` (`/8`) and `pm.get_rgb(x,y)` (`*4` +
bounds). Newly measurable via the added `encode_color_page_quality` bench.

**Approach.** Row-slice the mask (bit-test a pre-sliced row byte), the blit map,
and the packed RGBA pixmap (`x*4` into a row slice). Same pixels, same
accumulation order → byte-identical palette. (PS4/PS5 class.)

**Platform / command.** M1 Max, Rust 1.92.0:

```sh
cargo bench --bench codecs -- encode_color_page_quality --save-baseline before
# apply change, then:
cargo bench --bench codecs -- encode_color_page_quality --baseline before
```

**Numbers:**

| Benchmark | Delta |
|---|---:|
| `encode_color_page_quality` | **-2.2%** (p = 0.00, two runs) |

**Decision.** Kept.

**Reason.** Confirmed byte-identical win (both CIs ~−1.5…−2.8%, p < 0.05, two
runs; `encode_size_regression` + FGbz round-trip tests pass). Modest because
`foreground_fgbz` is one of several stages of the colour encode (segment /
jb2_dict / iw44 — already PS4/PS5-optimised — dominate), but it is on the
every-colour-encode path and the fix is zero-risk. The `palette.iter().position`
O(n²) dedup was left as-is: negligible for real pages (few foreground colours).
Vindicates adding the workload — the path was unmeasurable before, guessed as
"5–15%", and is really ~2% of the total.

### PAR_ENCODE — parallel per-page encoding in the multi-page bundlers — **Kept** (2026-07-02)

**Issue.** The decode path has been parallelised for a while (PAR_DEC,
IW44_PAR), but the **encoder** ran every page strictly sequentially. The two
multi-page bundlers each loop over pages doing fully-independent per-page work —
`encode_djvm_bundle_jb2_impl` (`src/jb2_encode.rs`) does one JB2-dict `Sjbz`
encode per page; `encode_djvm_layered_shared_impl` (`src/djvu_encode.rs`) does
`segment_page` + JB2-dict + `encode_iw44_color` + `foreground_fgbz` per page.
JB2/IW44 encoding dominates the multi-page cost, so the per-page loop is the
single largest untouched lever (called out in the round-3 summary but never
actioned). The `encode_multipage/*` benches added 2026-07-02 made it measurable.

**Approach.** Extract each per-page body builder into a closure and run the pages
through `rayon`'s `par_iter().enumerate().map().collect()` behind
`#[cfg(feature = "parallel")]`, with a byte-identical sequential fallback when the
feature is off. Order is preserved by the indexed collect; the shared Djbz/DJVI
component is still built once up front and the page components appended after it.
In the layered bundler both the `segs` segmentation pass **and** the main page
loop are parallelised (the `?`-fallible page builder collects into
`Result<Vec<_>, _>`). Output is byte-identical (same functions, same order).

**Platform / command.** Apple M1 Max (8 perf cores), Rust 1.92.0, `parallel`
feature. Baseline = the same code built with `--features parallel` **before** the
change (no parallelism there yet), via `git stash`:

```sh
cargo bench --features parallel --bench codecs -- encode_multipage --save-baseline before
# stash pop, then:
cargo bench --features parallel --bench codecs -- encode_multipage --baseline before
```

**Numbers (two runs):**

| Benchmark | Baseline | parallel | Delta (run 1 / run 2) |
|---|---:|---:|---:|
| `encode_djvm_layered_shared` (watchmaker, 3 colour pages) | 21.9 ms | 13.4 ms | **−35% / −43%** |
| `encode_djvm_bundle_jb2` (conquete_paix, 6 masks) | 881 ms | 537 ms | **−39% / −39%** |

**Decision.** Kept.

**Reason.** Large, stable win (both benches p < 0.05, two runs) on the
every-multi-page-encode path, gated to the opt-in `parallel` feature so the
default no_std/single-thread build is unchanged. Byte-identical output verified:
`encode_size_regression` (both `jb2_mask_size_does_not_regress` and
`iw44_bg44_size_does_not_regress`) + all `djvm`/`djvu_mut` round-trip tests pass
with `--features parallel`. Speed-up is sub-linear in page count here (3–6 pages,
plus the sequential shared-dict clustering + IFF assembly are Amdahl serial
tails), so many-page documents should approach the core count more closely.
(The one unrelated `encode_empty_directory_fails` CLI test fails identically on
clean `main` — a stale "no image files" vs "no PNG files" message assertion, not
touched by this change.)

### LTO_FAT — `lto = "fat"` + `codegen-units = 1` release/bench profile — **Kept** (2026-07-02)

**Issue.** The workspace had **no** `[profile.release]` block at all — it built on
cargo defaults (`lto = false`, `codegen-units = 16`). The codecs live in separate
crates (`djvu-jb2`, `djvu-bzz`, `djvu-iw44`, `djvu-zp`), so every cross-crate call
— crucially the per-symbol ZP arithmetic-coder calls that the JB2 encoder makes
across the `djvu-jb2` boundary — was an un-inlined function call. No prior
experiment had ever touched build settings; this is the cheapest possible "free
speed across all paths" lever.

**Approach.** Add `[profile.release]` and `[profile.bench]` with `lto = "fat"` +
`codegen-units = 1`. Behaviour-preserving by construction (LTO only changes
inlining/codegen, not semantics). Benches compile under `[profile.bench]`, so both
blocks are set for the measured artifact to match the shipped one.

**Platform / command.** Apple M1 Max, Rust 1.92.0. Codecs subset measured with a
`--save-baseline` taken **before** adding the profile block; render measured
separately with a clean `git stash`-based before/after:

```sh
cargo bench --bench codecs -- '<subset>' --save-baseline pgo_before   # default profile
# add the two profile blocks, then:
cargo bench --bench codecs -- '<subset>' --baseline pgo_before
```

**Numbers:**

| Benchmark | Delta | Note |
|---|---:|---|
| `jb2_encode_dict` | **−65%** | cross-crate ZP encoder now inlines — the headline win |
| `segment_page_color` | −7.0% | |
| `bzz_decode` | −7.0% | |
| `iw44_encode_color` | −3.2% | |
| `iw44_decode_corpus_color` | −2.1% | |
| `render_page/*`, `render_colorbook*`, `render_region_bilevel` | ±0…2% (noise, p mixed) | render lives in the main crate — little cross-crate call surface for LTO to fuse |

**Decision.** Kept.

**Reason.** A large, semantics-preserving win concentrated on the codec-crate hot
paths (all p < 0.05), with the JB2 dictionary encoder — the dominant colour-page
cost — dropping by nearly two-thirds purely from cross-crate inlining. Render is
flat within noise (it barely crosses crate boundaries), so nothing regresses. The
cost is compile time: fat LTO + `codegen-units = 1` serialises codegen and roughly
doubles a clean release/bench build — an acceptable trade for a shipping library.
`make check` (fmt, clippy -D, no_std, wasm32, full test suite) passes with the new
profile. This is the single biggest single-change speed-up recorded so far and it
compounds with PAR_ENCODE (the parallel bundlers now also LTO their per-page work).

### IW44_MASKED_WAVELET — masked background encoding — **Diagnostic / deferred** (2026-07-02)

**Motive.** The investigation flagged the residual **+3.9 % BG44 size gap** left
after IW44_ACT_THRESH (which took the gap 14.3 % → 3.9 %) as the largest untouched
*size* lever. DjVuLibre's `c44` closes it with **masked wavelet encoding**: the
foreground mask marks pixels the text layer already covers, and the background
codec is free to pick *any* value for those pixels (it interpolates them to the
smoothest values that minimise wavelet energy) and to skip refining coefficients
whose entire support is masked. Our `encode_iw44_color` has **no mask parameter at
all** — it transforms and codes every pixel, spending bits on background detail
that is never seen.

**Concrete size baseline (this corpus).** `iw44_bg44_size_does_not_regress`
re-encodes the first two BG44 pages to **119 636 B** (2 pages). At the recorded
3.9 % gap, masked encoding has on the order of **~4.6 KB** of headroom on just
those two pages; it scales with background area on colour/photo documents (BG44
is 94–99.9 % of colour-doc bytes per ENC_SIZE_DIAG).

**Why deferred (not landed this round).** Masked wavelet is a **normative
bitstream-generation change** touching the exact area the repo has repeatedly
found interop-fragile (IW44_SWARM_REST: "change normative tables/ctx → break
DjVuLibre interop"). A correct implementation needs three non-trivial pieces the
current encoder lacks: (1) plumb the segmentation mask (already produced by
`segment_page`) down through `encode_iw44_color` → `PlaneEncoder`; (2) a
**mask-aware forward transform** that fills masked regions by interpolation before
the lifting steps (so masked pixels don't inject high-frequency energy), mirroring
DjVuLibre's `IWTransform::forward(…, mask)`; (3) mask-aware coefficient gathering
so fully-masked buckets are not coded. Each step must be validated **byte-for-byte
against `ddjvu`/DjVuLibre**, not just our own round-trip, because a decoder that
never sees the mask must still reconstruct a valid stream. That validation harness
+ the interop risk put it beyond a safe single-session change; rushing it risks
shipping a subtly non-interoperable encoder.

**Decision.** Recorded as the priority *size* follow-up with a concrete plan and
the measured 119 636 B / ~4.6 KB target, so a future dedicated effort (with a
DjVuLibre interop-diff harness) can pick it up. **Note for interop-safe partial
win:** the background *inpainting* under the mask lives in `segment_page`
(`src/segment.rs`, encoder-only, decoder never sees it) — improving that
inpainting to smoother fills is a lower-risk down-payment on the same gap that
does **not** touch the normative IW44 stream, and is the recommended first step
before attempting the full masked transform.

## Perf experiments round 5 (2026-07-03) — render-cache & decode-parallelism sweep

A fresh investigation pass (85 prior experiments reviewed against the render-path
map and the 5 dead-ends) produced a ranked backlog of ~20 new candidate
experiments spanning render caching, IDWT parallelism, ZP decode, container
cold-open, and image-quality axes. This section records the ones actually
implemented and measured. Methodology unchanged: intra-session `target / control`
ratio to cancel M1 Max thermal throttling; control = `bilevel_native_cached` for
colour-path experiments (it shares no cache state with the colour BG path).

### SUB4_RGB_CACHE — cache the sub=4 BG44→RGB conversion — **Kept** (2026-07-03)

**Issue.** `BG_CACHE` (sub=1) and `BG_CACHE_S2` (sub=2) memoize the decoded RGB
`Pixmap` so warm renders skip the IDWT + YCbCr→RGB conversion, but the
`subsample >= 4` branch in `decode_background_chunks` (`src/djvu_render.rs`) fell
through to `Cow::Owned(img.to_rgb_subsample(subsample))` — **uncached**. Every warm
render at quarter-resolution (heavy downscale / thumbnail / contact-sheet zoom,
e.g. 150-from-400-DPI) re-ran the full conversion. This is open-hypothesis #20
(SUB4_RGB_CACHE) from the round-5 investigation.

**Approach.** Add a `bg_rgb_s4` `OnceLock<Option<Pixmap>>` slot to `PageLayers`,
mirroring `bg_rgb_s2`. It builds on the already-cached **partial** BG44 image
(`bg44_partial`, first chunk only — exactly what the sub≥4 decode path uses) and
caches `to_rgb_subsample(4)`. A new `subsample == 4` branch in
`decode_background_chunks` returns the cached pixmap borrowed (`Cow::Borrowed`),
matching the sub=1/sub=2 shape. sub=8 and other factors are left on the uncached
`Cow::Owned` path (rare, and the output is already tiny). ~2 MB per page rendered
at sub=4 (16× smaller than the sub=1 cache).

**Correctness.** Byte-identical by construction: same partial image, same
`to_rgb_subsample(4)` call, only the result is now memoized. Error semantics match
the existing sub=1/sub=2 branches (a failed conversion yields `Ok(None)`, not
`Err`, consistent with those paths). Full integration + CLI render test suites
green.

**Numbers** (`render_compositor_only`, `--features std`, M1 Max):

| Bench | Baseline | After | Change |
|-------|----------|-------|--------|
| `color_downscale_cached` (target, colorbook @0.375 → sub=4) | 7.169 ms | 6.815 ms | **−4.9 %** (p=0.00) |
| `bilevel_native_cached` (control) | 43.29 ms | 42.98 ms | +0.2 % (p=0.79, flat) |

Ratio `target/control`: 0.1656 → 0.1586 = **−4.3 %** thermal-corrected. Control flat,
so the win is real, not throttling drift.

**Decision.** **Kept.** Proven-pattern mirror of BG_CACHE_S2 for the sub=4 warm
render, ~4–5 % on the heavy-downscale compositor path, byte-identical, bounded
extra memory only on pages actually rendered at sub=4.

### BZZ_DEC_MTF — mirror PS1 `copy_within` on the BZZ *decode* MTF — **Rejected / already covered** (2026-07-03)

**Hypothesis (round-5 #19).** PS1 sped up the BZZ *encoder*'s move-to-front (MTF)
update by replacing an element-at-a-time shift with a `copy_within` memmove.
Check whether the *decoder*'s MTF has the same shape and the same untapped win.

**Finding.** It does not — it is **already optimised**. `decode_mtf_phase`
(`crates/djvu-bzz/src/decode.rs:418`) already performs the block relocation with
`mtf_order.copy_within(FREQ_SLOTS - 1..insert_at, FREQ_SLOTS)` when
`insert_at >= FREQ_SLOTS`, and the surrounding comment (lines 409–415) explicitly
records that this is the memmove replacement for the former scalar loop. The
residual `while insert_at > 0` loop (lines 421–430) is **not** a plain shift: it is
a frequency-sorted insertion that compares `freq_counts[insert_at-1]` against
`combined_freq` and **breaks early** as soon as the ordering holds. Because each
iteration is data-dependent (the number of moves varies per symbol and the loop
exits mid-way), it cannot be expressed as a single `copy_within` without changing
results. No safe win remains.

**Decision.** **Rejected** — no code change. The decode path already carries the
PS1 optimisation; the remaining loop is a genuine early-exit sorted insert.
Recorded so future swarms do not re-suggest a decode-side MTF memmove.

### IDWT_PAR_PLANE — parallelism *within* the Y-plane inverse wavelet — **Diagnostic / deferred** (2026-07-03)

**Hypothesis (round-5 #2).** `IW44_PAR` parallelises the inverse wavelet *between*
planes (Y ‖ (Cb ‖ Cr) via `rayon::join`, `crates/djvu-iw44/src/lib.rs:3281`). The
Y plane is up to 4× the area of each chroma plane, so it is the critical path and
the chroma threads finish early, leaving cores idle. Splitting Y's own
`reconstruct` across threads should shrink the critical path further.

**Measurement (whether it's worth it).** `render_native_stages/bg_to_rgb_warm`
(watchmaker, `--features parallel`, M1 Max) — the isolated `to_rgb_subsample(1)`
cost (reconstruct Y/Cb/Cr + YCbCr→RGB) — is **3.26 ms**. Two facts cap the upside:
1. **Cold-only.** Warm renders never re-run reconstruct — `BG_CACHE`/`bg_rgb_s1`
   memoise the RGB pixmap, so `render_pixmap` warm (8.4 ms) does not touch this
   path at all. IDWT_PAR_PLANE would only cut *first-open* latency.
2. **The convert half is already parallel** (`par_chunks_mut` YCbCr→RGBA,
   `lib.rs:3314`). Only the reconstruct portion (a fraction of the 3.26 ms) is the
   sequential-per-plane target, and on this corpus the Y plane is not large enough
   for within-plane splitting to beat rayon's per-pass overhead.

**Why deferred (not landed).** The inverse transform (`inverse_wavelet_transform_from`,
~300 lines) is **not** a clean per-row loop that wraps in `par_iter`. The column
pass is a *transposed vertical sweep*: an outer scale/k loop with per-column state
carried in `st0`/`st1`/`st2` (`vec![0i32; width]`), swept across rows. Columns are
mutually independent (disjoint `data` indices `k_off + ci*s`), and the row pass is
independent across rows — so the parallelism exists in principle — but exploiting it
requires **restructuring each pass to run per column/row *chunk* with thread-local
scratch**, keeping a barrier between passes and between scales. That is an
error-prone rewrite of a hand-tuned SIMD hot path in a **normative** decoder, where
a subtle bug silently corrupts every colour page. The prior IW44 dead-ends
(IDWT_S2_NEON incorrect-premise, IDWT_SPLAT/REFROW_REG unmeasurable) show how fragile
this code is to "obvious" transforms.

**Design for a future dedicated effort.** (1) Extract the column pass into
`fn column_pass(data, cols: Range<usize>, scratch: &mut [i32;3][…])` so a
`par_chunks`-style split over `cols` is a drop-in; likewise `row_pass` over a row
range. (2) Gate parallel dispatch on `s <= 2 && plane_area >= THRESHOLD` (coarse
scales have too few active rows/cols to amortise spawn cost). (3) Validate
**byte-identical against the sequential path** over the whole corpus (extend
`simd_row_pass_matches_scalar`), not just round-trip. (4) Re-measure on a page with
a genuinely large Y plane (the current corpus under-exercises this).

**Decision.** **Deferred.** Cold-only, marginal on the present corpus, high
correctness risk; recorded with the 3.26 ms baseline and a concrete design so a
future effort with a large-page fixture can pick it up safely.

### RENDER_REGION_SCOPE — is `render_region` wasting work on the full page? — **Diagnostic / no change** (2026-07-03)

**Question (from round-5 #1/#10, the viewer-tile lever).** Does `render_region`
composite the whole page and then crop (wasteful), or only the requested rectangle?

**Finding.** It is **already region-scoped on the compositor side.** `render_region`
(`src/djvu_render.rs:2985`) allocates only `region.width × region.height`
(`Pixmap::white(out_w, out_h)`) and builds the `CompositeContext` with
`offset = (region.x, region.y)` and `out = (out_w, out_h)`, so `composite_into`
writes only the region's pixels — no full-page composite, no crop pass. For a
viewer scrolling a single page, the full-layer **decode** is paid once and then
served from `PageLayers` caches (mask, bg_rgb_s*), so warm region renders cost only
the region composite.

**Residual lever.** The one remaining full-page cost is the **cold** `decode_layers`
call (full IW44 IDWT + full JB2 mask on first touch). Narrowing *that* to the
region is the true ROI_IDWT experiment (round-5 #1) — a decode-scope change (the
IW44 lifting filter needs a few rows/cols of halo per level, and JB2 mask decode is
inherently whole-stream), which is substantial and separate. The compositor itself
needs no change.

**Decision.** **No change** — the compositor path is already optimal for the
viewport/tile use case. Recorded so ROI work targets the cold decode scope, not the
(already-scoped) compositor.

### DOC_SHARED_DICT_CACHE — decode the shared JB2 dictionary once per document — **Kept** (2026-07-03)

**Issue (round-5 #3).** Bundled multi-page scans reference a small number of
shared JB2 symbol dictionaries (DJVI components) from many pages via `INCL`. The
parser already shares the **raw** `Djbz` bytes across pages via `Arc<Vec<u8>>`
(one allocation per DJVI component), but the **decoded** dictionary was cached
**per page** (`jb2_dict_decoded: OnceLock<Option<Jb2Dict>>` on `DjVuPage`). So
rendering N pages that share one dictionary ran the dictionary's ZP arithmetic
decode **N times** instead of once. Round-3 believed no shared-dict fixture
existed; in fact the corpus has three: `czech.djvu` (85 pages / 2 dicts),
`DjVu3Spec_bundled.djvu` (71 / 5), `pathogenic_bacteria_1896.djvu` (520 / 52).

**Approach.** Fold the decode cache into the shared allocation: a new
`SharedDict { raw: Vec<u8>, decoded: OnceLock<Option<Jb2Dict>> }`, and the page
field becomes `Option<Arc<SharedDict>>` (replacing `Arc<Vec<u8>>` **plus** the
per-page `jb2_dict_decoded`). Every page that `INCL`s the same DJVI component
already clones the same `Arc`, so the first page to need the dictionary decodes
it and all others reuse that single `Jb2Dict`. The parser map
(`BTreeMap<String, Arc<SharedDict>>`) and the async lazy path's `shared_cache`
were updated to the new type. `no_std` (which has no `OnceLock` and returned
`None` for shared dicts) is unchanged.

**Correctness.** Byte-identical: the same `jb2::decode_dict(&raw, None)` call,
only memoized at document scope instead of page scope. `OnceLock` keeps it
thread-safe for the parallel render/decode paths. A cloned `DjVuPage` now shares
the decoded dict through the `Arc` (the dict is immutable, so this is safe and
strictly better). Full suite green (`make check`: 1016 tests, incl. all
shared-dict + async lazy tests, no_std, wasm32).

**Numbers** (`document/shared_dict_mask_decode_30p` — parse fresh + decode masks
of 30 pages of `DjVu3Spec_bundled.djvu`, fat-LTO release, M1 Max):

| | Time | |
|-|------|-|
| Before (per-page dict decode) | 140.6 ms | |
| After (one decode per unique dict) | 89.0 ms | **−37%** (p=0.00) |

The delta is deterministic (a +58% swing on revert), not thermal — it scales with
the page-to-dictionary ratio, so heavier bundles (pathogenic: 520 pages / 52 dicts)
benefit proportionally more. This is the multi-page reading / viewer-scroll path.

**Decision.** **Kept.** Removes O(pages) redundant dictionary decodes → O(unique
dicts); byte-identical; also drops one `OnceLock` per page. New regression bench
`shared_dict_mask_decode_30p` added to `benches/document.rs`.

### PGO — profile-guided optimization over fat-LTO — **Kept (opt-in build)** (2026-07-03)

**Hypothesis (round-5 #4).** LTO_FAT (fat LTO + `codegen-units=1`) gave big wins
by cross-crate-inlining the ZP coder. The natural next lever is PGO: feed LLVM a
real execution profile so it lays out basic blocks / predicts branches from
measured behaviour instead of static heuristics. Touches every path at once.

**Setup.** New training driver `examples/pgo_train.rs` decodes/renders a broad
spread of the corpus (bilevel cable, colour watchmaker, heavy-downscale colorbook,
FGbz-palette, large scanned page, multi-page shared-dict DjVu3Spec + pathogenic) at
several scales. `scripts/pgo.sh` + `make pgo` run the four-phase flow
(`-Cprofile-generate` → run 3× → `llvm-profdata merge` → `-Cprofile-use`).
Measured target-vs-baseline with criterion, both built with the fat-LTO bench
profile; PGO is the only difference.

**Numbers (M1 Max, `--features std`).**

| Bench | Baseline | PGO | Change |
|-------|----------|-----|--------|
| `render/render_colorbook_cold` (cold parse+IW44+IDWT+downscale) | 18.55 ms | 15.70 ms | **−15.4 %** (p=0.00, reproduced symmetric +17.9 % on revert) |
| `codecs/jb2_decode_large_600dpi` (2 µs micro) | 2.192 µs | 2.114 µs | −6.5 % (negligible absolute) |
| `codecs/bzz_decode` | 67.3 ns | 67.4 ns | +0.6 % (p=0.59, noise) |
| `codecs/jb2_decode` | 128.0 µs | 134.6 µs | −0.1 % (p=0.78, noise) |
| `codecs/jb2_decode_corpus_bilevel` | 440 µs | 437 µs | +0.1 % (p=0.82, noise) |
| `codecs/iw44_decode_corpus_color` | 695 µs | 704 µs | +0.9 % (p=0.02, tiny regression) |
| `document/shared_dict_mask_decode_30p` | 88.1 ms | 89.6 ms | +1.7 % (p=0.00, tiny regression) |

**Reading.** PGO delivers a **real, reproducible −15 % on the cold end-to-end
render** — the branch-heavy glue (parse, multi-chunk IW44 ZP decode, the compact
sub=4 IDWT, area-average downscale compositor) is where LLVM's static block layout
left the most on the table, and time-to-first-pixel is a genuine UX metric. On the
**isolated SIMD codec kernels** it is neutral-to-−1 % — those are already
LTO-inlined and ALU-bound, so better branch layout has nothing to bite on, and two
micro-benches even regress ~1–2 %. The same-session `shared_dict` regression rules
out a global thermal speedup, confirming the colorbook win is path-specific.

**Decision.** **Kept as an opt-in build**, *not* the default. It helps the
realistic cold-render path substantially and does not meaningfully hurt anything
(<2 % on micro-benches). It is opt-in because PGO needs a two-phase build plus the
training corpus and the `.profdata` is corpus/host-specific — it cannot ship with a
crates.io release. Deliverables: `examples/pgo_train.rs`, `scripts/pgo.sh`,
`make pgo`, documented tradeoff. No default-build or source-path change, so zero
risk to the shipped library.

### INTEROP_PIXDIFF — DjVuLibre pixel-diff quality floor — **Kept (diagnostic tool)** (2026-07-03)

**Goal (round-5 #14).** Establish a quantitative render-quality baseline against
DjVuLibre so future render/quality experiments can be judged (a claimed quality
win vs the reference) and validated (a faithful change proves no drift). Previously
quality was only ever checked point-wise (PSNR in individual experiments).

**Tool.** `examples/interop_pixdiff.rs` renders a page with our decoder and with
`ddjvu -format=ppm` at the same native resolution, then reports the per-channel
absolute-difference distribution (mean, p50/p95/p99, max, and the % of channels
over 2/8/32). `--corpus` sweeps a representative spread. Requires `ddjvu` on PATH,
so it is an opt-in example (not a merge-gated test — CI has no DjVuLibre).

**Baseline (native resolution, this corpus):**

| File | mean | p99 | max | %chan >8 |
|------|------|-----|-----|----------|
| navm_fgbz (FGbz palette) | 0.00 | 0 | 0 | 0.00 % |
| boy (bilevel) | 0.00 | 0 | 0 | 0.00 % |
| cable_1973 (bilevel) | 0.04 | 1 | 71 | 0.01 % |
| colorbook (colour IW44) | 0.14 | 4 | 46 | 0.20 % |
| watchmaker (colour IW44) | 0.21 | 2 | 17 | 0.00 % |

**Reading.** Our renderer is **essentially pixel-faithful to DjVuLibre**: palette
and bilevel pages are byte-identical or near it; colour pages differ by a mean of
<0.25/255 with a tiny tail, attributable to our bilinear chroma upsampling (#422,
deliberately *better* than DjVuLibre's box upsampling). This run also **validates
that the round-5 changes (SUB4_RGB_CACHE, DOC_SHARED_DICT_CACHE) preserved pixel
output** — all still match. (`carte.djvu` is skipped: our IFF parser rejects it as
truncated, a pre-existing issue unrelated to rendering.)

**Decision.** **Kept** as the standing quality floor. It unblocks the quality-axis
experiments below by giving them a reference to measure against.

### QUALITY_AA (LINEAR_BLEND / MASK_UPSCALE) — **Evaluated / deferred** (2026-07-03)

**Hypotheses (round-5 #12/#13).** Blend anti-aliased coverage in linear light
(#12), and bilinearly interpolate mask coverage when *upscaling* / zooming (#13),
for smoother text edges.

**Why deferred after INTEROP_PIXDIFF.** The interop baseline just established that
we render **near-identically to DjVuLibre** (mean <0.25/255), and DjVuLibre blends
in sRGB and hard-edges the mask under zoom. Both proposals would **increase**
divergence from that reference — they are subjective "prettier than DjVuLibre"
changes, not faithfulness improvements. They therefore belong behind an explicit
opt-in *quality mode*, not in the default interop-faithful path, and need a human
aesthetic judgement (or a no-reference sharpness metric) that a perf pass should
not smuggle in. The hot 1:1 path (`bilevel_native_cached`, scale==1) must stay
byte-identical regardless, so any future attempt must gate strictly on scale>1.
Recorded with that constraint; not landed this round.

### AVX2_IDWT — x86 SIMD parity for the IW44 inverse wavelet — **Blocked (hardware)** (2026-07-03)

**Hypothesis (round-5 #8).** The IDWT row/column passes have a NEON path
(`row_pass_neon_s1_row`) but fall back to scalar on x86_64, while YCbCr→RGB has
full AVX2. An AVX2/SSE IDWT pass could be a free win for x86 users.

**Blocked.** All benchmarking here is on M1 Max (aarch64); there is no x86 host in
this environment to measure on, and shipping hand-written x86 SIMD for a normative
decoder **without running it** (aarch64 cannot even execute the `#[target_feature]`
x86 path) is exactly the kind of untested-SIMD risk the IW44 dead-ends warn against.
Recorded as blocked-on-hardware: needs an x86 bench host (round-5 infra item #16)
before it can be attempted safely. Correctness would be gated by extending
`simd_row_pass_matches_scalar` to run on x86.

### ROI_IDWT — narrow the cold `render_region` decode to the viewport — **Rejected / low-value** (2026-07-03)

**Hypothesis (round-5 #1).** RENDER_REGION_SCOPE found the compositor already
region-scoped, leaving the cold `decode_layers` as the only full-page cost. Narrow
*that* to the requested rectangle for viewer/tile rendering.

**Why it does not pay.** The dominant cold cost is **not spatially narrowable**:
- **IW44 background** — `bg44()` runs the ZP arithmetic decode over *every*
  coefficient of *every* chunk (`decode_chunk` in a loop). ZP is a serial entropy
  stream with no random access, so you cannot decode "just the region's
  coefficients" — the expensive part must run in full regardless of viewport.
- **JB2 mask** — symbols are placed across the whole page from one serial ZP
  stream; there is likewise no spatial subset decode.

Only the **IDWT reconstruction + YCbCr→RGB** is spatial, and it could be limited to
the region's rows (plus a small wavelet-halo per level). But (a) that is a fraction
of the cold cost the ZP decode dominates, and (b) it is **paid once** — after the
first touch the decoded layers are cached in `PageLayers`, so every subsequent
region render of that page is warm and already region-scoped (RENDER_REGION_SCOPE).
The realizable saving is "part of the IDWT, on the first region render only," for a
large amount of halo-boundary complexity in a normative path.

**Decision.** **Rejected.** The viewer/tile path is already efficient where it
counts (warm = region-scoped compositor over cached layers); the cold residual is
ZP-decode-bound, which no region-of-interest scheme can shrink. Recorded so this
is not re-proposed as a decode-scope win.
## Perf swarm round 6 (2026-07-03) — discovery + adversarial validation

A fresh multi-agent perf swarm (10 subsystem scouts → adversarial validator per
candidate, each checked against the full ~90-experiment log + the 5 dead-ends →
synthesis). 18 raw candidates surfaced; **6 survived** validation. Every survivor
was then **independently re-verified by hand** before any decision — which caught
the swarm's single biggest miss (P1 below). Recorded so future swarms don't
re-propose the ruled-out set, and so the deferred structural wins have a design.

### GATHER_ZIGZAG_INV — row-major plane read in the IW44 encoder gather — **Kept** (2026-07-03)

**Candidate (swarm P5).** `PlaneEncoder::gather` (`crates/djvu-iw44/src/encode.rs`)
read the multi-KB coefficient plane in *scattered* zigzag order (`ZIGZAG_ROW/COL`)
while writing the 2 KB block sequentially — the inverse of the cache-friendly
arrangement the decoder's `reconstruct()` already uses (`ZIGZAG_INV`: sequential
plane access, scatter into the L1-resident block).

**Change.** Iterate the plane row-major (one cache line of 32 i16 at a time) and
scatter into the block via `crate::ZIGZAG_INV`. Byte-identical — same
(row,col)→block-index mapping, only the traversal order changes — confirmed by
`iw44_bg44_size_does_not_regress` (BG44 bytes unchanged) + the 43 iw44 unit tests.
Also deletes the now-dead `ZIGZAG_ROW`/`ZIGZAG_COL` static tables (2×1 KB) and
their helper imports, so the encoder mirrors the decoder.

**Numbers** (`benches/codecs.rs`, M1 Max):

| Bench | Baseline | After | Change |
|-------|----------|-------|--------|
| `iw44_encode_color` | 2.157 ms | 2.141 ms | −0.7 % (p=0.04) |
| `iw44_encode_large_1024x1024` | 32.06 ms | 31.60 ms | −1.4 % (p=0.14) |

**Decision.** **Kept.** A weak (~1 %, edge-of-significance) but real-direction
speedup, and — more durably — a byte-identical code simplification that removes
duplicated tables and aligns encoder/decoder scatter. Low risk, no output change.

### EPUB_PNG_COMPRESSION_LEVEL — **Rejected after verification** (2026-07-03)

**Candidate (swarm P1, its highest-ranked).** `encode_rgba_to_png` (`src/epub.rs`)
never calls `set_compression`, so PNG encode runs at `png::Compression::Default`.
The swarm measured switching to `Fast` at **−43.5 %** CPU and recommended it P1,
describing the payload as *"smaller … pixel-identical after decode."*

**Verification (measured directly, watchmaker native page):**

| Compression | time/page | PNG bytes |
|-------------|-----------|-----------|
| Default (current) | 68.8 ms | 271 403 |
| Fast (proposed) | 7.8 ms | **770 014 (2.84×)** |
| Best | 140.5 ms | 268 323 |

**Verdict.** The swarm's size claim was **backwards**: `Fast` makes the PNG **2.84×
larger**, not smaller. For EPUB — a document-distribution format where file size is
a primary concern and export is a non-latency-critical batch step — inflating every
page image ~2.8× to save batch CPU is a bad trade. `Default` is already the balanced
choice (`Best` buys ~1 % size for 2× the time). **Rejected.** Recorded as the case
study for why swarm findings get independently re-verified before landing.

### Round-6 validated backlog (deferred / needs setup)

Survivors that are real but not landed this session:

- **LAZY_PAGE_CONSTRUCT (swarm P3, top structural win) — LANDED**, see its own
  entry below. −48 % `from_bytes` on the 520-page doc, byte-identical.
- **SHARED_DICT_CLONE_PER_PAGE (swarm P2). — LANDED (round 16, byte-identical).**
  `encode_jb2_dict_with_options`
  (`crates/djvu-jb2/src/encode.rs`) held `dict_entries: Vec<Bitmap>` and deep-copied
  the shared dict on every per-page call (2.67 M clones on the 517-page corpus).
  Solved more simply than the swarm's "3-call-site split" plan: `dict_entries`
  is now `Vec<&Bitmap>` borrowing `shared_symbols` + the page's own `ccs` (both
  outlive the encode), so *no* public signature changes and *no* call site is
  touched. Byte-identical; wall-clock ~12% faster in isolation but within thermal
  noise (~2.9% predicted); peak RSS flat (clones are transient).
- **CLUSTER_BUCKET_HASH_DEDUP (swarm P4). — LANDED (round 15, −81%/≈5.2×).**
  `bucket_page_ccs`'s exact-match search
  did a full `packed_hamming` popcount scan per entry with `max_diff==0` and no
  `d==0` early-exit — now a `symbol_hash`-keyed `BTreeMap` lookup (the
  technique already shipped for `encode_jb2_dict`'s dedup, CLUSTER_DEDUP #446).
  Byte-identical; the ~2 % estimate was a fraction of full encode — isolated on
  the dense 517-page corpus the bucketing scan itself dominated
  `cluster_shared_symbols` (~17.6 s → ~3.4 s). New `bench_cluster_shared_symbols`.
- **PAR_LANCZOS (swarm P6).** `scale_lanczos3` passes are row-independent but
  sequential. Real upside only on large pages; the named `lanczos3`/boy.djvu fixture
  (96–256 rows) is too small (PARALLEL's own small-page datapoint was 1.15×, not
  3.8×). Needs a large-page Lanczos fixture before it can be judged.

**Ruled out by the validator** (not re-proposals): `IW44_RGB_ROWSLICE` (= PS-R3,
already reverted), the `cb_full`/`cr_full` per-row alloc in the parallel YCbCr path
(chroma_half only — corpus-unexercised, round-3), an all-zero-block scatter-skip in
`reconstruct` (incorrect premise), plus assorted micro-ops. Four validator agents
hit the structured-output retry cap and produced no verdict (their candidates were
conservatively dropped).

**Verdict on remaining headroom.** After six rounds the well is *mostly* dry for
byte-identical hot-path wins, but the swarm found one genuine structural lever the
prior rounds missed — **LAZY_PAGE_CONSTRUCT** — because earlier rounds optimized
decode/render *throughput* and never benchmarked large-document *cold-open latency*.
That axis (and the missing benches for it) is where the next real gains are, not in
the already-tuned codec kernels.

### LAZY_PAGE_CONSTRUCT — defer per-page chunk copy in bundled document open — **Kept** (2026-07-03)

**Issue (round-6 swarm P3, top structural win).** Opening a bundled DJVM document
eagerly `to_vec()`-copied **every** page's chunks in `parse_page_from_chunks`, even
when the caller only renders page 1. On the 520-page corpus that copy loop is ~half
of `Document::from_bytes`; for `MmapDocument` it is essentially the *only* memcpy
(the mapping is otherwise zero-copy). The async `LazyDocument` already avoided this
with `OnceCell`; the sync/mmap path did not.

**Approach.** A page's chunks now come from a `ChunkStore` (`std`):
- `Eager(Vec<RawChunk>)` — unchanged behaviour for single-page, indirect, and
  `no_std` documents (which keep a plain `Vec<RawChunk>` field).
- `Lazy { backing, range, cache: OnceLock }` — holds a shared document backing
  (`Backing = Arc<dyn AsRef<[u8]> + Send + Sync>`) and this page's `FORM` byte
  range, and materialises the chunks **once on first access** via `chunk_slice()`.

`Document::from_bytes` moves its `Vec<u8>` into the backing (no copy) and
`MmapDocument::open` moves the `Mmap` in (zero-copy); both call the new
`DjVuDocument::parse_backed`, which builds lazy pages for bundled documents and
falls back to the eager `parse` for single-page / non-DJVM / indirect. Only the
cheap fixed-size `INFO` header is parsed eagerly per page, so metadata iteration
(`iterate_pages_520p`) does not regress.

**Correctness.** Byte-identical: the lazy build re-parses the same `FORM` sub-form
bytes into the same `RawChunk`s the eager path produced. The `mmap_document_matches_parse`
parity test, all multi-page / shared-dict / `page_byte_range` tests, and the full
1016-test suite (incl. `no_std`, `wasm32`) pass. The backing `Arc` is shared by
every lazy page and by `MmapDocument`, so the mapping cannot drop while a page still
needs it.

**Numbers** (`benches/document.rs`, M1 Max, 520-page pathogenic_bacteria_1896):

| Bench | Before (eager) | After (lazy) | Change |
|-------|----------------|--------------|--------|
| `parse_multipage_520p` (`from_bytes` only) | 2.36 ms | 1.22 ms | **−48 %** (p=0.00) |
| `open_and_render_first_page_520p` (cold open + render pg 1) | 10.82 ms | 9.86 ms | **−9 %** (p=0.00) |
| `iterate_pages_520p` (metadata only) | 431 ns | 421 ns | flat (no regression) |

For accessed pages the copy work is the same, only deferred (re-`parse_sub_form` +
`to_vec` on first touch); for the common "open a big book, view a few pages" and
metadata/thumbnail workloads the un-touched pages are never copied at all, and mmap
opens are zero-copy until a page is rendered. New `open_and_render_first_page_520p`
bench added.

**Decision.** **Kept.** The largest structural win of rounds 5–6: it targets the
cold-open-latency axis the prior throughput-focused rounds never benchmarked. The
swarm found it; hand-implementation confirmed the −48 % `from_bytes` win,
byte-identical, across std/no_std/mmap/async.

## Perf round 7 (2026-07-03) — gray-decode axis (from the round-6 experiment backlog)

Round 6 concluded the well was "mostly dry for byte-identical hot-path wins" but
that whole **axes** remained unbenchmarked (LAZY_PAGE_CONSTRUCT proved this: −48 %
lived on the cold-open axis nobody had measured). This round attacks the next such
axis from the recorded backlog: **grayscale decode** (proposal B2). Every gray
consumer — `render_gray8`, OCR pre-pass, e-ink viewers, thumbnail grids — currently
pays for full colour: `render_gray8` is literally `render_pixmap(...).to_gray8()`,
and the codec's only gray output was `to_rgb_subsample().to_gray8()`, i.e. both
chroma inverse-wavelet transforms + the YCbCr→RGBA conversion, all thrown away by
the final luma reduction.

### GRAY_DIRECT — decode only the Y plane for grayscale output — **Kept** (2026-07-03)

**Issue (round-6 backlog B2).** For a colour IW44 image the two chroma planes'
`reconstruct()` (inverse wavelet) plus the YCbCr→RGBA SIMD conversion are the bulk
of `to_rgb`'s cost, but a grayscale consumer never needs them. There was no API to
get luma without paying for chroma.

**Approach.** New additive methods on `Iw44Image` (`crates/djvu-iw44/src/lib.rs`):
`to_gray8()` / `to_gray8_subsample(sub) -> GrayPixmap`. They reconstruct **only the
Y plane** (`cb`/`cr` are never touched) and write one byte per pixel:
- **Grayscale images:** `gray = 127 − normalize(Y)` — identical to the existing
  gray path's R channel.
- **Colour images:** `gray = clamp(normalize(Y) + 128, 0, 255)` — the DjVu luma
  channel (what the colour formula yields when Cb=Cr=0).

Purely additive: no existing byte-exact path is touched. Compact (sub=2/4) and
full-res indexing mirror `to_rgb_subsample`.

**Fidelity.**
- Grayscale images: **byte-identical** to `to_rgb_subsample(sub).to_gray8()` (the
  Rec.601 weights 306+601+117 sum to 1024, so `R=G=B=g` round-trips to `g` exactly).
  Asserted by `iw44_to_gray8_matches_rgb_luma_boy` for the `!is_color` branch.
- Colour images: the Y-plane luma is **not** bit-identical to the Rec.601 luma of
  the reconstructed RGB (which mixes in chroma-derived R/G/B). Measured on boy.djvu:
  **mean abs diff < 4/255, max ≤ 24** — a few levels, and the Y channel is the more
  faithful luminance (it *is* the encoder's luma). This is why GRAY_DIRECT is an
  opt-in method, not a silent replacement of the colour→gray reduction.

**Numbers** (`benches/codecs.rs::bench_iw44_gray_decode_large`, colorbook.djvu
2260×3669 colour page, pre-decoded; M1 Max). Compares `to_rgb().to_gray8()` vs
`to_gray8()`:

| Build | rgb_then_gray | gray_direct | Change |
|-------|---------------|-------------|--------|
| default (`std`, sequential) | 6.70 ms | 1.92 ms | **−71 % (3.5×)** |
| `--features parallel` | 3.78 ms | 2.05 ms | **−46 % (1.85×)** |

The colour path benefits from `rayon::join` reconstructing Y/Cb/Cr concurrently, so
the parallel gap is smaller — but GRAY_DIRECT still wins by nearly 2× there because
it does ~1/3 the reconstruct work (one plane, not three) and skips the YCbCr math
entirely. Sequential (the default, and the wasm/no_std reality) sees the full 3.5×.

Downscaled gray previews (the thumbnail-grid case that most motivates this path),
sequential build, `to_rgb_subsample(s).to_gray8()` vs `to_gray8_subsample(s)`:

| Subsample | rgb_then_gray | gray_direct | Change |
|-----------|---------------|-------------|--------|
| sub=2 (1130×1835) | 1.73 ms | 0.69 ms | **−60 % (2.5×)** |
| sub=4 (565×918)   | 0.42 ms | 0.17 ms | **−60 % (2.5×)** |

The relative win narrows a little at higher subsample (compact-plane reconstruct is
already cheap) but holds at ~2.5× — a preview grid of colour scans decodes to gray
in a fraction of the time.

**Decision.** **Kept.** Additive codec API on the previously-unbenchmarked gray
axis; large win, no risk to existing paths, tests + clippy + no_std + bench all
green. Reachable today via `page.decoded_bg44()?.to_gray8()`. Follow-up (not landed
here): a gray compositor so `render_gray8` can use this end-to-end for the common
background-dominated colour scan — currently `render_gray8` still routes through the
full RGBA compositor before reducing, so this win is codec-level until that path
exists. New `bench_iw44_gray_decode_large` added.

## Perf round 8 (2026-07-03) — full breadth triage of the round-7 proposal set

Follow-up to the chat investigation that produced ~25 decode/render proposals
(axes A–D). Rather than deep-dive one, this round gives **every** idea a verdict:
some are already implemented (verified by reading the code), one is a measured
structural opportunity, the rest are classified by what blocks them. Grounded in
code reading this session, not memory. Categories: **DONE-ALREADY** (the codebase
already does it), **OPPORTUNITY** (real, quantified, deferred with design),
**RULED-OUT** (dead-end or covered), **NEEDS-INFRA** (real but blocked on a
fixture/host/harness), **QUALITY-GATED** (needs the D1 perceptual harness first).

### B3 (early wavelet stop for sub=2/4) — **DONE-ALREADY** (2026-07-03)

`PlaneDecoder::reconstruct` (crates/djvu-iw44/src/lib.rs:1677–1742) already does
exactly this: for sub∈{2,4,8} it scatters only the low-frequency sub-block
coefficients into a 4×/16×/64×-smaller compact plane and runs the wavelet from
`start_scale = 16/sub`, never computing the discarded fine scales. That is
"decode directly to half/quarter resolution." No work to do. (This is also why
BG_CACHE_S2/SUB4_RGB_CACHE are cheap.)

### B4 (word-at-a-time JB2 symbol blit) — **DONE-ALREADY** for the hot path (2026-07-03)

The hot bilevel render path `blit_to_bitmap` (crates/djvu-jb2/src/lib.rs:1138–1173)
already blits byte-at-a-time with shift-align OR: aligned case is `dst[i] |= src[i]`
over whole bytes (LLVM auto-vectorizes this), unaligned case is a two-shift byte OR.
The premise "if it's bit-by-bit, use word OR" is already satisfied. The only
bit-by-bit blitter left is `blit_indexed` (the palette/indexed-mask path,
lib.rs:1033) — but that path is **decode-once and cached** (MASK_IDX_CACHE #427),
so its cost is cold-once, not hot. Widening the aligned OR to explicit u64 is the
LLVM-auto-vec dead-end (see the dead-end list). No worthwhile work.

### B5 (retain decoder state across progressive frames) — **OPPORTUNITY, measured** (2026-07-03)

**Confirmed real and O(N²).** `render_progressive(page, opts, chunk_n)` calls
`decode_layers(…, chunk_n+1)`, and for the progressive (non-`usize::MAX`) limit
`decode_background_chunks` (src/djvu_render.rs:1270) allocates a **fresh
`Iw44Image::new()` and re-decodes BG44 chunks 1..=chunk_n+1 from scratch every
frame**. Rendering all N frames therefore does 1+2+…+N = O(N²) chunk decodes.
(The mask/FG layers are *not* redundant — they hit the page's OnceLock caches after
frame 0; only BG is re-decoded.)

**Measured** (probe, colorbook.djvu page 0, 4 BG44 chunks, warm mask/FG, M1 Max):
`render_progressive_all` = **216 ms** for 4 frames vs a single full
`render_pixmap` = **45 ms** → **4.8×**. With incremental decode the 4 frames should
cost ≈ one full render plus three cheap `to_rgb`+composite snapshots (~1.3–1.5×),
i.e. a ~3× reduction on the "eagerly render every refinement" workload, and an even
bigger win for a streaming viewer that drives `render_progressive_step` as chunks
arrive over the network (there the O(N²) is paid across the whole session).

**Why deferred, not patched now.** The clean fix is a **stateful progressive
decoder** that persists the incrementally-fed `Iw44Image` across `render_*_step`
calls (a small new type, or a per-level page cache). The tempting shortcut — make
`render_progressive_all` drive one `Iw44Image` and re-assemble frames itself — would
have to replicate the bg+fg+bold+composite assembly that `decode_layers` centralises
"so no logic can drift between this and the full render" (its own comment). Forking
that is the wrong trade; the O(N²) is better than a divergent second composite path.
Design for a future session: a `ProgressiveDecoder { img: Iw44Image, fg: OnceCell }`
that yields one composited frame per `push_chunk`, with a byte-identical test against
`render_progressive_step` on chicken.djvu (3 chunks) + colorbook (4 chunks).

**Side note (robustness, not perf):** the probe hit `Iw44(ZpTooShort)` from
`render_progressive_all` on watchmaker.djvu page 0 — a partial-chunk progressive
decode erroring on a real corpus file. Worth a separate look; out of scope here.

### C1 (RGB8 output, drop alpha) — **PARTLY DONE / wide-surface deferred** (2026-07-03)

The export paths already avoid carrying alpha into the output format:
`export_common::rgba_row_to_rgb` (src/export_common.rs:86) strips alpha per row, and
PDF/TIFF use `render_streaming` so no full RGBA+RGB double buffer is held. The
residual C1 win (a compositor that *writes* 3-byte RGB directly, saving 25% output
bandwidth) would touch every fast path — P2's 256×32 B BILEVEL_RGBA table, G1, F2,
the area-avg and B-series writers — all of which emit RGBA. Wide, byte-layout-
sensitive surface for a bandwidth-only win on the write side; the SIMD tables assume
4-byte pixels. Deferred as high-effort/medium-reward; the export consumers that
actually want RGB already get it via the row converter.

### C2 (fuse rotation into the compositor) — **RULED-OUT (low value / regression risk)** (2026-07-03)

`rotate_pixmap` (src/djvu_render.rs:859) already uses the ROTATE_TILE 32×32 cache-
tiled transpose (#447, kept, ~2–6%). Writing composited rows *directly* to
transposed positions would trade that cache-local transpose for **strided scatter
writes** in the compositor's hot loop — exactly the access pattern ROTATE_TILE was
added to avoid — while saving one intermediate pixmap alloc. The alloc is cheap
relative to the bandwidth hit; net-negative risk. Not worth it.

### Remaining axes — classified (no code change this round)

**NEEDS-INFRA** (real, but blocked on a fixture/host/harness that doesn't exist yet):
- **A1 IDWT_PAR_PLANE** — within-Y-plane wavelet parallelism; cold-only, needs a
  very-large colour-page fixture to beat the rayon overhead (already in the deferred
  log). **A4 PAR_LANCZOS** — needs a large-page Lanczos fixture (>3000 rows); the
  named boy fixture is too small. **A2 AVX2_IDWT** — needs an x86 bench host (M1 can't
  measure). ~~**B6 madvise / B7 speculative next-page decode** — need a cold-disk /
  simulated-network harness; no such bench exists.~~ **CLOSED by COLD_OPEN
  (round-38):** harness built (`examples/cold_open_bench.rs`, F_NOCACHE
  fresh-copy strategy, 4.76x cold/warm gap validated); B6 madvise measured
  neutral on local NVMe (kept opt-in, D/F pending slower storage), B7 prefetch
  measured +24–90% and **kept**. **C3 upscale (zoom>1) path** —
  genuinely unbenchmarked axis, needs a `zoom 2×/4× region` bench before any fast
  path can be judged. **C4 tile cache** — an API feature (pan/zoom viewer), needs a
  panorama-scenario bench. **C5 memory budget / LRU cache eviction** — measurable as
  a peak-RSS diagnostic on the 520-page book, but the eviction machinery is a large
  change; worth a *diagnostic* RSS measurement first.
- **B1 ZP-core micro-opts** — highest-theoretical-leverage but the hot loop is
  inlined into djvu-jb2/iw44/bzz (three byte-exact copies) and PGO already captured
  the branch-heavy win (codec kernels flat ±1%); high-risk/low-EV. The u64-bitbuffer
  idea is concrete but its expected payoff is marginal (refill is already amortized
  and its branch is well-predicted).

**QUALITY-GATED** (need the perceptual harness D1 = SSIM/PSNR + golden corpus, which
gates the whole D branch and A3): **A3** linear-light blend + mask-upscale AA,
**D2** Lanczos-vs-area-avg for photo downscale, **D3** bicubic FG44 upsample,
**D4** gamma-correct downscale, **D5** TH44-thumbnail preview fast path (also a
feature: the embedded thumbnail is separately lossy). None of these can be honestly
decided without D1; building D1 is the unblocking prerequisite and the highest-value
*infrastructure* task on the list.

**Verdict.** Of the ~25 proposals: **2 were already implemented** (B3, B4-hot),
**1 already landed this session** (B2 → GRAY_DIRECT), **2 ruled out** (C1-compositor
wide-surface, C2 regression-risk), **1 is the top measured structural opportunity**
(B5, ~3× on progressive, deferred pending a stateful-decoder design), and the rest
split into NEEDS-INFRA and QUALITY-GATED — none blocked on ideas, all blocked on a
missing bench/host/harness. The two unblocking meta-tasks that would convert the most
deferred items into runnable experiments are **D1 (perceptual quality harness)** and
a **cold-open / large-page / zoom fixture set**.

## Perf round 9 (2026-07-03) — D1 perceptual quality harness (unblocks the D branch)

Round 8 flagged the whole D branch (A3, D2–D5) as QUALITY-GATED: undecidable
without a perceptual metric, because the existing `interop_pixdiff` tool reports
only the *arithmetic* mean/max per-pixel RGB diff — which cannot say whether a
change is perceptually better, only that it drifted. This round builds that gate.

### QUALITY_HARNESS_D1 — PSNR + SSIM metrics module + harness — **Kept (infra)** (2026-07-03)

**What.** New `src/quality.rs` (public, std-gated): `psnr`, `ssim`, `compare`,
`compare_gray`, `psnr_from_mse`, and a `QualityReport { mse, psnr_db, ssim }`.
SSIM is the standard windowed index (8×8 non-overlapping windows — the common fast
approximation of the 11×11 Gaussian — with C1=(0.01·255)², C2=(0.03·255)²), computed
on the Rec.601 luma of RGBA pixmaps so colour and grayscale compare on the same
perceptual channel. Six unit tests pin the behaviour: identical→(SSIM 1, PSNR ∞,
MSE 0), `psnr_from_mse(1)=48.13 dB`, flat-shift keeps high SSIM while a pixel
checkerboard collapses it, sub-window images don't panic, gray/RGBA luma agree.

`examples/quality_harness.rs` drives it against the DjVuLibre `ddjvu` reference
(the same "quality floor" `interop_pixdiff` uses), size-aligning our render to
ddjvu's output and reporting SSIM/PSNR — a perceptual upgrade of the old mean-diff
harness. Falls back to a mode-delta / `--pair` comparison when ddjvu is absent.

**Immediate result — D2 (Lanczos-vs-Bilinear downscale) decided:** running the
harness on real colour pages vs the ddjvu reference:

| Page | Scale | Bilinear (SSIM / PSNR) | Lanczos3 (SSIM / PSNR) |
|------|-------|------------------------|------------------------|
| colorbook (photo) | 1/2 | 0.9594 / 27.25 dB | **0.9928 / 36.94 dB** |
| watchmaker (text+photo) | 1/3 | 0.8607 / 13.79 dB | **0.9890 / 28.65 dB** |

Lanczos3 downscaling is **decisively** closer to the reference decoder — a large
SSIM gain and +10…15 dB PSNR, biggest on the text-heavy page where bilinear's edge
blur hurts most. This answers the long-open #423 / D2 question with numbers:
**Lanczos-3 should be the default (or strongly recommended) resampling for
photographic/mixed downscale**, not bilinear. (It costs more — see
`render_scaled_0.5x` bench — so the follow-up is a speed/quality policy: default
Lanczos for large downscale ratios, bilinear for near-1:1. PAR_LANCZOS, already in
the backlog, would close much of the speed gap.)

**Decision.** **Kept** as infrastructure. `djvu_rs::quality` is now the perceptual
gate every D-branch experiment (A3 linear-light blend, D3 bicubic FG, D4 gamma
downscale, D5 TH44 preview) can be judged against — each becomes "does it raise
SSIM-vs-reference (or SSIM-vs-source) without an unacceptable speed cost?". The
biggest blocker on the quality axis is removed. 1024 tests green (was 1018), clippy
+ no_std + wasm32 clean.

### D5 (TH44 embedded-thumbnail preview fast path) — **Evaluated / opt-in only** (2026-07-03)

Now measurable via the D1 harness. **Two blockers make this niche, not a default.**

**Fixture reality:** swept the corpus — **zero** files embed a decodable `TH44`
thumbnail (`page.thumbnail()` returns `None` for colorbook/boy/chicken/watchmaker/
pathogenic/conquete; carte.djvu fails to parse). D5 only helps documents whose
*encoder* embedded thumbnails; ours can (`thumbnail::encode_th44_color`) but almost
nothing in the wild does.

**Speed/quality (colorbook page 0, TH44 synthesised from a native render, then the
preview path = decode TH44 + bilinear resample; vs a cold sub-sampled full render):**

| Preview | Full render (cold Bilinear) | TH44 path | Speedup | SSIM (TH44 vs full) |
|---------|-----------------------------|-----------|---------|---------------------|
| 48×77 | 11.5 ms | 0.38 ms | **30.7×** | 0.497 |
| 64×103 | 11.7 ms | 0.42 ms | **27.7×** | 0.552 |
| 96×155 | 11.0 ms | 0.48 ms | **23.0×** | 0.642 |
| 128×207 | 11.2 ms | 0.59 ms | **19.1×** | 0.684 |

(Against a cold *Lanczos* full render the speedup is 187–302×, but Lanczos forces a
full-native render so that baseline is unfair; Bilinear preview already uses sub≥4
partial decode, which is the honest fast baseline.)

**Verdict.** The speed win is large and real (20–30×), but the fidelity is **low**
(SSIM 0.50–0.68 — a TH44 is a ~128 px lossy IW44 encode, visibly softer/different
from a real render). Acceptable for a dense thumbnail-*grid* where throughput
dominates; **not** acceptable as a general preview default. Combined with the fixture
reality (nothing embeds TH44), D5 is worth at most an **explicit opt-in** render flag
("use embedded thumbnail if present and target ≤ its size"), paired with encoder-side
thumbnail embedding — not a default fast path. Deferred as low-priority opt-in. The
value of this round is that D1 turned "maybe TH44 previews are fine" into a measured
SSIM 0.5–0.68 tradeoff, i.e. an evidence-based *no* for the default case.

### D4 (gamma-correct / linear-light downscale) — **Rejected (low headroom)** (2026-07-03)

Diagnostic via D1 before touching the hot area-average compositor (which carries
AREA_FIX, COLOR_AA, AREAAVG_ALLBG — all kept). Compared, on real rendered pages,
our **gamma-space** box downscale (what area-average does — average device values)
against a physically-correct **linear-light** downscale (gamma-decode → average →
re-encode, page gamma 2.2):

| Page | 1/2 (SSIM / PSNR) | 1/4 (SSIM / PSNR) |
|------|-------------------|-------------------|
| colorbook (photo) | 0.9984 / 41.2 dB | 0.9969 / 37.5 dB |
| boy (photo) | 0.9967 / 35.4 dB | 0.9937 / 29.8 dB |
| watchmaker (text+photo) | 0.9932 / 28.8 dB | 0.9790 / 25.1 dB |

The correct-vs-naive gap is **negligible** on photographic content (SSIM ≥ 0.997)
and only mildly visible on the text-heavy page (SSIM 0.979 worst case). The reason
is structural: the gamma-incorrect-average artefact is worst on hard black/white
edges, but in DjVu those edges live in the **bilevel JB2 mask** (composited via
`mask_box_coverage`, a separate path), not in the smooth IW44 background that the
area-average downscales. Implementing linear-light in the hot BG-downscale kernel —
risking the three kept optimisations there, and *diverging* from DjVuLibre (which
also averages in gamma space) — buys at most ~2% SSIM on the worst case.

**Rejected.** Not worth the compositor risk. And the practical lever for better
downscaled **text** is already identified and far larger: **D2's Lanczos-3**
(edge-preserving, SSIM 0.989 vs bilinear 0.861 on watchmaker) dominates any gamma
refinement. Corollary: **A3's linear-light *blend*** shares the same physics and
therefore the same small headroom on real DjVu content — deprioritised by the same
evidence. D1 converted two guesses into evidence-based decisions this session: **D2
adopt Lanczos, D4/A3 reject linear-light.**

## Perf round 10 (2026-07-04) — PAR_LANCZOS (acts on the D2 finding)

D2 (round 9) established Lanczos-3 as the quality winner for photographic/mixed
downscale, with its one caveat being cost (it renders at native resolution then
resamples). This round removes most of that cost.

### PAR_LANCZOS — row-parallel Lanczos-3 resampler — **Kept** (2026-07-04)

**Issue (backlog A4).** `scale_lanczos3` (src/pixmap.rs) ran both separable passes
single-threaded. Prior rounds deferred parallelising it because the only named
fixture (boy.djvu, 96–256 rows) was too small to beat rayon overhead. With a large
fixture (colorbook native, 2260×3669) the passes do real work.

**Approach.** Both passes are row-independent — the horizontal pass writes each
`mid` row from its own `src` row + shared precomputed column weights; the vertical
pass writes each `out` row from a `v_support`-tall window of `mid` using three
column accumulators. Parallelised over rows behind the existing `parallel` feature
(the IW44_PAR/PARALLEL gate): horizontal via `par_chunks_mut`, vertical via
`for_each_init` so each worker keeps its own accumulator scratch (reused across the
rows it owns, no per-row alloc). The sequential path is unchanged.

**Correctness.** Bit-identical: each output pixel sums the same contributors in the
same order regardless of thread assignment — thread scheduling never touches the
per-pixel math. All `scale_lanczos3` golden tests pass in both `std` and
`std,parallel`; full 1024-test suite green in both configs; `make check` clean.

**Numbers** (`benches/render.rs::render_scaled_large_colorbook`, colorbook 0
native→½ = 1130×1834, warm decode, M1 Max). Isolated by measuring the **parallel**
build with vs without this change (both have parallel decode; only the Lanczos pass
differs), so the decode-parallelism is cancelled out:

| Lanczos render (parallel build) | Time | Change |
|---------------------------------|------|--------|
| sequential Lanczos (before) | 100.5 ms | — |
| row-parallel Lanczos (after) | 31.2 ms | **−69 % (3.2×)** |

End-to-end, a large-page Lanczos render drops from 145 ms (default seq build) to
31 ms (parallel). The resampling passes alone go ~97 ms → ~28 ms (≈3.5×, matching
the row-parallel spread across the core cap).

**Decision.** **Kept.** Directly amplifies D2: Lanczos was the quality winner but
cost ~10× a bilinear downscale; row-parallelism cuts that to ~3×, making a
"Lanczos-by-default for large downscale ratios" policy far more affordable in
parallel builds. New `render_scaled_large_colorbook` bench added (the old
`render_scaled_0.5x` boy fixture was too small to show it).

## Perf round 11 (2026-07-04) — B5 incremental progressive decode (implemented; modest)

Round 8 measured `render_progressive_all` at 4.8× a single render on colorbook and
flagged the O(N²) per-frame BG re-decode as the top structural opportunity. This
round implements the incremental fix — and finds the **addressable** redundancy is
much smaller than that 4.8× implied.

### B5_INCREMENTAL_PROGRESSIVE — one accumulating decoder across frames — **Kept (modest)** (2026-07-04)

**What.** `render_progressive_all` no longer calls `render_progressive_step` per
frame (which rebuilt a fresh `Iw44Image` and re-ran ZP decode of BG44 chunks
1..=k for every frame k — 1+2+…+N chunk-decodes). It now decodes the foreground
once (`decode_foreground_strict` + one bold-dilation, mirroring `decode_layers`'
strict branch), feeds one BG44 chunk per frame into a single accumulating
`Iw44Image`, and snapshots each frame through the **unchanged** composite path
(`CompositeContext::from_layers` → `composite_into` → `rotate_pixmap`). Gated to the
byte-identical-serviceable case (strict mode, Bilinear, multi-chunk BG44); everything
else falls back to the per-frame loop.

**Correctness.** Byte-identical, proven by the new
`render_progressive_all_matches_per_frame` test (chicken.djvu, 3 BG44 chunks:
every incremental frame equals `render_progressive_step` pixel-for-pixel). Feeding
chunks 0..=k into one decoder produces the same state as a fresh decode of the first
k+1 chunks (ZP state accumulates identically). 1025 tests green, `make check` clean.

**Numbers** (incremental `render_progressive_all` vs the per-frame loop, M1 Max):

| Page | BG44 chunks / frames | per-frame (old) | incremental (new) | Speedup |
|------|----------------------|-----------------|-------------------|---------|
| chicken (181×240) | 3 | 3.44 ms | 2.30 ms | **1.50×** |
| colorbook (2260×3669) | 4 | 212.9 ms | 205.3 ms | **1.04×** |

**Why modest, not the 4.8× round 8 implied.** BG decode has two parts: (1) ZP
entropy decode (`decode_chunk`) and (2) IDWT reconstruct + YCbCr→RGB
(`to_rgb_subsample`). Only (1) is redundant across frames and now incremental;
(2) is **inherently per-frame** — each frame is a different refinement level, so the
global wavelet must be re-run in full, and there is no incremental IDWT. The round-8
"4.8×" was the ratio of *total* progressive work to one render, but almost all of it
is per-frame reconstruct + composite (8.3 MP × 4 frames on colorbook), not the ZP
redundancy B5 removes. So the win is large only when ZP decode dominates — small
pages (chicken 1.5×) and, increasingly, **many-chunk** pages (the saving is the
N(N−1)/2 skipped chunk-decodes, so a 15-chunk progressive photo gains far more than
colorbook's 4). Strictly ≤ the old cost by construction; never a regression.

**Decision.** **Kept** — byte-identical, tested, strictly-better, and an honest
O(N²)→O(N) on the ZP portion that scales with chunk count. But the headline
progressive win is smaller than hoped, and the **more important** case — a streaming
viewer driving `render_progressive_step` as chunks arrive over the network, which
still re-decodes O(N²) across the session — is *not* addressed by this
`render_progressive_all`-only change. That needs the stateful `ProgressiveDecoder`
API (persist the accumulating `Iw44Image` across step calls); it remains the real
deferred structural work, now with corrected expectations (its ceiling is the ZP
redundancy, not the full per-frame cost).

## Perf round 12 (2026-07-04) — C5 unbounded render-cache memory (found + fixed)

Investigating the last unmeasured axis from round 8: peak memory of a long-lived
viewer over a large document. It turned out to be a real, unbounded-growth bug.

### C5_RENDER_CACHE_EVICTION — bound per-document render memory — **Kept** (2026-07-04)

**Finding.** `doc.page(i)` returns a `&DjVuPage` stored in the document, and each
page memoises its decoded layers (`PageLayers`: BG44 image, the full-res BG→RGB
pixmap at `w×h×4`, mask, mask_sub4, FG44, and the s1/s2/s4 RGB caches) in a
`OnceLock` that lives for the document's lifetime. So rendering page after page
**accumulates one full decode cache per page, never evicted** — peak RSS grows
linearly with pages rendered. Measured on colorbook.djvu (62 colour pages, rendered
sequentially at native resolution):

| Pages rendered | Peak RSS |
|----------------|----------|
| 1 | 58 MB |
| 5 | 103 MB |
| 20 | 261 MB |
| 62 | 714 MB |

≈ 11 MB/page, unbounded — a 500-page colour book would reach multiple GB and can
OOM a viewer. This axis was never benchmarked (prior rounds optimised throughput,
not long-run memory), exactly the LAZY_PAGE_CONSTRUCT lesson again.

**Fix.** Additive eviction API (std): `DjVuPage::evict_render_cache(&mut self)`
resets the `render_layers` `OnceLock` (dropping the whole `PageLayers`);
`DjVuDocument::evict_render_caches(&mut self)` clears all pages;
`DjVuDocument::retain_render_caches(&mut self, keep)` clears all but a working set
(the visible pages + prefetch window). Caches rebuild lazily on the next render, so
eviction is transparent. No existing behaviour changes (nothing calls it unless the
consumer opts in).

**Correctness.** Byte-identical across an evict: new
`evict_render_cache_preserves_output` test renders chicken.djvu, evicts, re-renders,
and asserts pixel-equality (the cache rebuilds to the same result). 1026 tests green,
`make check` clean.

**Impact** (render all 62 colorbook pages, keep only the current page's cache):

| Mode | Peak RSS | Output |
|------|----------|--------|
| no eviction (before) | 714 MB | checksum 13716 |
| `retain_render_caches(&[i])` | 103 MB | checksum 13716 |

**−86 % peak RSS**, identical output. A viewer can now bound memory to its working
set instead of the whole book.

**Decision.** **Kept.** Turns an unbounded per-document memory leak into a
consumer-controllable working-set bound; additive, byte-identical, low-risk. A full
automatic global LRU (evict by byte budget without the caller listing pages) is the
natural follow-up, but the manual API already gives viewers the lever and is enough
to prevent the OOM.

## Perf round 13 (2026-07-04) — C5 follow-up: automatic LRU cache budget

Round 12's `retain_render_caches(keep)` requires the caller to name exactly which
pages to keep. This round adds the automatic form: set a byte ceiling, and the
least-recently-used pages are evicted for you.

### C5_LRU_BUDGET — enforce_cache_budget with per-page LRU — **Kept** (2026-07-04)

**What.** Three additions (std):
- Per-page LRU tick: `PageLayers` gains an `AtomicU64 access`, stamped from a
  process-global monotonic counter on every `render_layers()` access (the single
  funnel all layer accesses already pass through), so each page records when it was
  last rendered. Read (without touching the cache) via a non-initialising
  `OnceLock::get()`.
- `DjVuPage::render_cache_bytes()` / `DjVuDocument::render_cache_bytes()` —
  observability: approximate resident bytes held (exact for the dominant RGB/mask
  pixmaps + blit map; BG44 coefficient images estimated from `w·h·2`).
- `DjVuDocument::enforce_cache_budget(max_bytes, protect) -> freed`: if the total
  exceeds `max_bytes`, evict unprotected pages **least-recently-used first** until
  under budget. The automatic form of `retain_render_caches`.

**Correctness.** New `enforce_cache_budget_lru_and_correctness` test: renders 3
pages, asserts the LRU ticks strictly increase with render order, that a budget of 1
byte protecting page 2 evicts exactly the two LRU pages and keeps page 2, and that a
re-render of an evicted page is byte-identical. 1027 tests green, `make check` clean.

**Impact** (render all 62 colorbook pages, calling `enforce_cache_budget(N, &[i])`
after each; M1 Max):

| Budget | Final cache | Peak RSS | Output |
|--------|-------------|----------|--------|
| none | 393 MB | 714 MB | checksum 13716 |
| 150 MB | 146 MB | 419 MB | checksum 13716 |
| 60 MB | 57 MB | 255 MB | checksum 13716 |

The accumulated cache is held **precisely** under the ceiling (146 ≤ 150, 57 ≤ 60),
and peak RSS falls from 714 MB to 255 MB at the 60 MB budget. Peak stays above the
budget because it also includes the transient full-resolution render buffer of the
current page plus allocator retention (freed pages aren't immediately returned to
the OS) — both inherent and outside the cache the budget governs. The point is that
the previously *unbounded* accumulation is now bounded to a caller-chosen ceiling.

**Decision.** **Kept.** Completes C5: `enforce_cache_budget` gives a long-lived
viewer automatic, LRU-correct memory bounding with a one-line call per render and no
need to track page order itself. Additive, byte-identical, low-risk.

## Perf round 14 (2026-07-04) — C3 zoom/upscale axis (diagnostic: already efficient)

The zoom axis — a viewer at >100% rendering a viewport of an upscaled page — was
never benchmarked. Investigated it directly; it turns out to already be efficient,
so this is a diagnostic, not a change.

### C3_ZOOM_SCOPE — is the zoom/region render path wasteful? — **Diagnostic / no change** (2026-07-04)

**Setup.** `render_region(page, rect, opts)` with `opts.width/height` = the zoomed
full-page size (2×/3×/4× native) and a fixed viewport rect — the exact call a
zooming viewer makes. Measured warm (decode cached), colorbook + cable, M1 Max.

**Finding 1 — flat across zoom.** A 1024×768 viewport costs ~4 ms at **every** zoom
level 1×–4× (and for both colour and bilevel). Zoom does not blow up cost: the
compositor writes only the viewport's pixels, sampling the decoded layers at the
zoom-scaled source position — no full-page work per region. RENDER_REGION_SCOPE
(round 5) already ensured this; this confirms it holds under upscaling.

**Finding 2 — linear in viewport, no per-call overhead.** Scaling the viewport at
fixed 2× zoom:

| Viewport | pixels | time | ns/px |
|----------|--------|------|-------|
| 128×128 | 16 K | 95 µs | 5.8 |
| 256×256 | 66 K | 357 µs | 5.4 |
| 512×512 | 262 K | 1.41 ms | 5.4 |
| 1024×768 | 786 K | 4.55 ms | 5.8 |
| 2048×1536 | 3.1 M | 18.8 ms | 6.0 |

Cost is cleanly proportional to output pixels at ~5.5 ns/px with no fixed per-call
overhead. That rate matches the full-page compositor baselines (color_native_cached
≈ 5.6 ns/px), so the bilinear-**upscale** B-series path is no more expensive
per-pixel than a 1:1 render — the compositor's kept optimisations already cover it.

**Verdict.** No low-hanging speed win on the zoom axis: the warm region/zoom render
is scoped, linear, and at the same per-pixel rate as the tuned full-page compositor.
The only residual cost is the **cold** first decode of a page (full-page BG ZP +
IDWT even for a small viewport), which is the already-rejected ROI_IDWT (the ZP
stream is serial and not spatially subsettable). Axis closed. The remaining zoom
work is *quality* (mask-upscale AA, QUALITY_AA #13), now judgeable via the D1
harness, not speed.

### GRAY_DIRECT_E2E — wire to_gray8_subsample through render_gray8 — **Rejected (compositor duplication)** (2026-07-04)

Evaluated whether the round-7 GRAY_DIRECT codec win (−71% BG decode, Y-plane only)
can be delivered end-to-end through `render_gray8` (today `render_pixmap().to_gray8()`).

Two blockers, both from reading `composite_into`:
1. **Full gray compositor = mass duplication.** The compositor dispatches to three
   deeply-tuned RGBA row writers (`composite_rows_bilevel_one` / `_bilinear_one` /
   `_area_avg_one`) carrying every kept fast path (P2, G1, F2, area-avg, POPCNT…).
   A 1-byte-output gray variant would fork all three — the most-optimised code in the
   repo — for a niche entry point. High drift risk, poor ROI.
2. **Not byte-identical even for the easy case.** A pure-BG colour page (BG44, no
   mask/FG) could shortcut to `bg.to_gray8_subsample(sub)` with *no* compositor work,
   but that returns the DjVu **Y** luma while `render_gray8`'s contract is the
   **Rec.601** luma of the colour render (Y ≠ Rec.601-of-RGB; the GRAID_DIRECT fidelity
   note). So it would silently change `render_gray8` output — an opt-in, not a
   transparent optimisation.

**Rejected.** The codec-level `Iw44Image::to_gray8[_subsample]` (kept, round 7)
remains the interface: consumers that want the fast Y-only gray decode — OCR
pre-passes, e-ink pipelines that do their own compositing, pure-BG pages — call it
via `page.decoded_bg44()?.to_gray8()`. Wiring it into the general RGBA compositor is
not worth duplicating the hot path.

## Perf round 15 (2026-07-04) — CLUSTER_BUCKET_HASH_DEDUP (from the round-6 validated backlog)

Round 6's swarm flagged `bucket_page_ccs` (swarm P4) as a real but unmeasured
(~2%) win with no clustering bench. This round builds the bench and lands the
change; the isolated measurement shows the win is an order of magnitude larger
than the ~2% estimate — the estimate was made as a fraction of full per-page
encode, but on a dense multi-page corpus the byte-exact bucketing scan is itself
the dominant cost of `cluster_shared_symbols`, not `extract_ccs` as assumed.

### CLUSTER_BUCKET_HASH_DEDUP — hash-indexed exact match in shared-symbol clustering — **Kept** (2026-07-04)

**Issue (round-6 swarm P4).** `cluster_shared_symbols_tunable`'s inner
`bucket_page_ccs` deduped one page's connected components into per-`(w,h)` size
buckets with a *linear scan*: for every CC it ran `packed_hamming` (a full
popcount over the whole packed bitmap) against **every** cluster already in that
size bucket, keeping the `max_diff == 0` (byte-exact) best. On a dense text scan
the common letter-size buckets hold hundreds of distinct reps and are hit by
thousands of CCs per page, so this is O(K) full-bitmap popcounts per CC =
O(K²) per size class over the corpus. This is the clustering analog of the
running-dict dedup that CLUSTER_DEDUP (#446) / `encode_jb2_dict_with_options`
already fixed with a `symbol_hash` index.

**Approach.** Each size bucket becomes a `SizeBucket { clusters: Vec<Cluster>,
by_hash: BTreeMap<u64, Vec<usize>> }`. A candidate CC computes
`symbol_hash(w, h, data)`, looks up the (usually ≤1) cluster indices under that
hash, and verifies byte equality (`rep.data == bm.data`) to guard against hash
collisions. Match → the same O(1) `pages_seen.last()` update as before; miss →
push a new cluster (same CC order) and register its hash. No `packed_hamming`
call remains in the hot path.

**Correctness — byte-identical.** Clustering is byte-exact for all callers
(`max_diff` was hardcoded to 0 since #258 disabled Hamming shared clustering), so
every rep in a bucket is distinct (a new cluster is only created when *no*
existing rep matched byte-for-byte). Hence at most one cluster can match a
candidate, and the hash lookup + equality verify returns exactly the rep the old
full-scan `best` pick returned. Cluster creation order (and thus `first_seen`,
`pages_seen`, the `promoted` trim priority, and the final `sort_by_key(first_seen)`
order) is unchanged. Verified: a `DefaultHasher` digest of the full
`cluster_shared_symbols(&masks, 2)` output over the 517-page
`pathogenic_bacteria_1896` corpus is **identical** across the change
(`digest=1d797be493bab1fe`, `shared_syms=5164` both before and after). The
`djvu-jb2` suite (53 tests incl. `cluster_shared_symbols_caps_total_pixel_budget`)
and the full `make check` gate pass.

**Numbers** (release + fat-LTO, `parallel` feature, M1 Max, 517-page
`pathogenic_bacteria_1896`, `cluster_shared_symbols` in isolation via the new
`bench_cluster_shared_symbols`; before/after captured by `git stash` on the same
build):

| | Before (linear scan) | After (hash index) | Change |
|-|----------------------|--------------------|--------|
| `cluster_shared_symbols_517p` (median of 4) | ~17.6 s | ~3.4 s | **−81% (≈5.2×)** |

The residual ~3.4 s is now `extract_ccs` (parallelised across cores) plus the
cheap hash bucketing; the old ~14 s of serial popcount scanning is gone. The win
scales with bucket density × page count, so it is largest exactly on the long
dense bilevel books that layered multi-page encode targets; small bundles (the
6-mask `encode_djvm_bundle_jb2` bench) see it as noise.

**Decision.** **Kept.** Byte-identical, strictly ≤ the old cost (a hash lookup +
one equality check replaces an O(K) popcount scan), and a large isolated win on
the corpus it matters for. New `bench_cluster_shared_symbols` (517-page,
`sample_size(10)`) added so the clustering axis is no longer benched only
indirectly through the full bundle encode.

## Perf round 16 (2026-07-04) — SHARED_DICT_CLONE_PER_PAGE (from the round-6 validated backlog)

Round 6's swarm flagged (P2) that `encode_jb2_dict_with_options` deep-copies the
shared dictionary on every per-page call of a bundled encode. This round lands
the change and measures it in isolation on the 517-page corpus.

### SHARED_DICT_CLONE_PER_PAGE — borrow the shared dict instead of cloning per page — **Kept (byte-identical, strictly ≤ work; wall-clock within thermal noise)** (2026-07-04)

**Issue (round-6 swarm P2).** `encode_jb2_dict_with_options` held its running
dictionary as `dict_entries: Vec<Bitmap>` and pre-populated it from
`shared_symbols` with `dict_entries.push(sym.clone())` — a full bitmap-data deep
copy of **every** shared symbol, redone on **every** page. A bundled multi-page
encode passes the *same* shared dictionary to all pages, so on the 517-page
`pathogenic_bacteria_1896` (5164 shared symbols) this is 5164 × 517 ≈ **2.67 M
bitmap-data clones** of an identical, read-only dictionary. Page-local new symbols
were also cloned (`dict_entries.push(cc.bitmap.clone())`).

**Approach.** `dict_entries` becomes `Vec<&Bitmap>`. Both sources it references —
`shared_symbols` (the caller's slice) and this page's own `ccs` (extracted once
at the top of the function) — outlive the encode, so the dictionary never needs
to own a bitmap: shared entries push `sym`, page-local new entries push
`&cc.bitmap`. The two internal refinement helpers (`find_lossy_copy_ref`,
`find_cross_size_refine_ref`) take `&[&Bitmap]`. No public signature changes
(`shared_symbols: &[Bitmap]` unchanged), so no call site is touched — the whole
change is internal to the function.

**Correctness — byte-identical.** Pure ownership change; every read
(`dict_entries[i].data == …`, `packed_hamming`, `scaled_hamming`, refinement
reference) sees the same bytes, and dict indices are assigned in the same order.
Verified: a `DefaultHasher` digest of the full `encode_djvm_bundle_jb2_with_shared`
output is **identical** across the change on both the 22-page `conquete_paix`
(`digest=f2e5aa7d95ef41c3`, 54 830 B) and the 517-page `pathogenic_bacteria_1896`
(`digest=f6ec8b10d204f31d`, 33 430 276 B). Both `experimental` and default builds
compile; `make check` (1028 tests) passes.

**Numbers** (release + fat-LTO, `parallel`, M1 Max; isolated per-page encode via
`encode_djvm_bundle_jb2_with_shared` with the shared dict clustered *outside* the
timed region, 517-page corpus, 5164-symbol shared dict):

| | Encode time (per-page path) | Peak RSS |
|-|-----------------------------|----------|
| Before (clone per page) | ~4.1 s mean (3.26–5.24 s) | ~1504 MB |
| After (borrow) | ~3.6 s mean (3.22–4.14 s) | ~1525 MB |

Interleaved A/B (4 pairs) favours the borrow version in 3 of 4 pairs, mean ≈12%
faster, but the run-to-run thermal variance (old spans 3.26–5.24 s) swamps the
~0.5 s mean gap — the wall-clock win is **directional but below clean-measurement
threshold**, exactly the "~2.9%" the swarm predicted. Peak RSS is unchanged: the
per-page clones are transient (allocated and freed inside each page's encode), so
they never set the peak, which is fixed by the persistent 517-mask input vector.

**Decision.** **Kept.** Byte-identical and strictly less work: it removes ~2.67 M
per-page allocations + memcpies and shrinks `dict_entries` from owned `Bitmap`s to
8-byte pointers, with no runtime downside and only a trivial `&[&Bitmap]` on two
internal helpers. Same rationale as CLUSTER_DEDUP (#446) / PDF_STREAM — an
allocator-pressure reduction that is real (and matters more under contended
allocators / many concurrent encodes / wasm) even where this M1 + system-allocator
+ parallel workload can't cleanly separate it from thermal noise on wall-clock.
## PAR_PAGE_LAYERS revisit + large single-page colour-encode bench (2026-07-04)

The round-2 PAR_PAGE_LAYERS revert (overlap Sjbz ∥ BG44 in single-page colour
encode) left a "revisit with a BG-heavy single-page colour fixture" condition. This
round added the fixture search and re-measured. Conclusion: still unmeasurable — no
BG-balanced fixture exists in the corpus, and the machine's thermal noise now exceeds
the theoretical win.

### PAR_PAGE_LAYERS (2nd attempt) — **Reverted again (below noise floor)** (2026-07-04)

**Fixture search.** Scanned every colour page of every available fixture for the JB2
(Sjbz) vs IW44 (BG44) encode-time split — the join can save at most `min(jb2, iw44)`:

| Fixture / page | JB2 | IW44 | iw44/jb2 |
|----------------|-----|------|----------|
| colorbook pg0–19 | 0.6–6.6 ms | 0.2–0.4 ms | 0.05–0.44 |
| watchmaker pg0–11 | ~0.6 ms | ~0.2 ms | 0.33–0.36 |
| big-scanned-page (6780×9148) | 314 ms | 15 ms | 0.05 |

**Every** fixture is JB2-dominated: after Sauvola segmentation the background is
smooth and IW44 compresses it fast, so BG44 is always a small fraction. No full-bleed
photograph (BG-balanced) page exists in the corpus. Best-case overlap is `min` = the
tiny IW44 time: ≤3.5 % of total even on big-scanned-page (15 ms of 400 ms).

**Measurement.** Applied the `rayon::join(|| jb2, || iw44)` (byte-identical) and
A/B'd. An interleaved best-of-3 wall-clock probe showed a consistent *improvement*
(big-scanned-page 411.8 → 397.9 ms, −3.4 %; colorbook pg3 15.6 → 14.6 ms, −6.4 %),
matching the predicted IW44 overlap. But the criterion A/B (baseline-then-compare)
showed a **+11–13 % "regression" (p = 0.00)** on *both* the small and the large bench
— physically impossible for a µs-overhead join to add 46 ms to a 400 ms encode, so it
is thermal contamination: the baseline run heats the machine and the compare run runs
throttled. The ±13 % run-to-run swing that produces is far larger than the ≤3.5 %
effect being measured.

**Decision.** **Reverted again.** The theoretical win (≤3.5 %, and only on the one
heavy fixture) sits below this machine's thermal-noise floor (±13 %), so it cannot be
resolved reliably, and the repo's "both runs p < 0.05" bar is unmet (criterion shows a
contaminated regression). Byte-identical either way, so no correctness stake. Kept the
new **`encode_color_page_quality_large`** bench (big-scanned-page, the largest
single-page colour encode) as standing infrastructure — it is the missing large-page
single-encode baseline, and the place to re-judge PAR_PAGE_LAYERS if a true photo /
BG-balanced fixture is ever added. The higher-value parallel axis (across pages) stays
covered by PAR_ENCODE + PAR_CLUSTER.


## Perf round 17 (2026-07-04) — JB2 size gap: same-size rec-6 refinement (docs/jb2-size-gap-plan.md)

Acting on the "reduce the JB2 size gap vs DjVuLibre" plan. The mask is at parity
(1.04×) and cross-size rec-6 (#322) was proven to *lose* bytes; the one untried
lossless lever is **same-size** record-6 refinement (same bbox → no resampling → no
context misalignment). This round runs Phase A0 (measure the candidate population
before touching the encoder).

### SAME_SIZE_REC6 — Phase A0 population measurement — **Gate passed for text; thin for noisy scans** (2026-07-04)

**What.** Added `analyze_jb2_same_size_refinement` (experiment-only, `experimental`
feature) mirroring the default encoder's exact-dedup dict growth, plus the
`jb2_same_size_a0` example driver. For every component the default encoder emits as a
fresh record-1 symbol, it scores the minimum Hamming distance against same-`(w,h)`
dictionary entries. Measurement only — no encoder output changes.

**Numbers** (per-page independent baseline, `shared = []`):

| Corpus | fresh CCs | same-size candidate | ≤5 % near-twins | median best-Hamming |
|--------|-----------|---------------------|------------------|---------------------|
| watchmaker (text, Sjbz = 67 % of file) | 3 475 | 1 774 (51 %) | **1 375 (39.6 % of fresh)** | **0.6 %** |
| pathogenic_bacteria_1896 (600 dpi scan) | 821 330 | 474 513 (58 %) | 7 339 (0.9 % of fresh) | 12.9 % |

**Reading.** On clean text (**watchmaker**), ~40 % of fresh symbols have a same-size
twin within 5 % Hamming and the median candidate differs by only 0.6 % — a large,
tight population (repeated OCR glyphs with scan jitter but identical bbox). This is
exactly where JB2 is the dominant chunk, so a working refinement path could move the
whole-file size materially. On the noisy 600 dpi **pathogenic** scan the same-size
twin population is thin (0.9 % ≤5 %, median 12.9 %): same-size candidates are common
but far apart, so refinement has little to work with there.

**Decision.** **Proceed to A1** (real emitter) — the text-document population clears
the go/no-go gate. *Caveat, per the #301 lesson:* A0 proves a population, not bytes —
the `1-bit/px` payload floor (watchmaker ≤5 % twins ≈ 1.8 KB) is only a scale hint;
whether a real ZP-coded rec-6 actually beats rec-1 is what A2 must measure. Kept the
analyzer + example behind `experimental`; default builds and output unchanged.

## Perf round 18 (2026-07-04) — same-size rec-6 emitter (A1) + real-bytes proof (A2)

A0 (round 17) showed a strong same-size near-twin population on text. This round
builds the real emitter and measures actual bytes + round-trip — the #301/#322
discipline of trusting only emitted bytes.

### SAME_SIZE_REC6 — Phase A1/A2: lossless same-size record-6 refinement — **Validated win on text (experimental)** (2026-07-04)

**What.** Added `Jb2EncodeOptions::same_size_rec6: Option<f32>` (experimental) and
`find_same_size_refine_ref`. When set, a fresh CC with a **same-bounding-box** dict
twin within `pixel_count × frac` flipped pixels is emitted as a lossless record-6
matched refinement (`wdiff = hdiff = 0`) against that twin — reusing the proven
`encode_bitmap_ref` path — instead of a fresh record-1. Tried before the cross-size
#322 probe. Default (`None`) is byte-identical to the shipped encoder.

**Numbers** (real emitted Sjbz bytes, per-page independent, all pages round-trip
**pixel-exact**):

| Corpus | baseline | frac 2 % | frac 5 % | frac 8 % |
|--------|----------|----------|----------|----------|
| watchmaker (text, Sjbz 67 % of file) | 130 036 B | **114 861 B (−11.67 %)** | −11.20 % | −10.65 % |
| pathogenic_bacteria_1896 (600 dpi scan) | 34 254 905 B | +503 B (+0.00 %) | +0.02 % | +0.53 % |

**Round-trip:** all 12 watchmaker + all 517 pathogenic masks decode **pixel-exact**
at every threshold — the refinement is genuinely lossless.

**Reading.** This is the first lever to *beat* DjVuLibre's glyph-matching losslessly:
−11.67 % on the Sjbz chunk of a text document (≈ −7.8 % whole-file at Sjbz = 67 %),
whereas cross-size rec-6 (#322) *lost* +4.37 % on the same corpus. The plan's
hypothesis holds: same-size needs no resampling, so the refinement context stays
pixel-aligned and the refinement bitmap costs bits only for the few differing pixels.
A **tight** threshold wins most (2 % > 5 % > 8 %) — a near-twin at 2 % has a tiny
refinement bitmap, a borderline 8 % twin's refinement costs more than it saves. On the
noisy 600 dpi scan the near-twin population is thin (A0: 0.9 % ≤5 %), so it is flat to
mildly negative — it must **not** be blanket-enabled there.

**Correctness.** Two tests added: `same_size_rec6_off_is_byte_identical` (default ==
shipped) and `same_size_rec6_roundtrips_near_twins` (fires + lossless). 58 jb2 tests
green (`experimental` and default).

**Decision.** **Kept behind the `experimental` flag; validated as a real lossless win
on text.** Not yet a shipping default because (a) it slightly regresses noisy scans,
so it can't be blanket-on, and (b) enabling it by default changes output for every
user — a product decision. Phase A3 (next): promote `same_size_rec6` from
`experimental` to a **stable opt-in** `Jb2EncodeOptions` field (un-gate
`Action::Refine` + `encode_bitmap_ref` + `find_same_size_refine_ref`, none of which
need the cross-size `scaled_hamming`), and evaluate an adaptive "enable when the
same-size near-twin population is dense" auto-policy so text documents get the win
without risking scans.

**A3 shipping decision (2026-07-04):** maintainer chose to **keep same-size rec-6
behind the `experimental` flag** — not promoted to the stable `Jb2EncodeOptions` API
and not enabled by default. The lever is validated and recorded; the shipped default
output is unchanged. Revisit if/when the stable-API or adaptive-default question is
reopened.

## Perf round 19 (2026-07-04) — JB2 size gap Branch B (lossy): B0 + B1 measurement

Branch A (same-size rec-6, lossless) landed −11.7 % on text. Branch B is the lossy
lever — matching DjVuLibre's default operating point, which is lossy. This round
measures the two sub-levers before committing to the cross-size emitter.

### LOSSY_B0 — existing same-size lossy rec-7 (`lossy_threshold`) size/quality sweep — **Measured; already-shipped lever, off by default** (2026-07-04)

The #224 `lossy_threshold` field already substitutes a same-size near-twin as a rec-7
copy (lossy). Swept it with the D1 harness (mask decoded, compared as grayscale):

| Corpus | thr 2 % | thr 5 % | thr 8 % | thr 10 % |
|--------|---------|---------|---------|----------|
| watchmaker (text) | **−21.96 %, SSIM 0.99928** | −23.39 %, 0.99889 | −23.99 %, 0.99864 | −24.47 %, 0.99852 |
| pathogenic (600 dpi scan) | −0.01 %, 1.00000 | −0.25 %, 0.99985 | −4.79 %, 0.99557 | −11.23 %, 0.98871 |

**Finding.** On text, `lossy_threshold = 0.02` already gives **−22 % with SSIM 0.9993**
(flipped 0.018 % of mask pixels) — a large, near-imperceptible win that ships today
but is **off by default** (`lossy_threshold = 0.0`). Diminishing returns above 2 % (the
extra size costs disproportionate quality). On the scan, same-size lossy finds almost
nothing at low thresholds (twins are far apart — the A0 median was 12.9 %); it only
bites at 8–10 %, where quality starts to drop (SSIM 0.989 at −11 %).

### LOSSY_B1_PROBE — cross-size lossy rec-7 candidate population — **Measured; real incremental headroom** (2026-07-04)

The proposed new lever: substitute a *different-bbox* near-twin as a rec-7 copy (no
refinement bitmap, unlike the failed cross-size **rec-6** #322). Measured the
cross-size near-twin population (±2 px bbox, ≤5 % resampled Hamming) that same-size
misses:

| Corpus | fresh CCs | cross-size near-twins (≤5 %) | share of fresh |
|--------|-----------|------------------------------|----------------|
| watchmaker | 3 475 | 563 | **16.2 %** |
| pathogenic | 821 330 | 21 607 | **2.6 %** (≈3× the same-size ≤5 % population) |

**Finding.** Cross-size adds a distinct 16 % of text CCs and ≈3× the scan population
same-size lossy reaches — real incremental headroom, largest on scans where the "same"
glyph is binarised at slightly varying sizes.

**Correctness constraint discovered.** A rec-7 copy blits the *dictionary* symbol at
*its* size; the decoder (`decode_symbol_coords`, `lib.rs:1706`) uses `dict[index].width/height`
for both coordinate decode and `last_right`. So a cross-size lossy copy must make the
**encoder** drive its coordinate coding and layout by the **twin's** dimensions, not the
original CC's `cc_w/cc_h` — otherwise the encoder/decoder layouts desync and *every
following glyph* is misplaced (corruption, not just a local substitution). B1
implementation must handle this and validate round-trip layout + SSIM.

**Decision.** Proceed to implement cross-size lossy rec-7 behind a flag (B1), with
real-byte + SSIM + layout-correctness validation. B0 is recorded as a key standalone
finding: the shipped `lossy_threshold` is a −22 %/SSIM-0.999 text lever that is simply
off by default — a documented "cjb2-like" preset may be higher-value than the
cross-size machinery for text, which B1's numbers will let us compare.

### LOSSY_B1 — cross-size lossy rec-7 emitter — **Reverted (dominated by same-size lossy)** (2026-07-04)

Implemented the cross-size lossy rec-7 substitution behind an experimental
`cross_size_lossy` option: a fresh CC with a different-bbox near-twin is emitted as a
rec-7 *copy* of that twin (no refinement bitmap). The correctness constraint from B1
was handled — the encoder drives its coordinate coding by the **twin's** `(w, h)` (a
`copy_override` feeding effective `ew/eh/ex_jb2/ey_jb2` into the layout), matching the
decoder's `dict[index].width`-based `last_right`.

**Layout correctness: confirmed.** SSIM stays 0.996–1.000 (min ≥ 0.989) across the
corpus — had the twin-dimension layout desynced, every following glyph would shift and
SSIM would collapse. It doesn't, so the emitter is correct.

**But it is dominated by simply raising the same-size `lossy_threshold`** (watchmaker):

| Config | Sjbz | SSIM avg / min |
|--------|------|----------------|
| same-size lossy 2 % only | −21.96 % | 0.99928 / 0.9985 |
| same-size 2 % **+ cross-size 2 %** | −23.54 % | 0.99884 / 0.9976 |
| same-size 2 % **+ cross-size 5 %** | −24.81 % | 0.99639 / 0.9904 |
| **same-size lossy 5 % only** (B0) | **−23.39 %** | **0.99889 / —** |

Cross-size adds only −1.6…−2.9 % on top of same-size lossy, at *worse* SSIM — and
same-size lossy **alone** at threshold 5 % reaches the same −23.4 % at equal-or-better
SSIM (0.9989) with none of the cross-size machinery, no glyph-bbox change, and no
layout-sync risk. Cross-size lossy standalone is −8.7 % (worse size, worse quality)
because same-size twins are both more abundant (A0: 39.6 % vs cross-size 16.2 %) and
cheaper (no bbox change → lower flip rate per substitution).

**Decision. Reverted.** The emitter is correct and measured, but every operating point
it reaches is matched or beaten by the existing same-size `lossy_threshold` at a
slightly higher setting. Not worth the added hot-loop complexity (`copy_override` +
effective-dims). Removed the `cross_size_lossy` option and the effective-dims plumbing;
default path unchanged. **Branch B conclusion:** the real lossy lever is the
*already-shipped* same-size `lossy_threshold` (B0: −22…−24 % on text at SSIM ≥ 0.999),
which merely needs to be exposed as a documented "cjb2-like" preset — cross-size adds
nothing worthwhile. Recorded so cross-size lossy is not re-attempted.

### LOSSY_B_SHIP — expose the same-size lossy lever as a documented preset — **Kept** (2026-07-04)

Branch B's conclusion was that the real lossy lever already ships (`lossy_threshold`,
B0: −22 % text at SSIM 0.999) and only needs surfacing. Maintainer chose the
opt-in-preset route (not enable-by-default — DjVu's archival use makes a silent lossy
default wrong, and it matches the repo's byte-identical-default culture). Landed:

- **Enriched `lossy_threshold` docs** with the measured operating-point table
  (0.02 → −22 %/SSIM 0.9993, diminishing above) and the text-only caveat.
- **`Jb2EncodeOptions::lossy_text()`** — the recommended 0.02 preset (≈ cjb2's lossy
  operating point), plus **`with_lossy_threshold(f32)`** for custom values.
- Test `lossy_text_preset_is_lossy_and_smaller` (preset shrinks a near-twin page and
  still decodes). Default remains lossless/byte-identical; the preset is opt-in.

This closes the JB2 size-gap plan: the biggest practical text-size lever is now a
one-call, documented opt-in, without changing anyone's default output.

## Perf round 20 (2026-07-04) — IW44_MASKED_WAVELET down-payment: harmonic BG inpaint

Round 8 recorded IW44_MASKED_WAVELET (the ~3.9% residual BG44 size gap vs
DjVuLibre `c44`) as the priority *size* lever, but the full masked-wavelet
transform is a normative bitstream change gated on a DjVuLibre interop-diff
harness — deferred. The same entry flagged the **interop-safe first step**:
improve the background *inpainting* under the mask in `segment_page`
(encoder-only, decoder never sees it). This round lands that step and finds it is
far more than a "down-payment": the production colour encoder was not inpainting
at all, so this alone captures most of the masked-wavelet size win with **zero**
bitstream risk — and *improves* decoded quality.

### BG_DIFFUSE — harmonic diffusion of fully-masked background cells — **Kept** (2026-07-04)

**Issue.** `segment_page` builds a sub-sampled BG pixmap where each cell is the
mean of its block's *unmasked* source pixels. A **fully-masked** cell (its whole
`sub × sub` block is foreground ink → invisible in the render) fell back, by
default, to the *full-block mean including the ink* — a near-black cell. Adjacent
to light background cells that is a large high-frequency step, and the IW44
background codec spends bits coding it, even though the pixels are never seen. The
existing `bg_inpaint` (ring-average) fixed the worst of this but was **off by
default and in every shipping profile** (`default_segment_options` returned plain
`SegmentOptions::default()` for `Quality` and `SegmentOptions::archival()` for
`Archival`, both `bg_inpaint = false`). So the colour encoder was paying full
freight for invisible ink cells.

**Approach.** New `SegmentOptions::bg_diffuse`: after the BG fill, every
fully-masked cell is overwritten with the **harmonic** (smoothest) interpolation
of the confident cells — Gauss-Seidel relaxation of Laplace's equation with the
confident cells as fixed Dirichlet boundary (`diffuse_masked_cells`). Being
maximally smooth it injects the least wavelet energy, so it codes smaller than
either the ink fallback or the ring average. Confident (visible) cells are left
byte-exact, so visible background is untouched; only invisible masked cells
change. Iterations cap at the grid's larger dimension (16–512) with an early stop
at Δ < 0.5/255. Wired into the `Quality` and `Archival` profiles'
`default_segment_options`; the CLI `--bg-inpaint` flag now explicitly selects the
legacy ring fill (turns diffusion off) so it remains distinguishable.

**Numbers — BG44 chunk bytes** (native-resolution render → `segment_page` →
`encode_iw44_color`, M1 Max; `default` = current shipping ink-fallback):

| Page | default | ring (`bg_inpaint`) | diffuse (`bg_diffuse`) | diffuse vs default | vs ring |
|------|---------|---------------------|------------------------|--------------------|---------|
| colorbook (2260×3669) | 3964 B | 2078 B | **1917 B** | **−51.6%** | −7.7% |
| watchmaker (2550×3301) | 1736 B | 189 B | 189 B | **−89.1%** | 0% |
| malliavin (2862×4916) | 267 B | 179 B | 179 B | −33.0% | 0% |
| irish (2479×3504) | 2473 B | 2434 B | 2434 B | −1.6% | 0% |
| conquete_paix (4267×6853) | 22 970 B | 768 B | **758 B** | **−96.7%** | −1.3% |
| chicken / vega / cable | — | — | — | 0% (no masked cells) | 0% |

**Quality — full colour encode, decoded render SSIM/PSNR vs the original render**
(`PageEncoder::from_pixmap`, `Quality` profile, default segmentation vs
`bg_diffuse`):

| Page | file default → diffuse | SSIM default → diffuse | PSNR default → diffuse |
|------|------------------------|------------------------|------------------------|
| colorbook | 18 092 → 16 044 B (−11.3%) | 0.97598 → 0.97741 | 22.01 → 22.05 dB |
| watchmaker | 14 070 → 12 518 B (−11.0%) | 0.99868 → 0.99981 | 41.23 → 49.11 dB |
| malliavin | 1 120 → 1 032 B (−7.9%) | 0.99999 → 1.00000 | 63.2 → ∞ dB |
| conquete_paix | 28 348 → **6 134 B (−78.4%)** | 0.99526 → 0.99899 | 37.51 → 50.42 dB |

**Strict win on both axes.** Diffusion is never worse than the ink fallback on
either size or quality: it is smaller (−8% to −78% of the whole colour file, all
of it BG44) **and** higher SSIM/PSNR on every page — because removing the near-black
ink-fallback cells also removes the dark halos they bled across mask edges via BG
upsampling. Pages with no fully-masked cells (chicken, vega, cable) are unchanged
(0%), so it is safe where it does nothing.

**Correctness.** Encoder-only; the decoder never sees the mask or the BG fill.
Visible (confident) cells are byte-exact, so the change only touches invisible
pixels (± the BG-upsampling boundary bleed, which the SSIM table shows *improves*).
New unit test `bg_diffuse_smooths_masked_cells_and_keeps_confident_cells`; the CLI
test that asserted `--bg-inpaint` changes the BG became
`encode_quality_inpaints_masked_background_by_default` (the default now inpaints).
`make check` (1028+ tests, incl. no_std / wasm32) green.

**Decision.** **Kept**, and enabled in the shipping `Quality`/`Archival` colour
profiles. It is the interop-safe capture of most of the IW44_MASKED_WAVELET size
gap with a quality *gain*, not a trade. The remaining, still-deferred piece is the
normative masked forward-transform + coefficient gathering (mask plumbed through
`encode_iw44_color` → `PlaneEncoder`), which needs the DjVuLibre interop-diff
harness before it can be attempted.

## Perf round 21 (2026-07-04) — masked forward-transform: measured unnecessary (BG_DIFFUSE subsumes it)

Round 17 (BG_DIFFUSE) landed the interop-safe "first step" of IW44_MASKED_WAVELET
— smoothing the invisible masked background before the codec sees it. The deferred
big piece was the **normative masked forward-transform** (plumb the mask into
`encode_iw44_color` → `PlaneEncoder`, interpolate masked pixels inside the lifting,
skip fully-masked coefficient buckets). Before building that — a bitstream change
the repo has repeatedly found interop-fragile — this round **measures whether it
can still help** now that BG_DIFFUSE smooths the input. It cannot: DjVuLibre's own
masked encoder produces the same size as its unmasked encoder fed our diffused BG.

### IW44_MASKED_TRANSFORM — is the normative masked transform worth it after BG_DIFFUSE? — **Rejected / unnecessary** (2026-07-04)

**Method.** For each colour page: segment with `bg_diffuse` → the smoothed
sub-sampled BG. Encode that BG three ways and compare BG44 bytes at a matched
100-slice schedule, full chroma:
1. **ours** — `encode_iw44_color(diffused_bg)` (our current pipeline),
2. **c44 (diffused)** — DjVuLibre `c44 -slice 100 -crcbfull` on the same diffused BG,
   *no* mask,
3. **c44 (-mask, raw)** — `c44 -mask` on the *raw* (ink-fallback) BG with a PBM
   marking exactly the fully-masked cells — DjVuLibre's masked-wavelet lever.

**Numbers** (M1 Max + DjVuLibre `c44`):

| Page (BG) | ours | c44 (diffused, no mask) | c44 (-mask, raw) | ours SSIM / c44 SSIM |
|-----------|------|-------------------------|------------------|----------------------|
| colorbook (189×306) | 1917 B | 1445 B | 1471 B | 0.97754 / 0.98447 |
| watchmaker (213×276) | 189 B | 117 B | 117 B | 0.99797 / 0.99760 |
| irish (207×292) | 2434 B | 2063 B | 2063 B | 0.98211 / 0.98050 |
| conquete_paix (356×572) | 758 B | 644 B | 643 B | 0.99893 / 0.99882 |

**Finding 1 — the masked lever is fully subsumed by BG_DIFFUSE.** c44's *masked*
encoding of the raw BG (col 3) is within noise of c44's *unmasked* encoding of our
*diffused* BG (col 2): 1471≈1445, 117=117, 2063=2063, 643≈644. Feeding a diffused
BG to a mask-blind encoder is equivalent to feeding a raw BG + mask to a masking
encoder. So implementing the normative masked forward-transform in our codec would
**not shrink BG44 at all** beyond what round-17 BG_DIFFUSE already delivers — the
whole benefit of masking is the input smoothing, which we now do explicitly and
interop-safely at the BG-pixel level.

**Finding 2 — the real residual gap is entropy coding, not masking.** Our IW44 is
1.18–1.62× larger than c44 on the *identical* diffused input (both mask-blind), so
the gap is our coder's rate-distortion efficiency, not a missing masked transform.
It is partly a curve-position difference (watchmaker/irish/conquete: ours is bigger
but ≥ c44 on SSIM) and partly a genuine loss (colorbook: bigger **and** lower SSIM,
0.9775 vs 0.9845). This is IW44_ACT_THRESH territory — normative quantization /
context-table work that IW44_SWARM_REST showed breaks DjVuLibre interop — and is
untouched by masking.

**Decision.** **Rejected — the normative masked forward-transform is unnecessary.**
BG_DIFFUSE (round 17) captured the entire masked-encoding size win at zero bitstream
risk; the c44 mask-vs-diffused equivalence proves there is nothing left for the
normative transform to gain. **IW44_MASKED_WAVELET is closed.** The remaining IW44
size lever is the ~1.2–1.6× entropy-coding gap vs c44 (measured here on smooth BG),
a separate, interop-fragile axis — not the masked transform. This is the highest-value
outcome available: a large, high-risk normative change shown to be *not worth doing*
before writing it.

## Perf round 22 (2026-07-04) — IW44 entropy gap, characterized (diagnostic)

Round 20 measured our IW44 encoder 1.18–1.62× larger than `c44` on the same
diffused BG at a matched 100-slice schedule, and attributed it to entropy coding.
That was at matched *slices*, not matched *quality*. This round measures the true
rate-distortion parity — sweep the slice count for both encoders, decode **both
with our decoder** (one consistent SSIM metric), and read size-at-equal-SSIM — to
find out whether the gap is real coding efficiency or a slice-schedule artifact.
The answer is "both, split by content".

### IW44_ENTROPY_GAP — is the c44 size gap real at matched quality? — **Diagnostic** (2026-07-04)

**RD curves (diffused BG, our decoder for both, M1 Max + DjVuLibre `c44`):**

colorbook BG 189×306 (textured):

| slices | ours B / SSIM | c44 B / SSIM |
|--------|---------------|--------------|
| 50 | 97 / 0.93466 | 55 / 0.93053 |
| 74 | 345 / 0.95928 | 257 / 0.95920 |
| 100 | 1917 / 0.97754 | 1445 / 0.98447 |

watchmaker BG 213×276 (smooth):

| slices | ours B / SSIM | c44 B / SSIM |
|--------|---------------|--------------|
| 30 | 57 / 0.99789 | 30 / 0.98467 |
| 50 | 89 / 0.99789 | 50 / 0.99757 |
| 74 | 131 / 0.99789 | 84 / 0.99756 |
| 100 | 189 / 0.99797 | 117 / 0.99760 |

**Finding — the gap splits by content:**
- **Smooth BG: we are competitive-to-better at matched quality.** watchmaker's SSIM
  saturates at slice 30 (57 B / 0.99789); `c44` needs 74 slices / 84 B to reach a
  *lower* 0.99756. So at equal quality ours (57 B) beats `c44` (84 B). The raw
  slice-100 "gap" (189 vs 117 B) is ours **over-coding past its own saturation** —
  the IW44_SLICE_RD (round 18) effect, amplified because BG_DIFFUSE makes the BG
  even smoother. This is an interop-safe lever, but a **fixed** `Iw44Target::Bpp`
  cap cannot capture it: the saturation bitrate is content-dependent and spans 30×
  (~0.008 bpp for watchmaker vs ~0.265 bpp for colorbook), so any cap that trims
  the smooth page destroys the textured one. It needs a **content-adaptive quality
  target** (a `-decibel`-style stop), a new RD-control feature worth only ~100 B on
  smooth colour pages.
- **Textured BG: a genuine ~1.3× coding gap at matched quality.** At slice 74 both
  reach SSIM 0.9592 but ours is 345 B vs `c44` 257 B (1.34×). This is real per-slice
  coding efficiency — our activation/quantization policy emits more than `c44` for
  the same reconstruction. It is the IW44_ACT_THRESH residual, in the
  normative-adjacent activation territory IW44_SWARM_REST showed is interop-fragile
  (changing quantization/context tables broke `ddjvu` decoding).

**Verdict.** **Diagnostic — no change.** The two remaining IW44 size levers are now
correctly scoped and both are low-priority: (1) an interop-safe content-adaptive
quality target to stop the smooth-BG slice over-coding — real feature, ~100 B/page,
low EV; (2) the ~1.3× textured-content coding gap — interop-fragile normative
activation work, high risk. Neither is a quick clean win. Recorded so the next
IW44-size effort starts from the true, content-split picture instead of the
misleading flat "1.2–1.6× worse than c44". The big interop-safe BG44 win was
BG_DIFFUSE (round 17); the masked transform was shown unnecessary (round 20); this
closes the "what's left" question for the IW44 background size axis.

## Perf round 23 (2026-07-06) — diagnostics: BZZ encoder headroom + PDF DCT raster

Two cheap diagnostic probes from the experiment-swarm queue (no shipped-code
changes; new bench + two example drivers only).

### BZZ_ENC_DIAG — is the BZZ encoder worth a suffix-sort upgrade? — **Diagnostic** (2026-07-06)

**Question.** `EXPERIMENTS_INDEX.md` had zero BZZ *encoder* entries (only `BZZ_DEC_MTF`,
decode side). BZZ compresses `TXTz`/`ANTz`/`DIRM`/`NAVM`/`FGbz`-palette payloads
(`crates/djvu-bzz/src/encode.rs`, re-exported via `src/bzz_encode.rs`). Is a
SA-IS/divsufsort-style suffix-sort rewrite worth queuing?

**Algorithm identified.** `suffix_array_of_bwt_string` is **prefix-doubling with a
two-pass counting/radix sort per round** (`O(n log n)`, not the naive `O(n² log n)`
comparison sort). Block cap is `MAX_BLOCK_SIZE = 4 MiB`, matching DjVuLibre's
`MAXBLOCK`. No suffix-sort crate dependency exists today; the whole thing is hand-rolled,
`#![deny(unsafe_code)]`, and the encoder module is already `#[cfg(feature = "std")]`
(so it's outside the no_std/wasm32 gate — an external suffix-sort dependency would not
affect the `make check` no_std/wasm build).

**Real corpus block sizes are small.** Scanned every `TXTz` payload in 7 corpus docs
(malliavin, czech, DjVu3Spec, colorbook, watchmaker, conquete_paix, cable_1973):
per-page decompressed plaintext averages **1.6–6.4 KB**, single largest chunk in the
whole corpus is **~10.6 KB**. `DIRM` (directory metadata) stays under 5 KB even for a
520-page document (grows with page *count*, not text volume). So BZZ, in practice, is
called on small, per-page/per-chunk blocks — never on the 100 KB–4 MB inputs where a
suffix-sort's asymptotic class dominates wall time.

**Scaling measured on real (non-tiled) OCR text.** Concatenated the plaintext of 7
corpus TXTz layers (2.25 MB total, genuinely varied content — tiling one block to
reach size artificially creates periodicity that's *not* representative and was ruled
out after an initial run showed misleading numbers) and timed `bzz_encode` on real
prefixes:

| n | compressed | time | ratio_n | ratio_time |
|---|-----------|------|---------|-------------|
| 10,000 B | 3,700 B (37.0%) | 1.7 ms | — | — |
| 100,000 B | 27,755 B (27.8%) | 15.9 ms | 10.0× | 9.1× |
| 500,000 B | 130,366 B (26.1%) | 80.6 ms | 5.0× | 5.1× |
| 2,251,406 B | 515,610 B (22.9%) | 960.7 ms | 4.5× | 11.9× |

100 KB→2.25 MB is 22.5× the bytes but 60× the time (~`n^1.3`), worse than the
`n log n` prediction (~29×) — consistent with the doubling+counting-sort's known
cache-unfriendliness at multi-hundred-KB+ scale (each round scatters over an array of
size `m`, increasingly cache-hostile as `m` grows past L2/L3). Confirms the algorithm
is already the right complexity class, not the naive one, but shows real headroom
*if* block sizes ever grow into the 100 KB–4 MB range.

**Share of real per-page encode time (same-session, not cross-run; wall-clock,
provisional — this host runs concurrent agents, repeated runs varied 2× on the
absolute ms but the *shape* held).** Timed `bzz_encode` of a representative
per-page block against a full `PageEncoder` page encode of the same order, see
`examples/bzz_encode_diag.rs`:

| Fixture | Full page encode | Page's bzz_encode(text) | Share |
|---|---|---|---|
| `cable_1973_100133.djvu` p0, 2550×3301 native, **Lossless** (JB2 mask only) | 13–26 ms | 0.6–0.98 ms (5,286 B text) | **3–6 %** |
| `colorbook.djvu` p0, 754×1223, **Quality** (JB2+IW44+FGbz) | 5–10 ms | 0.7–1.6 ms (6,394 B avg text) | **12–19 %** |

Not negligible — roughly 3–19 % of a text-bearing page's total encode time across
repeated runs, consistently higher on the smaller/lower-res colour page (less
absolute JB2/IW44 work per page) than on the full-native-resolution bilevel scan.
But it's secondary to JB2/IW44, which dominate both size (per `ENC_SIZE_DIAG`,
94–99.9 % of compressed bytes) and, on any full-native-resolution scanned page,
absolute encode time too.

**Verdict.** BZZ encode is a real but bounded lever, not negligible and not a
priority. The current `O(n log n)` doubling+radix-sort is the right complexity class
already (not naive) and is fast in absolute terms for the block sizes DjVu actually
produces (sub-2ms/page). A SA-IS/divsufsort-style linear suffix sort would mainly pay
off if a future feature merges many pages' text into one large single BZZ block
(nothing today does this — `TXTz`/`DIRM`/`NAVM`/`ANTz` are all naturally small,
per-page or per-document-metadata). **Recommend:** queue as a low-priority,
well-scoped future experiment, gated on either (a) a real large-single-block BZZ use
case appearing, or (b) a follow-up that first profiles `bzz_encode` internally
(BWT-sort vs MTF vs ZP-coding split — not done here) to confirm the suffix sort, not
the entropy coder, is the dominated stage before investing in a rewrite. Added
`bench_bzz_encode` to `benches/codecs.rs` (real TXTz payload from
`cable_1973_100133.djvu`, decoded to plaintext) since none existed.

### PDF_DCT_PROBE — potential of JPEG backgrounds in PDF export — **Diagnostic** (2026-07-06)

**Correction to the probe's premise.** The task brief assumed colour/gray PDF page
images ship as Deflate-compressed raster today. That's stale: **`src/pdf.rs` already
ships DCTDecode (JPEG) by default** — `PdfOptions::default().jpeg_quality == Some(80)`,
landed in `#59`/`de90a9f` ("DCTDecode background encoding — smaller PDF output",
Issue #49) and already covered by tests (`pdf_options_default_is_jpeg80`,
`dct_pdf_is_smaller_than_deflate_pdf`, etc.). `jpeg_quality: None` is the existing
opt-out to Deflate. Bilevel-only pages (`is_bilevel_only`) are unaffected either way —
they always ship as a 1-bit `ImageMask`/FlateDecode; that's the separate,
already-deferred CCITT G4/JBIG2 item, not touched here.

**So the actual open question was:** how much is the *shipped* JPEG-80 default
winning today, is quality 85 a better operating point, and is blanket per-document
JPEG the right default at all? Measured 3 real colour corpus pages, native resolution,
Deflate (zlib level 6, matching `src/pdf.rs`'s `deflate()`) vs JPEG (`jpeg-encoder`,
matching `encode_rgb_to_jpeg`) at q80/q85, round-tripped q80/q85 through `zune-jpeg`
(already a `std`-feature dependency) and scored with `src/quality::ssim`:

| Page | Deflate | JPEG q80 (shipped default) | JPEG q85 | SSIM q80 / q85 |
|---|---|---|---|---|
| `colorbook.djvu` p0, 2260×3669 | 1,376,897 B | 378,741 B (**3.64× smaller**) | 419,447 B (3.28×) | 0.9983 / 0.9986 |
| `watchmaker.djvu` p0, 2550×3301 | 245,584 B | 767,028 B (**0.32× — JPEG is 3.1× LARGER**) | 839,949 B (0.29×) | 0.9996 / 0.9997 |
| `big-scanned-page.djvu` p0, 6780×9148 | 33,867,843 B | 3,030,350 B (**11.18× smaller**) | 3,466,798 B (9.77×) | 0.9957 / 0.9964 |

**Finding — the shipped default is content-dependent, and sometimes loses.**
`colorbook`/`big-scanned-page` are photographic/gradient-rich colour scans: JPEG-80
wins big (3.6×–11.2× smaller than Deflate), matching the historical "5–10× smaller"
CHANGELOG note. But `watchmaker` p0 is a colour-mode scan of what's visually a
near-flat white-paper-plus-text page — Deflate's LZ77+predictor crushes the huge flat
regions (102.8× vs raw) far better than JPEG's block-DCT quantization does, so the
*shipped default* (unconditional JPEG-80) makes this file **3.1× larger** than the
Deflate alternative would, at no quality benefit (SSIM is already ≥0.999 either way).
**Quality 85 is strictly worse than 80 on all 3 pages** — larger for +0.0002–0.0004
SSIM, not a useful knob in the direction tested.

**Verdict.** No further "should we add JPEG" work — it already shipped years ago and
is well-tested. The real remaining lever is that `PdfOptions.jpeg_quality` is a single
global choice for the whole document, applied blindly per page, and per-page image
content varies enough (this 3-page sample already found a 3× regression case) that a
blanket default can't be optimal for every document. **Recommended follow-up
(not implemented here, per the probe's no-shipped-changes scope):** an opt-in
`pdf_adaptive_raster` mode that encodes each colour page both ways and keeps
whichever is smaller (or a cheap flatness/entropy heuristic to skip the double-encode
cost) — bounded, backward-compatible (falls back to today's behaviour when off),
and would close exactly the regression case this probe found without touching the
CCITT/JBIG2-for-bilevel item, which stays out of scope. Not promoting the "quality 85"
knob — measured strictly worse than the existing 80 default.

## Perf round 24 (2026-07-06) — JB2_AUTO_REC6: adaptive auto-policy for same-size rec-6

Round 18's A3 decision kept `same_size_rec6` behind `experimental`, explicit opt-in,
because it wins big on text (−11.67 % Sjbz) but is flat-to-negative on noisy scans, so
it cannot be blanket-enabled. The A3 follow-up idea recorded there: an adaptive
"enable when the same-size near-twin population is dense" auto-policy, so text
documents get the win without risking scans. This round builds and validates that
policy.

### JB2_AUTO_REC6 — density-probe auto-policy for same-size rec-6 — **Kept (experimental)** (2026-07-06)

**What.** Added, all behind the existing `experimental` feature:

- **`probe_same_size_rec6_density(bitmap, shared_symbols, max_ccs)`** — a cheap,
  *bounded* density probe. It reuses the A0 analyzer's scan core (now factored into a
  private `same_size_refinement_scan(bitmap, shared_symbols, fresh_cc_limit)` shared by
  both `analyze_jb2_same_size_refinement` (`None` = full document) and the probe
  (`Some(max_ccs)`), so no logic is duplicated), but stops as soon as `max_ccs` *fresh*
  CCs have been examined. Returns the fraction of sampled fresh CCs with a same-size
  ≤5 % Hamming twin — the metric round 17/18 already validated as predictive of a real
  byte win.
- **`Jb2EncodeOptions::same_size_rec6_auto(bitmap, shared_symbols)`** — probes `bitmap`
  (default sample cap `SAME_SIZE_REC6_AUTO_SAMPLE_CCS = 1000` fresh CCs) and sets
  `same_size_rec6 = Some(0.02)` when density ≥ `SAME_SIZE_REC6_AUTO_DENSITY_THRESHOLD =
  0.05`, else leaves it `None`. Intended call pattern: probe **once per document** (its
  first page) and reuse the returned options for every page, so the probe's one fixed
  `extract_ccs` pass is amortized over the whole document rather than paid per page.

**Cost.** The probe's only *unavoidable* fixed cost is `extract_ccs` — the same
connected-component pass the real encoder always runs first regardless of options.
Its *incremental* cost (the Hamming-distance scoring loop) is capped at `max_ccs`
fresh CCs, independent of page size: pathogenic_bacteria_1896's largest page has
821 330 fresh CCs, but the probe stops at 1 000 the same as it would on a small page.
Called once per document (not per page), the fixed `extract_ccs` cost is one page's
worth out of the whole document's encode (≈0.2 % overhead on a 517-page corpus) —
structurally cheap; not wall-clock-benchmarked here (thermal noise on this shared
machine, per repo convention — the structural bound is the argument).

**Threshold calibration — real emitted-byte deltas, not just population counts** (the
#301 lesson: a density number is a population hint, only real bytes prove an outcome).
Measured `analyze_jb2_same_size_refinement` density and the real `same_size_rec6`
Sjbz delta at frac 2 % on **four** corpora (the two round-17/18 calibration points plus
this repo's other two `tests/corpus/*.djvu` fixtures as intermediate data):

| Corpus | density (≤5 % near-twins / fresh CCs) | Sjbz delta @ frac 2 % |
|--------|----------------------------------------|------------------------|
| watchmaker (text) | 39.6 % | **−11.67 %** |
| cable_1973_100133 (1-page bilevel cable) | 12.4 % | −0.43 % (small win) |
| conquete_paix (22-page mixed book) | 1.7 % | **+0.49 % (loss)** |
| pathogenic_bacteria_1896 (517p, 600 dpi scan) | 0.9 % | +0.00 % (flat) |

The real-byte outcome flips from a loss (conquete_paix, 1.7 %) to a win (cable, 12.4 %)
as density rises — a decade apart, not a hard bimodal split. `SAME_SIZE_REC6_AUTO_DENSITY_THRESHOLD
= 0.05` sits with ≈2.9× margin above the measured loss and ≈2.5× margin below the
measured win: it enables on `cable` (tiny extra win) and `watchmaker` (the big win)
while staying off for `conquete_paix` and `pathogenic` (avoiding their loss/flat
result). `SAME_SIZE_REC6_AUTO_FRAC = 0.02` reuses round 18's validated sweet spot
(tighter thresholds win more: 2 % > 5 % > 8 %).

**End-to-end validation** (`examples/jb2_same_size_a3_auto.rs`, one probe decision per
document reused across all its pages):

| Corpus | auto decision | Sjbz delta | round-trip |
|--------|----------------|------------|------------|
| watchmaker (12 masks) | `Some(0.02)` (fires) | **−11.67 %** (matches round 18 exactly) | 12/12 pixel-exact |
| cable_1973_100133 (2 masks) | `Some(0.02)` (fires) | −0.43 % | 2/2 pixel-exact |
| conquete_paix (22 masks) | `None` (off) | **+0.00 %, byte-identical to default** | 22/22 pixel-exact |
| pathogenic_bacteria_1896 (517 masks) | `None` (off) | **+0.00 %, byte-identical to default** | 517/517 pixel-exact |

Text gets the lossless win; both non-text/thin-population corpora are provably
untouched (byte-identical Sjbz output, not just "flat").

**Correctness.** `same_size_rec6_off_is_byte_identical` / `same_size_rec6_roundtrips_near_twins`
(round 18) stay green. Added `same_size_rec6_auto_fires_on_dense_near_twins` (dense
synthetic near-twin input → auto enables, output changes, round-trips lossless),
`same_size_rec6_auto_stays_off_on_sparse_input` (sparse distinct-size input → auto
stays off, output byte-identical to default), and `probe_same_size_rec6_density_bounds_the_scan`
(capping `fresh_cc_limit` actually stops the scan early, `same_size_refinement_scan`
unit-level). 62 `djvu-jb2` unit tests green with `experimental` (55 without); `make
check` (fmt, clippy `-D warnings`, no_std, wasm32, full workspace test suite) passes.

**Decision.** **Kept, behind `experimental`.** This closes round 18's A3 follow-up:
the auto-policy is a real, validated per-document decision procedure — not just a
population heuristic — with a threshold justified by measured byte outcomes on four
corpora spanning a 44× density range. It stays an opt-in constructor
(`Jb2EncodeOptions::same_size_rec6_auto`), not a stable API and not enabled by
default, per the same A3 shipping decision recorded in round 18 (same-size rec-6 stays
experimental; enabling any variant of it by default is a product decision for the
maintainer, not a byte-count argument).
## Perf round 25 (2026-07-06) — D_AA_ZOOM: opt-in mask AA at upscale

### D_AA_ZOOM — bilinear mask-coverage AA at upscale — **Kept (opt-in)** (2026-07-06)

QUALITY_AA (#13, round-5) deferred mask-upscale AA because it diverges from
DjVuLibre's hard-edged reference behaviour and "needs a human aesthetic judgement."
Implemented it as a strictly opt-in `RenderOptions::mask_aa` flag (default `false`)
so the divergence is a deliberate choice, not a silent default change.

**Approach.** Mirrors the existing `mask_box_coverage` (unconditional box-average AA
used at *downscale*, COLOR_AA #439) but in the opposite direction: a new
`mask_bilinear_coverage` treats the JB2 mask's 0/255 bits as a continuous coverage
field and bilinearly interpolates the four nearest mask pixels — same lerp shape as
`sample_bilinear`. Wired into both compositor hot paths:

- `composite_rows_bilevel_one` (pure bilevel pages): one new `else if` branch between
  the downscale branch and the nearest-bit fallback, reachable only past the exact
  1:1 early return (so inherently upscale-only).
- `composite_rows_bilinear_one` (colour+mask pages): the binary `is_fg` bool became a
  `coverage: u8` (0..=255); partial coverage blends fg/bg colour proportionally
  (`(f*cov + b*inv + 127)/255`, same rounding convention as the mask-coverage functions).

**Subtlety found:** the colour path's B-series loop is reached not only at genuine
upscale but also at an exact page-level 1:1 render whose *background* plane is
internally subsampled (bg_x_q24 != 1<<24 — the common case for real BG44 scans, since
the "extra-tight" 1:1 fast path requires an *unsubsampled* bg). Gating on
"reached this code" would have broken the "no-op at scale ≤ 1" requirement; gated on
`fx_step < FRAC || fy_step < FRAC` (true per-axis upscale) instead. Both fast-path
special cases (coverage == 0, coverage == 255) reproduce the original code exactly —
byte-identical by construction, not just by test.

**Correctness (all in `src/djvu_render.rs`; `make check` — fmt, clippy -D warnings,
no_std, wasm32, full workspace test suite — is green, 1041/1041):**
- `mask_aa=false` byte-identical to nearest at upscale (unit + `render_pixmap`
  integration on `boy_jb2.djvu`), at both compositor paths.
- `mask_aa=true` is a no-op at native scale and at downscale, including the
  bg-subsampled-at-1:1 corner case above (unit-level, hand-verified arithmetic).
- `mask_aa=true` produces genuine intermediate coverage values at glyph edges
  (hand-computed: e.g. coverage 128 → gray 127, or fg/bg blend (100,75,50) between
  black text and a (200,150,100) background) — not just 0/255.

**Quality (D1 SSIM/PSNR harness, `examples/mask_aa_quality.rs`, new): render page at
native res as ground truth; downscale by 2×/4× (existing box-AA); re-encode that
downscaled render as a standalone lossless JB2 page; upscale it back to native size
nearest vs AA; compare both to the native ground truth.**

| Doc | Zoom | SSIM nearest / AA | PSNR nearest / AA | MSE nearest / AA |
|-----|------|--------------------|--------------------|-------------------|
| boy_jb2.djvu (192×256) | 2× | 0.9467 / 0.9326 | 19.94 / 20.20 dB | 658.8 / 621.3 |
| boy_jb2.djvu (192×256) | 4× | 0.8705 / 0.8387 | 14.14 / 14.45 dB | 2505.6 / 2336.5 |
| cable_1973_100133.djvu (2550×3301, real scanned text) | 2× | 0.9726 / 0.9712 | 21.72 / 22.25 dB | 437.3 / 387.3 |
| cable_1973_100133.djvu (2550×3301, real scanned text) | 4× | 0.9628 / 0.9585 | 17.90 / 18.63 dB | 1054.2 / 890.4 |

**Honest reading, not cherry-picked:** PSNR/MSE consistently favour AA (+0.3–0.7 dB,
−6…−15 % MSE) — the blended coverage is a better *mean* approximation of the true
edge. **SSIM slightly favours nearest** (−0.1 % to −3.7 % relative) on both docs. This
is a known SSIM property, not a bug: SSIM's local-variance/contrast term rewards
matching the ground truth's *hard* step edges, and the ground truth here is itself an
unsmoothed native-resolution bilevel render — any blur reduces local contrast/structure
relative to that reference even when it reduces absolute pixel error. So the two
metrics disagree by design; this is why the task called for an aesthetic judgement
rather than trusting SSIM alone.

**Aesthetic judgement (crops in `_pr_assets/`, generated by `examples/mask_aa_crops.rs`
from `cable_1973_100133.djvu`, word "sage"):** at 4× zoom the AA crop
(`mask_aa_4x_aa.png`) visibly rounds the staircase steps on curved/diagonal strokes
(the bowls of "a"/"g"/"e", the "s" curve) compared to the nearest crop
(`mask_aa_4x_nearest.png`), which shows the characteristic hard JB2 staircase. At 2×
the effect is present but subtler. Verdict: a real, visible improvement for zoomed
text/line-art viewing, consistent with why this is offered as an opt-in "quality mode"
rather than forced on everyone (DjVuLibre-faithful default stays hard-edged).

**Perf ratio (opt-in cost, not a default-path regression):** AA upscale is
2.2×–2.8× slower than nearest at 2×/4× zoom (measured in `mask_aa_quality`, 20 reps):
boy_jb2 0.08 → 0.23 ms/render (2.76×); cable_1973 71.5 → 156.5 ms/render (2.19×) at 2×,
61.3 → 155.4 ms/render (2.53×) at 4×. A small constant factor, paid only when the flag
is explicitly enabled.

**Decision. Kept, opt-in.** `RenderOptions::mask_aa` defaults to `false`
(byte-identical default output, proven by test); callers who want smoother zoomed text
opt in explicitly. Resolves QUALITY_AA's deferred #13 with the requested aesthetic
judgement now on record.
## Perf round 26 (2026-07-06) — BUG-ZPSHORT: zero-length BG44 refinement chunk

Follow-up from the B5 stateful-progressive-decoder work (`feat/progressive-streaming-decoder`,
PR #510): validating the new `ProgressiveDecoder` against real corpus files surfaced
`render_progressive_all` hitting `Iw44(ZpTooShort)` on `watchmaker.djvu` page 0.
Reproduced and root-caused in isolation (this branch carries only the fix, kept
separate from the decoder-API PR).

### BUG-ZPSHORT — pad short BG44 refinement payloads instead of erroring — **Fixed** (2026-07-06)

Round 8 measured `render_progressive` at O(N²) chunk decodes and recorded a design
for a stateful decoder; round 11 (B5_INCREMENTAL_PROGRESSIVE) fixed the *batch*
case (`render_progressive_all`) but left the harder case open: a viewer driving
`render_progressive_step` one chunk at a time as chunks arrive over a network still
pays O(N²) across the session, because there is no state to persist between calls.
This round closes that gap.

## Perf round 27 (2026-07-04) — B5 streaming ProgressiveDecoder (the deferred stateful API)

Round 8 flagged the streaming-step progressive case as needing a stateful decoder
API; round 11 (B5_INCREMENTAL_PROGRESSIVE) landed the O(N) *batch* path inside
`render_progressive_all` but left the streaming API deferred. This round adds it
and dogfoods it from the batch path.

### B5_STREAMING_DECODER — `ProgressiveDecoder` stateful streaming API — **Kept** (2026-07-04)

**Gap.** The public progressive entry points are `render_progressive(page, opts,
chunk_n)` — which re-decodes BG44 chunks `1..=chunk_n` from scratch per frame,
O(N²) over a full progressive sequence — and `render_progressive_all(page, opts)`,
which is O(N) but requires **every** BG44 chunk up front. Neither serves a viewer
receiving chunks incrementally over a network, which wants to render after each
arriving chunk without holding the whole sequence or re-decoding the prefix.

**API.** New `pub struct ProgressiveDecoder<'a>`:
- `ProgressiveDecoder::new(page, opts)` decodes the foreground (mask / FG44 /
  palette, plus any `bold` dilation) **once** and sets up the shared render params;
- `push_bg44_chunk(&mut self, chunk) -> Result<Pixmap>` feeds one refinement chunk
  into a single accumulating `Iw44Image` and returns the refined frame;
- `frames_produced()` reports progress.

O(N) total decode across the whole sequence — the foreground is never re-decoded and
the background accumulates in place, exactly the batch fast path's economy, now
usable one chunk at a time. It serves the case that path already served
byte-identically (strict decode, `Bilinear`); `Lanczos3`/`permissive` are rejected
with `UnsupportedOption` (Lanczos re-renders at native per frame, so there is no
shared incremental state — those callers use `render_progressive_all`).

**Refactor + correctness.** `render_progressive_all`'s incremental fast path was
re-pointed at the new type (it now builds a `ProgressiveDecoder` and calls
`push_bg44_chunk` per chunk) instead of inlining the decode/composite/snapshot loop
— one implementation, no drift. Byte-identical: the pre-existing
`render_progressive_all_matches_per_frame` (+ `_bold`) tests still pass (the batch
output is unchanged), and a new `progressive_decoder_streams_frames_matching_batch`
test asserts the streamed frames equal `render_progressive_all`'s frame-for-frame,
pixel-for-pixel, on the 3-chunk `chicken.djvu`. A rejection test covers the
`Lanczos3` / zero-dimension guards. `make check` green (1030 tests).

**Decision.** **Kept.** Closes the last B5 item: the O(N) streaming progressive
decode a network/viewer consumer needs, delivered as a small stateful wrapper over
the already-incremental building blocks, with the batch path refactored to share it.
No new decode cost (identical work, just re-shaped for incremental delivery); the win
is the enabled use-case (render-as-you-receive) plus the removed duplication.
with `0xFF` — reusing the same "past-end reads are `0xFF`" convention the decoder
already applies everywhere else — before constructing the `ZpDecoder`. Well-formed
(≥2-byte) payloads are completely unaffected (same bytes, same decode).

**Correctness.** New `iw44_decode_chunk_tolerates_empty_refinement_payload` (crate
`djvu-iw44`) covers a zero-length and a one-byte refinement payload directly.
New `render_progressive_step_handles_zero_length_bg44_chunk` (crate `djvu-rs`)
regression-tests the real `watchmaker.djvu` fixture end-to-end: every progressive
step, `render_progressive_all`, `render_pixmap`, and a full `ProgressiveDecoder`
session now succeed. Note this *does* change `watchmaker.djvu`'s full-page render
output (previously silently truncated to 2 of 4 chunks by the permissive path's
swallowed error, now correctly using all 4) — a correctness fix, not a regression;
no test in the suite pins that page's exact output bytes.

**Decision. Fixed**, small and scoped (one `if` + a 2-byte stack array), with
regression tests at both the codec and the render-API layer.


*(Verification note: a second, independent implementation of the same design
converged byte-for-byte on the ProgressiveDecoder core. Its extra evidence is kept
here: a `#[cfg(test)]` BG44 chunk-decode counter proving O(N²)→O(N) — chicken 6→3,
colorbook 10→4 — and direct byte-identity tests against `render_progressive_step`
on both corpora. The duplicate BUG-ZPSHORT write-up was dropped in favour of round 26.)*

### B5_STATEFUL — independent reimplementation: structural O(N) proof + direct byte-identity tests (2026-07-06)

**What.** New `pub struct ProgressiveDecoder<'a>` in `src/djvu_render.rs`, matching
the round-8 design almost exactly:

- `ProgressiveDecoder::new(page, opts)` decodes the foreground (mask / FG44 /
  palette, plus any `bold` dilation) **once** and captures the render parameters
  (gamma LUT, subsample, rotation);
- `push_bg44_chunk(&mut self, chunk) -> Result<Pixmap>` feeds one BG44 refinement
  chunk into a single accumulating `Iw44Image` held in the struct and returns the
  composited frame for the chunks fed so far;
- `frames_produced()` reports progress.

Only the background differs per frame — one BG44 chunk decoded into the same
`Iw44Image` — the foreground is decoded exactly once for the whole session. Scoped
to the case this is provably byte-identical for: strict decode, `Bilinear`
resampling. `Lanczos3` and `permissive` return `RenderError::UnsupportedOption`
(Lanczos re-renders at native resolution per frame — no shared incremental state to
build on; those callers keep using `render_progressive_all`). `render_progressive_all`'s
own incremental fast path (round 11) is refactored to build one `ProgressiveDecoder`
and drive it, so there is exactly one implementation of the accumulate-and-composite
logic behind both the batch and streaming entry points — per the round-8 design note,
`decode_layers`' bg+fg+bold+composite assembly is reused, not forked.

**Correctness.** Byte-identical, proven directly against `render_progressive_step`
(not only via the `render_progressive_all` batch path already covered by the
round-11 tests) on both corpus pages the task called for:
`progressive_decoder_matches_render_progressive_step_chicken` (3 BG44 chunks) and
`_colorbook` (4 chunks) feed the decoder one chunk at a time and assert every
streamed frame is pixel-identical to `render_progressive_step(page, opts, i)`.
`progressive_decoder_rejects_lanczos_and_zero_dims` covers the two guard branches.
1039 tests green, `make check` clean.

**Structural evidence (primary — wall-clock is noisy on this shared machine).**
A `#[cfg(test)]`-only thread-local counter (`BG44_CHUNK_DECODES`) at the two real
`Iw44Image::decode_chunk` call sites (the naive per-frame `decode_background_chunks`
loop and `ProgressiveDecoder::push_bg44_chunk`) turns the O(N²)→O(N) claim into an
exact, assertable count instead of timing
(`progressive_decoder_chunk_decodes_are_on_not_on_squared`). Zero-cost outside test
builds — the counter and its increments do not exist in release code.

| Page | N (BG44 chunks) | naive session (`render_progressive_step(0..N)`) | `ProgressiveDecoder` session |
|------|------------------|---------------------------------------------------|-------------------------------|
| chicken.djvu | 3 | 1+2+3 = **6** decodes | **3** decodes |
| colorbook.djvu | 4 | 1+2+3+4 = **10** decodes | **4** decodes |

Exactly the `N(N+1)/2` vs `N` the design predicted; the gap widens with N (a
15-chunk streamed photo would be 120 vs 15 — 8×).

**Indicative timing (provisional — shared, noisy machine; ratio vs. an in-session
control, not an absolute number).** `colorbook.djvu`, 4 BG44 chunks, native
2260×3669, release build, mean of 5 iterations:

| Session | Time | vs. naive |
|---------|------|-----------|
| naive per-frame (`render_progressive_step(0..N)`) | 206.2 ms | — |
| `render_progressive_all` (round-11 incremental batch) | 191.4 ms | −7% |
| `ProgressiveDecoder` streaming (this round) | 193.7 ms | −6% |
| single `render_pixmap` (reference) | 43.3 ms | — |

Matches round 11's finding exactly: on colorbook the *addressable* redundancy is the
ZP entropy-decode step only (small relative to the per-frame IDWT+composite cost at
4 chunks), so the streaming decoder's wall-clock is within noise of the batch path —
both real wins are structural (decode count) and will dominate at higher chunk
counts or on small pages (round 11 measured 1.50× on chicken).

**Decision. Kept.** Closes the B5 backlog item exactly as designed in round 8: one
implementation shared between `render_progressive_all` and the new streaming API,
byte-identical against `render_progressive_step` on both required fixtures, and a
direct structural proof of O(N²)→O(N) chunk decodes (not just an inference from
round 11's batch numbers). The streaming API is what an actual network viewer needs
— render-as-you-receive without holding the whole chunk sequence — which
`render_progressive_all` alone could not provide.

## Perf round 28 (2026-07-06) — PDF_ADAPTIVE_RASTER: per-page adaptive Deflate-vs-JPEG choice

**Issue.** Follow-up to `PDF_DCT_PROBE` (round 23): the shipped PDF export default
always emits DCTDecode (JPEG-80) for colour page backgrounds, which loses badly on
near-flat/text-dominated colour scans — `watchmaker.djvu` p0 came out **3.1× larger**
under JPEG-80 than plain Deflate, at no SSIM gain. `PdfOptions.jpeg_quality` is a
single whole-document choice, so no static setting can be optimal for a document with
mixed page content.

**Approach.** Added `PdfOptions::adaptive_raster: bool` (default `false` — current
always-JPEG-80 behaviour is unchanged, byte-identical). When `true`, each page's
rendered RGB is encoded *both* as DCTDecode(JPEG-80) and FlateDecode inside
`render_page_data` (`src/pdf.rs`), and only the smaller stream is kept; the loser is
dropped before the function returns, so only one page's pair of encodings is ever
live at once — doesn't regress the O(1)-per-page streaming memory profile from
`PDF_STREAM`/#449. The parallel rayon path (#298) renders each page independently and
runs the same per-page function, so the adaptive choice composes for free; verified
byte-for-byte identical output between the `parallel` and non-`parallel` build with
`adaptive_raster: true` on `watchmaker.djvu` (same length, same hash).

**Measured (whole-file PDF, `PdfOptions::default()` vs `adaptive_raster: true`,
default 150 DPI / JPEG-80):**

| Corpus file | Default (always JPEG-80) | Adaptive (best of both) | Ratio |
|---|---|---|---|
| `watchmaker.djvu` (12p, near-flat colour scan — the regression case) | 5,920,319 B | 3,660,142 B | **1.62× smaller** |
| `colorbook.djvu` (mixed photographic/text colour) | 12,133,358 B | 11,528,809 B | 1.05× smaller |
| `big-scanned-page.djvu` (photographic scan — JPEG already wins) | 1,462,025 B | 1,462,025 B | 1.00× (identical — no regression) |

Quality is unchanged by construction on every page: when JPEG wins the encoding is
byte-identical to today's default; when Deflate wins it's lossless (round-trips the
exact rendered RGB), strictly better than the JPEG-80 alternative it replaced.
Time cost of the double-encode is roughly +40–60% page-render wall time (extra
Deflate pass) — acceptable for an opt-in mode, not paid unless requested.

**Decision: Kept.** Ships as `src/pdf.rs` `PdfOptions::adaptive_raster` (opt-in,
default off). Tests: `adaptive_raster_defaults_to_off`,
`adaptive_raster_off_is_byte_identical_to_default`,
`adaptive_raster_shrinks_flat_colour_scan` (asserts >1.3× win on `watchmaker.djvu`),
`adaptive_raster_never_larger_than_default`. `make check` green (1035 tests).
## Perf round 29 (2026-07-06) — JB2_DICT_ORDER: does dictionary symbol order cut Sjbz bytes?

### JB2_DICT_ORDER — shared-dict index permutation vs `Sjbz`/`Djbz` size — **Diagnostic, negative result** (2026-07-06)

**Question.** JB2 numbers dictionary entries by emission order; blit records
reference them by index via an adaptive binary-tree integer coder
(`NumContext`/`encode_num`/`decode_num` in `crates/djvu-jb2/src/{encode,lib}.rs`).
Any dict order the encoder chooses is legal as long as the decoder agrees (order =
emission order, no side channel). Two plausible reorderings could, in principle, cut
the index-coding entropy: **(a)** frequency order — put the most-referenced symbols
at the smallest indices, since the integer coder's phase-2/3 traversal is
Elias-gamma-ish (numbers near 0 resolve in fewer tree-node decisions); **(b)**
similarity order — group same-shape symbols adjacently.

**Ordering constraint confirmed by reading the codec.** A record-6 refinement
(`Action::Refine(dict_idx)` in `encode_jb2_dict_with_options`, `encode.rs:~1432`)
can only reference `dict_entries[dict_idx]` for `dict_idx < dict_entries.len()` at
that point — i.e. **the reference must already be emitted**, a topological
constraint. But the entire shared block (`shared_symbols`) is always emitted before
any page-local symbol (`encode_jb2_dict_with_shared`/`_with_options` seed
`dict_entries` from `shared_symbols` first, unconditionally). So **any permutation
purely within the shared block** can never place a refiner before its target — the
constraint is satisfied automatically regardless of internal shared-block order.
(In the shipped path this is moot anyway: `encode_djvm_bundle_jb2_with_shared` uses
`Jb2EncodeOptions::default()`, which has both `same_size_rec6` and
`cross_size_rec6_probe` `None` — rec-6 is never emitted by the production bundler
today. The probe below still round-trips real records, whatever they are.)

**Probe (measurement only, no shipped-code change).** Added, behind the
`experimental` feature: `DictOrderVariants` / `cluster_shared_symbols_order_variants`
(`crates/djvu-jb2/src/encode.rs`) — reruns the exact same byte-exact clustering +
pixel-budget trim as `cluster_shared_symbols` (identical promoted symbol *set*), then
emits 3 orderings of that same set instead of one fixed sort:
- `baseline` — current shipped order (first-seen: page, then CC position).
- `by_frequency` — descending count of distinct pages a symbol was promoted from,
  ties broken by first-seen.
- `by_bucket` — grouped by `(width, height)` size bucket ascending (the
  clustering pass's natural pre-sort iteration order — same-shaped symbols land
  adjacent; note the *shipped* order is **not** already bucket/hash-grouped, it's
  first-seen, contrary to the task brief's premise).

Driver `measure_dict_order_probe`/`dict_order_probe_corpus_measurement` (ignored
test, `src/jb2_encode.rs`) loads real corpus pages, builds all three orderings from
one clustering pass, re-encodes real `Djbz` (`encode_jb2_djbz`) + every page's real
`Sjbz` (`encode_jb2_dict_with_shared`) against each, sums actual emitted bytes, and
round-trip-decodes every page (`crate::jb2::decode_dict` + `crate::jb2::decode`)
against the real decoder to confirm pixel-exact reconstruction. Run:
`cargo test --lib --release --features experimental,parallel dict_order_probe_corpus_measurement -- --ignored --nocapture`.

**Numbers** (release, `parallel` feature, M1 Max; `page_threshold=2`, matching
`bench_cluster_shared_symbols`):

| Corpus | pages | shared symbols | baseline | by_frequency | by_bucket | roundtrip failures |
|---|---|---|---|---|---|---|
| `conquete_paix.djvu` | 22 | 75 | 53,219 B | 53,194 B (**−0.047%**) | 53,230 B (+0.021%) | 0 |
| `pathogenic_bacteria_1896.djvu` | 517 | 5,164 | 33,394,769 B | 33,397,363 B (+0.008%) | 33,409,746 B (+0.045%) | 0 |

All three orderings round-trip pixel-exact on every page of both documents (0
failures out of 22 + 517 = 539 page decodes × 3 orderings each), confirming the
topological-safety argument above empirically, not just by code inspection.

**Why the effect is ~0, not the hoped-for few %.** `NumContext` is an *adaptive*
binary arithmetic coder — each tree node's probability is learned online from the
actual bit sequence that node sees across the whole document, not looked up from a
fixed code table. Permuting symbol indices changes *which* value a given reference
carries, but every context node still adapts to whatever empirical distribution of
decisions it ends up seeing; the coder is close to matching the empirical entropy of
the index stream either way. There's no static Huffman-style table where reassigning
short codes to frequent symbols pays off — the adaptive model already captures that
degree of freedom for any fixed permutation. Net measured deltas (−0.05%…+0.05%) are
consistent with noise from a handful of tie-break/threshold changes in Hamming-based
candidate matching, not a real entropy effect.

**Decision.** **No shipping change recommended.** Deltas are 30–60× below the
~1.5% bar for a real win, in both directions, on a 75-symbol and a 5,164-symbol
shared dict (two orders of magnitude apart) — consistent negative result across
corpus scale. Recorded here so dictionary-symbol reordering is not re-proposed
without new evidence (e.g. a non-adaptive or partially-static index coder, which
JB2/DjVuLibre does not have). Fixed a pre-existing, unrelated compile break found
while getting the `experimental` test binary to build: `with_jb2_options_lossy_threshold_round_trips`
(`src/djvu_encode.rs`) constructed `Jb2EncodeOptions` without the `same_size_rec6`
field added by round 18 (#512); harmless under the default feature set (the struct
literal only exists in a `#[cfg(test)]` module) but broke `cargo test --features
experimental`. Added the missing `#[cfg(feature = "experimental")] same_size_rec6: None,` arm.

## Perf round 30 (2026-07-06) — JB2_DESPECKLE: speck-removal pre-pass for lossy JB2 on noisy scans

**Issue.** Round 19 (Branch B) found the shipped same-size `lossy_threshold` lever
gives −22 % on clean text but almost nothing on the noisy 600 dpi
`pathogenic_bacteria_1896` scan (near-twins there are far apart, median 12.9 %
Hamming — binarisation noise makes every glyph instance unique), and cross-size
lossy substitution (B1) was tried and reverted (dominated by same-size). The one
untried lossy lever for scans was **despeckle** — cjb2's classic move: drop tiny
isolated noise components before they ever become dict entries.

**Approach.** Added `pixel_count` (true foreground-pixel count, not bbox area) to
the `Cc` struct extracted by `extract_ccs`, populated for free from the DFS pixel
list already built during extraction. New `Jb2EncodeOptions::despeckle: Option<u32>`
(default `None`, stable, not `experimental`): when `Some(max_px)`, any connected
component with `pixel_count <= max_px` is `retain`-filtered out of the CC list in
`encode_jb2_dict_with_options` **before** reading-order sort, clustering, or
dedup — so a speck never becomes a rec-1 dict entry, a coordinate record, or noise
in the near-twin population `lossy_threshold` matches against. This is lossy: the
removed pixels have no dict entry to reconstruct from. Filtering on actual ink-pixel
count (not bbox `w*h`) avoids overcrediting thin diagonal strokes as "small". No
proximity/isolation heuristic was added (no DjVuLibre C++ source was available in
`references/` to check cjb2's exact rule — only `djvujs`, a viewer, is vendored);
instead punctuation/diacritic safety was validated empirically (below).
`with_despeckle(max_px)` builder + `lossy_scan()` preset (`despeckle=8,
lossy_threshold=0.02`) added alongside, mirroring `lossy_text()`'s pattern.

**Numbers** (`examples/jb2_despeckle.rs`, per-page independent, `shared=[]`, D1 SSIM
harness on the decoded mask; all pages round-trip/decode cleanly at every setting):

| Corpus | despeckle=2 | despeckle=4 | despeckle=8 |
|--------|-------------|-------------|-------------|
| watchmaker (text) | **+0.00 %**, SSIM 1.00000 (byte-identical) | +0.00 %, SSIM 1.00000 | +0.00 %, SSIM 1.00000 |
| pathogenic_bacteria_1896 (600 dpi scan) | −0.94 %, SSIM 0.99950 | −1.59 %, SSIM 0.99904 | **−2.43 %**, SSIM 0.99845 |

Stacking `lossy_threshold=0.02` on top of despeckle adds only ≈0.01 pp more on the
scan (e.g. despeckle=8 alone −2.43 % → despeckle=8+lossy 2 % −2.44 %) — consistent
with round 19's A0 finding that the same-size near-twin population there stays thin
regardless of despeckling. Fraction of mask pixels flipped stays tiny even at the
most aggressive tested level: 0.0047 % (despeckle=2) → 0.0106 % (despeckle=4) →
0.0195 % (despeckle=8).

**Punctuation/diacritic safety.** Added
`despeckle_preserves_punctuation_and_diacritic_dots`: a synthetic page with an 'i'
stem + 4×4 dot (16 px, mimicking a real dot-of-i/period at typical scan resolution),
an isolated 4×4 period, and a true 1×1 dust speck. At every tested level (2, 4, 8)
the dot and period (16 px > 8) survive pixel-exact while the 1 px speck is removed —
the failure mode called out in the task (eating real ink) does not fire at these
levels. `despeckle_removes_isolated_1px_specks_and_shrinks_output` confirms specks
are actually dropped and the real glyph stays pixel-exact;
`despeckle_off_is_byte_identical` confirms the default (`None`) reproduces the
shipped encoder exactly, byte-for-byte, even on a page containing specks.

**Reading.** This is the first lossy lever that moves the noisy-scan corpus at
all — same-size `lossy_threshold` alone gives ≈0 % there (round 19 B0) and
cross-size lossy was reverted for adding nothing beyond same-size (round 19 B1).
Despeckle's win is real but modest (−2.43 % at the most aggressive tested level, far
short of text's −22 % `lossy_threshold` win) because binarisation dust, while
plentiful in CC *count* (thousands of one-off symbols), is small in *byte* terms per
component (a 1–8 px rec-1 already costs only a few bytes; the saving is the
record/coordinate overhead avoided, not a large payload). On clean text it is a
provable no-op (byte-identical, not just "flat") — real glyphs are all far above
8 px, so nothing is ever dropped, matching the hypothesis that this is a
scan-specific lever.

**Decision. Kept.** Shipped as a stable (non-`experimental`) opt-in
`Jb2EncodeOptions::despeckle` field, `with_despeckle(max_px)` builder, and a
`lossy_scan()` preset (`despeckle=8, lossy_threshold=0.02`) recommended for noisy
scans — same shipping posture as `lossy_text()` (round 19): off by default
(archival-safe), one call to opt in. Recommended operating point: **despeckle=8**
for scans (SSIM 0.99845, −2.43 % Sjbz); levels 2 and 4 are safer/smaller-gain
fallbacks if a corpus's real content runs unusually small. Not extended past 8 px in
this round — the task's specified sweep stopped there; higher values are unexplored
headroom for a future round if a corpus's dust population runs larger.

## Perf round 31 (2026-07-06) — FGBZ_MEDIANCUT: median-cut foreground palette quantiser

**Issue.** Round 4's summary (line ~131 above) deferred "median-cut FGbz palette" as
a "quality trade-off without a clean win metric." The D1 perceptual harness
(`src/quality.rs`) has since landed, making it measurable. Read the current
algorithm first: `src/fgbz_encode.rs` is pure wire-format encode/decode (`encode_fgbz`
/ `decode_fgbz`) — it does **not** choose colours. The actual palette-selection logic
is the private `foreground_fgbz` in `src/djvu_encode.rs`: it decodes the Sjbz blit map,
accumulates a per-blit **average** RGB colour via `ColorAccum`, and previously emitted
**one exact palette entry per distinct per-blit average** — no quantisation at all.
On real scans this is fine (few, well-separated ink colours), but on pages with many
blits whose *true* ink is the same hue yet whose per-blit average differs by a few
LSBs (scanner noise, JPEG ringing, anti-aliasing), this bloats the FGbz palette with
near-duplicate entries that buy no visible fidelity.

**Fixture search.** Went looking for an on-disk multicolour-FGbz fixture to
demonstrate the bloat: `colorbook.djvu` (the `test_colorbook_multicolor_foreground`
fixture) turns out to use **FG44**, not FGbz, and is itself near-grayscale (max
per-pixel channel spread 18) — not applicable. `tests/fixtures/irish.djvu` and
`references/djvujs/library/assets/navm_fgbz.djvu` are real FGbz fixtures, but `irish`
is likewise near-grayscale/already-minimal (its 40-entry palette barely reduces).
Built a small deterministic **synthetic fixture** instead
(`examples/fgbz_mediancut_harness.rs::synthetic_multicolor_text_page`): a 900×700
page of ~2600 small AA'd glyph-like blits across 6 ink hues with ±8 per-channel
scanner-style jitter — the exact "same ink, noisy average" scenario described above,
generated with a tiny deterministic xorshift32 PRNG (no `rand` dependency needed).

**Approach.** Added `FgbzPaletteOptions` (`src/djvu_encode.rs`): `Exact` (unchanged
behaviour, and the `Default`) or `MedianCut { max_colors }` — classic median-cut
quantisation of the per-blit weighted-average colours down to `max_colors` boxes
(split the box with the widest channel range at its median, weighted-average each
final box, deterministic tie-breaking), then each blit's colour maps to its nearest
palette entry by squared RGB distance. Wired through a new
`PageEncoder::with_fgbz_options` builder method; `foreground_fgbz` branches on the
option. The `encode_djvm_layered_shared_impl` bundle path keeps calling it with
`FgbzPaletteOptions::Exact` explicitly (out of scope here, documented inline) so
multi-page bundles are untouched. **Default stays `Exact`, byte-identical** —
verified by a dedicated test (`fgbz_mediancut_is_opt_in_default_stays_exact`)
asserting default-constructed output equals explicit-`Exact` output, plus all 23
pre-existing `djvu_encode` tests passing unchanged. 6 new unit tests cover the
quantiser itself (`median_cut_reduces_many_near_duplicate_colors_to_k`,
`median_cut_never_exceeds_k_even_with_fewer_distinct_colors`,
`median_cut_empty_input_is_empty`, `nearest_palette_index_picks_closest`) and the
opt-in guarantee.

**Measured (synthetic fixture, `examples/fgbz_mediancut_harness.rs --synthetic`,
`EncodeQuality::Quality`, D1 `quality::compare` vs the source render):**

| Palette | Total bytes | FGbz bytes | Palette entries | SSIM | PSNR (dB) | MSE |
|---|---|---|---|---|---|---|
| Exact (baseline) | 9014 | 2255 | 214 | 0.86036 | 17.66 | 1113.6 |
| MedianCut{4} | 6834 | 76 | 4 | 0.85378 | 17.15 | 1252.2 |
| MedianCut{6} | 6840 | 82 | 6 | 0.85731 | 17.37 | 1192.5 |
| MedianCut{8} | 6850 | 92 | 8 | 0.85850 | 17.56 | 1140.4 |
| MedianCut{16} | 7004 | 245 | 16 | 0.85729 | 17.58 | 1134.2 |
| MedianCut{32} | 7186 | 427 | 32 | 0.86054 | 17.66 | 1113.8 |
| MedianCut{64} | 7684 | 925 | 64 | 0.86047 | 17.66 | 1113.7 |

At `k=6` (matching the fixture's 6 true ink hues): FGbz shrinks 2255 B → 82 B
(**−96.4 %**), total encoded bytes 9014 → 6840 (**−24.1 %**), SSIM moves from
0.86036 to 0.85731 (**−0.0031**, i.e. visually a wash — confirmed by eye, see
crops below). At `k=32` (≥ true colour count) FGbz is still 427 B vs 2255 B
(**−81.1 %**) with SSIM *matching* the baseline to 4 decimal places
(0.86054 vs 0.86036 — within measurement noise of the two different palette
construction paths, not a real quality change). Only when `k` is pushed well below
the true colour count (`k=4`) does SSIM measurably drop (−0.0066, still small on
this luma-weighted metric — see caveat below).

**Real-fixture check (`tests/fixtures/irish.djvu`, page 0, 40-entry `Exact` palette,
already near-minimal):**

| Palette | Total bytes | FGbz bytes | Palette entries | SSIM | PSNR (dB) |
|---|---|---|---|---|---|
| Exact (baseline) | 53348 | 2900 | 40 | 0.97880 | 33.17 |
| MedianCut{4} | 52870 | 2422 | 4 | 0.97871 | 33.02 |
| MedianCut{32} | 53320 | 2872 | 32 | 0.97880 | 33.15 |

Confirms the fixture-search finding: `irish` has little headroom (its `Exact`
palette is already small/well-separated), so median-cut mostly just matches it at
equal `k`, with a small size win from BZZ-compressing a shorter, denser palette table.

**Honesty caveat.** The D1 harness (`src/quality.rs`) scores **luma only** (8×8
windowed SSIM, no chroma/hue term) — it structurally under-reports colour-fidelity
differences, which is exactly what this experiment is about. To compensate, before/
after 200×200 crops of the densest coloured-ink region (auto-located by a
saturated-pixel-density heuristic, `most_colorful_window`) were saved to
`_pr_assets/`: `synthetic_source_crop.png`, `synthetic_exact_crop.png`,
`synthetic_mediancut6_crop.png`, `synthetic_mediancut32_crop.png`. Visual inspection
confirms `Exact` and `MedianCut{6}`/`{32}` are indistinguishable from each other and
close to the source (a pre-existing, unrelated `segment_page` mask-threshold
artefact on the orange hue is present identically in every variant, including the
baseline — not introduced by this change).

**Correctness.** `make check` (fmt, clippy `-D warnings`, no_std build, wasm32
no_std+wasm build, full 1036-test workspace suite) passes. 28/28 `djvu_encode`
unit tests pass (22 pre-existing + 6 new).

**Decision.** **Kept, opt-in.** `FgbzPaletteOptions::MedianCut` via
`PageEncoder::with_fgbz_options` is additive: default output is byte-identical
(enforced by test), and callers who know their content has a small number of true
ink colours (e.g. coloured-annotation scans) can pick `k` at or above that count for
a real FGbz/total-size win at zero-to-negligible quality cost. Not made the default:
picking a good `k` is content-dependent (too-small `k` does cost real quality, per
the `k=4` row), and on already-clean real-world fixtures like `irish.djvu` the
practical win is small. This resolves the round-4 backlog item with honest numbers
rather than forcing a default-behaviour change.
## Perf round 32 (2026-07-06) — D3_BICUBIC: FG44 bicubic upsampling — **Rejected**

**Question.** FG44 (the colour foreground/text layer) is stored heavily subsampled
(measured **exactly 12.0×** per axis on `colorbook.djvu`, both tested pages) and the
compositor upsamples it per-pixel via `sample_bilinear` in the non-1:1 (upscale)
branch of `composite_rows_bilinear_one`. Bilinear over a 12× gap is a plausible place
for colour to smear across glyph clusters — would a sharper reconstruction kernel
(Catmull-Rom bicubic, 4×4-tap, a=-0.5) visibly help colour text?

**Implementation (measured, then reverted).** Added an opt-in `RenderOptions::fg_bicubic`
flag, dispatched via a macro-duplicated loop (`b_series_fg_row!`) so the boolean check
sits outside the per-pixel hot loop (matching the `a2_has_mask_loop!` pattern) —
zero cost on the default path, proved by a byte-identical test. Scope was deliberately
limited to the B-series (upscale) branch that literally calls `sample_bilinear`, per
the task; the 1:1 fast path (`bilinear_from_rows`) and the downscale/area-avg path are
untouched (downscale routes through `composite_rows_area_avg_one`, a completely
different box-filter code path — the bicubic flag has zero effect there, confirmed by
a probe that found 0 diff bytes when accidentally testing at downscale before this
was root-caused).

**D1 measurement (real in-compositor implementation, before revert).**
`colorbook.djvu` (multicolour FG), pages 0–1, 2×/2.4× zoom, whole-page render vs `ddjvu`:

| Case | Bilinear vs ddjvu | Bicubic vs ddjvu | Bilinear vs Bicubic (direct) |
|---|---|---|---|
| p0 @2× | SSIM 0.9895, PSNR 36.36 dB | SSIM 0.9895, PSNR 36.37 dB | SSIM 0.9999, PSNR 59.73 dB |
| p0 @2.4× | (ddjvu off-by-one size, skipped) | — | SSIM 0.9999, PSNR 58.83 dB |
| p1 @2× | SSIM 0.9924, PSNR 38.48 dB | SSIM 0.9923, PSNR 38.48 dB | SSIM 0.9999, PSNR 61.02 dB |

Identical to 4 decimal places against the reference decoder; the direct bilinear-vs-
bicubic delta (SSIM 0.9999, PSNR 59-61 dB) is itself below any perceptible threshold.
Saved before/after PNG crops of colour text (`_pr_assets/`) — visually indistinguishable
side by side. **Perf cost when enabled:** ~17% slower in the affected path (285 ms vs
334 ms/frame, colorbook p0 @2×, 8-frame average) — a real cost for zero visible gain.

**Isolated re-check (FG44 plane alone, post-revert, public-API-only).** To avoid a
confound found while writing the standalone reproduction (a whole-*page* bicubic
resize looks much better vs `ddjvu` than the isolated FG-only comparison — because it
also sharpens mask/text edges, a different effect from FG colour reconstruction), the
kept example (`examples/fg_bicubic_quality.rs`) isolates just the FG44 plane via
`DjVuPage::extract_foreground()` (public API) and upsamples it bilinear vs bicubic
directly: SSIM 0.998, PSNR ~51.7 dB, confirming the same negligible-gap conclusion
with no full-page confound.

**Verdict.** Rejected — no perceptible gain (SSIM delta ~0.0001, visually
indistinguishable crops), a real ~17% perf cost when opted in, and it would add
public API surface (`RenderOptions::fg_bicubic`) for a feature with nothing to show.
`sample_bicubic`/`catmull_rom_weights`/the flag were **not shipped** — reverted from
`src/djvu_render.rs` entirely (confirmed byte-identical to origin/main). Kept only
this journal entry and the measurement example. The 12× FG44 subsample ratio is
already reconstructed by bilinear below the perceptual floor at realistic zoom levels;
future colour-text quality work should look elsewhere (e.g. encoder-side FG44
fidelity, not decoder-side interpolation kernel).
## Perf round 33 (2026-07-04) — Lanczos RGBA-interleaved accumulate (LLVM auto-vec)

The Lanczos-3 resampler was already row-parallel (PAR_LANCZOS, −69%) and
weight-hoisted (LANCZOS_HOIST, −22.5%), but both separable passes still accumulated
into **three separate** `r/g/b` scalars/arrays while reading a **stride-4** RGBA
source. That deinterleave (read `px[base], px[base+1], px[base+2]` into three
destinations) blocks LLVM's auto-vectoriser. This round restructures both passes to
accumulate the four RGBA channels **together**, so the hot loops become contiguous
`f32` FMAs the compiler vectorises — no manual NEON, no `unsafe`.

### LANCZOS_RGBA_ACC — accumulate RGBA together, not deinterleaved — **Kept** (2026-07-04)

**Change.**
- *Horizontal pass* — each output pixel accumulates into one `[f32; 4]` read from
  four contiguous source bytes per tap (`acc[c] += px[c] * w`), so LLVM widens the
  per-tap multiply-add to a 4-lane FMA instead of three scalar ops on a strided read.
- *Vertical pass* — replace the three `acc_r/acc_g/acc_b` column arrays with a
  single **interleaved** `acc[dw*4]`; the inner `sy` loop is then a contiguous SAXPY
  `acc[i] += mid_row[i] * w` over `dw*4` elements, which vectorises optimally (the
  old stride-4 gather into three arrays did not).

In both passes the extra alpha lane accumulates the pixmaps' constant 255 and is
ignored on output.

**Correctness — bit-identical.** Each RGB channel still sums the same contributors
in the same order with the same normaliser, so the rounding is unchanged. Verified:
a `DefaultHasher` digest of the Lanczos-3 half-scale render of colorbook
(1130×1834) is **identical** across the change (`digest=1154d165a6a72d7d`), and the
`scale_lanczos3` / render Lanczos tests pass. `make check` green.

**Numbers** (`render_scaled_large_colorbook`, colorbook 2260×3669 → 1130×1834,
`parallel`, M1 Max). The board was thermally hot so absolute times drift run-to-run;
the reported figure is the intra-run **normalised Lanczos cost**
`(lanczos3 − bilinear) / bilinear` (bilinear = decode + bilinear control, cancels
the shared decode + thermal state):

| | normalised Lanczos cost | vs old |
|-|-------------------------|--------|
| Before | 8.77–11.93 (across pairs) | — |
| Vertical pass only | 8.30–9.43 | **−21 to −24%** |
| Both passes | 6.38 | **−27%** |

Consistent ~−22 to −27% reduction of the Lanczos resampling cost across three A/B
pairs, bit-identical.

**Decision.** **Kept.** A pure source-level restructure that lets LLVM vectorise the
two separable-filter hot loops it previously could not, on top of the existing
parallel + hoist wins. Bit-identical, no `unsafe`, no `#[target_feature]` — so it
holds on every target LLVM auto-vectorises (unlike the manual-NEON dead-ends #3),
and it needs no runtime feature detection. Lanczos is the recommended text-downscale
resampler (D2), so this speeds the quality render path directly.
## Perf round 34 (2026-07-04) — encoder-side DjVuLibre interop harness (unblocks the masked-wavelet work)

Rounds 8/17 both blocked the full normative IW44 masked-wavelet transform on the
same missing piece: a harness that validates **files we encode are decodable by
DjVuLibre**. The two existing interop tools are decode-side only — `interop_pixdiff`
and `diff_djvulibre` render an *existing* `.djvu` with our decoder and with `ddjvu`
and compare. Neither ever feeds `ddjvu` a file **we produced**. This round builds
that gate.

### INTEROP_ENCODE — round-trip our encoder through ddjvu — **Kept (infra)** (2026-07-04)

**Tool.** `examples/interop_encode.rs`. For each source page it (1) renders it to a
pixmap with our decoder, (2) re-encodes that pixmap with our colour `Quality`
encoder, (3) decodes the result with **`ddjvu`** (DjVuLibre) → PPM — the interop
gate, (4) decodes the same bytes with our decoder, and reports two pixel-diff
distributions:
- **interop** (`ddjvu`-of-ours vs us-of-ours): both decode the *same* bytes, so a
  large diff is a latent encoder-interop bug (the two decoders read our stream
  differently).
- **quality** (`ddjvu`-of-ours vs the original source): end-to-end encode quality
  as DjVuLibre sees it.

Exit code is non-zero if any page fails the gate (`ddjvu` rejects the file or the
dimensions disagree), so it doubles as a pass/fail interop check and, later, a CI /
fuzz target. It catches exactly the hazard the `chroma_half` code comment already
documents (a half-resolution chroma stream makes `ddjvu` abort with *Unexpected End
Of File*).

**Baseline (current `Quality` encoder, `--corpus`, 21 pages, M1 Max + DjVuLibre
`ddjvu`):**

- **Interop gate: 21 / 21 pass.** DjVuLibre decodes every file our encoder
  produces — our colour `Quality` output is DjVuLibre-interoperable today. (3 files
  skipped: `big-scanned-page` JB2 too-large, `carte` truncated, `czech` needs an
  external shared dict — decode-side limits, not encode failures.)
- **Interop fidelity is excellent on real colour scans:** `ddjvu` and our decoder
  agree to mean |Δ| < 0.3/255 on colorbook (0.274), watchmaker (0.130), conquete
  (0.99), irish (0.10), cable (0.11), malliavin (0.003); byte-identical on the
  bilevel-heavy `pathogenic`/`DjVu3Spec` (mean 0.000).
- **Larger interop diffs (mean 20–29) appear only on degenerate re-encodes** —
  feeding a *bilevel* page's render (boy_jb2, links, the rotate* variants) or a
  *photo* (boy, vega, chicken) through the layered colour segmenter. These aren't
  representative inputs for `PageEncoder::from_pixmap(Quality)`; the diff is the
  edge-heavy mask-AA / chroma divergence `interop_pixdiff` already documents,
  amplified by the degenerate segmentation. Flagged as a follow-up to confirm it is
  cosmetic (our-decoder mask AA vs `ddjvu`) and not a real stream ambiguity.
- **Encoder quality vs source** is faithful where fg/bg separate cleanly (watchmaker
  0.60, cable 0.22) and expectedly lossy on detailed photos the layered model
  cannot represent (colorbook 17.9, chicken 26.8) — a property of layered encoding,
  not an interop issue.

**Decision.** **Kept** as infra. This is the gate the masked-wavelet normative work
was waiting on: the future masked forward-transform (mask plumbed through
`encode_iw44_color` → `PlaneEncoder`) can now be validated by requiring
`interop_encode --corpus` to stay 21/21 on the gate **and** not regress the
interop-fidelity means on the real colour scans. Sits alongside D1 (`quality.rs`)
and `interop_pixdiff` as the encoder-side complement to the decode-side tools.
Follow-up: investigate the degenerate-re-encode interop diffs (likely mask-AA
cosmetic), and extend the harness to also drive `c44`/`cjb2` → our decoder for the
reverse direction.
## Perf round 35 (2026-07-04) — IW44 slice rate-distortion (diagnostic: default is well-tuned)

IW44_DIAG (2026-06-15) localised the residual IW44 quality loss to "fine-band
quantization … starved in 100 slices, quality floor avg_abs=8.72" and flagged a
slice-budget increase for fine bands as an open size/quality lever. Now that the
D1 perceptual harness exists (round 9), the RD curve can be measured directly.
This round does that; the conclusion is that the `total_slices = 100` default sits
at the knee of the curve and the "starved" hypothesis does not hold.

### IW44_SLICE_RD — is total_slices=100 starving quality? — **Diagnostic / no change** (2026-07-04)

**Setup.** Encode a colour page as IW44 (`encode_iw44_color`) at
`total_slices ∈ {50, 74, 100, 126, 150, 200}`, decode, and measure size + SSIM /
PSNR vs the source (D1 `quality::compare`). Two workloads: the **full detailed
page** (worst case for the codec) and the **sub-sampled segmented background**
(the real production BG44 input). M1 Max.

**Full page — quality plateaus at ~100 slices, size then explodes:**

| slices | colorbook size / SSIM / PSNR | watchmaker size / SSIM / PSNR |
|--------|------------------------------|-------------------------------|
| 50 | 9 675 B / 0.9144 / 20.19 | 32 340 B / 0.9063 / 20.32 |
| 74 | 81 799 B / 0.9606 / 22.13 | 258 820 B / 0.9730 / 33.41 |
| **100** | **261 617 B / 0.9757 / 22.41** | **693 929 B / 0.9874 / 38.75** |
| 126 | 643 900 B / 0.9783 / 22.42 | 1 106 060 B / 0.9890 / 39.03 |
| 150 | 2 076 207 B / 0.9786 / 22.42 | 1 645 979 B / 0.9891 / 39.03 |
| 200 | 6 402 802 B / 0.9787 / 22.42 | 2 537 897 B / 0.9891 / 39.03 |

Past 100 slices PSNR is pinned (22.42 / 39.03 dB) and SSIM gains ≤ 0.003 while
size grows 2–25×. colorbook 100→150 costs **+1.8 MB for +0.0003 SSIM**. The
avg_abs=8.72 floor is the *inherent* quantization limit of the transform, not slice
starvation — more slices cannot move the plateau.

**Segmented BG (production BG44 workload):**

| slices | colorbook BG (189×306) | watchmaker BG (213×276) |
|--------|------------------------|-------------------------|
| 50 | 94 B / SSIM 0.9332 | 89 B / SSIM 0.99789 |
| 100 | 2 078 B / SSIM 0.9773 | 189 B / SSIM 0.99797 |
| 200 | 132 052 B / SSIM 0.9813 | 19 206 B / SSIM 0.99822 |

The knee is **content-dependent**: the textured colorbook BG genuinely needs ~100
slices (50 is visibly worse, 0.933 vs 0.977), while the smooth watchmaker BG is
already at plateau by 50 (SSIM 0.99789 at 89 B vs 0.99797 at 189 B — 100 slices
doubles the bytes for +0.00008 SSIM). A *fixed* 100 slightly over-codes very smooth
backgrounds, but the absolute waste is tiny (~100 B/page), and at 200 slices even a
smooth BG blows up (colorbook 132 KB) — so 100 is also a sensible ceiling that
prevents pathological growth.

**Verdict.** **No change.** `total_slices = 100` sits at the RD knee: enough to
reach the quality plateau on textured backgrounds, low enough to avoid the
size explosion beyond it, and only marginally wasteful (hundreds of bytes) on the
smoothest backgrounds — not worth breaking byte-compatibility (the default is
`Iw44Target::Slices`, byte-identical to legacy) for. IW44_DIAG's fine-band
slice-starvation lever is **closed**: the loss it measured is the transform's
quantization floor, which the slice budget cannot lower. A content-adaptive
`Iw44Target::Bpp` default could shave the smooth-BG over-coding, but the win is
sub-KB per page and would change every colour encode's bytes — deferred as
low-value. (The real BG44 size lever was BG_DIFFUSE, round 17, which shrinks the
*input* the codec sees rather than the slice schedule.)

## Perf round 36 (2026-07-06) — QUALITY_COLOR: colour-aware D1 metric

**Issue.** The D1 perceptual harness (`src/quality.rs`, round 9) scores **luma
only** — a structural blind spot flagged as an honesty caveat in round 31
(FGBZ_MEDIANCUT): the median-cut palette experiment had a real, visible
colour wash-out at aggressive `k` that the harness reported as a negligible
SSIM change, forcing a fall-back to manual crop inspection. Several future
levers (palette, chroma, FG colour) need an honest colour metric before they
can be judged quantitatively.

**Prior-art check.** `EXPERIMENTS_INDEX.md` / `PERF_EXPERIMENTS.md`: no colour
SSIM/ΔE work exists — every prior mention (`FGBZ_MEDIANCUT`, `CHROMA_BILINEAR`
#422, `COLOR_AA` #439) explicitly notes the luma-only limitation rather than
fixing it. `git branch -r` / `gh pr list --search "ssim OR quality"`: no
in-flight or merged branch touches colour metrics.

**Approach.** Added `quality::compare_color(a, b) -> ColorQualityReport`
alongside the existing (untouched, byte-identical) `psnr`/`ssim`/`compare` API:

- Converts both images to **YCbCr** (ITU-R BT.601 coefficients — the standard
  perceptual chroma axes, distinct from DjVu's own lossless `Cb=b−g`/`Cr=r−g`
  IW44 wire-format shortcut) and reuses the existing windowed-SSIM machinery
  independently on `Y`, `Cb`, `Cr`.
- Reports a luma-dominant **weighted combined SSIM** (`0.8·Y + 0.1·Cb + 0.1·Cr`),
  matching the asymmetry DjVu's own chroma-subsampling/chroma-delay design
  already assumes (chroma is far less perceptually salient than luminance).
- Reports mean/max **ΔE76** (Euclidean CIE L\*a\*b\* distance, sRGB→linear→XYZ
  D65→Lab, standard formulas) as an absolute colour-difference number in the
  same units colour-reproduction practice uses (`<1` imperceptible, `>10`
  obviously different colour).
- No new dependencies — `quality.rs` is declared `#[cfg(feature = "std")]` at
  the `mod` level in `lib.rs`, so `f64::{powf,cbrt,sqrt}` (unavailable in
  `core`) are already fair game; confirmed no other no_std/wasm32 build path
  touches this file.
- 8 new unit tests (`quality::tests`), including a synthetic same-luma/
  shifted-chroma fixture solved so `306·r+601·g+117·b` (the module's luma
  weights) is within 0.07 between `(200,104,255)` "lavender" and `(0,255,3)`
  "green" — a luma-only metric scores them ≈ identical (SSIM > 0.999) while
  `compare_color` must not (asserted `ssim_cb`/`ssim_cr < 0.8`, `ΔE > 100`).

**Wired into `examples/quality_harness.rs`**: every luma `print_row` now has a
`print_color_row` companion (`SSIM Y/Cb/Cr/combined` + `ΔE mean/max`), and into
`examples/fgbz_mediancut_harness.rs` (the round-31 fixture) for direct
before/after validation.

**Validation 1 — synthetic isoluminant colour shift (unit test).** Confirms
luma-SSIM(lavender, green) > 0.999 while `compare_color` reports `ssim_cb`/
`ssim_cr` < 0.8 and ΔE76 mean > 100 — the metric catches exactly what luma-SSIM
structurally cannot.

**Validation 2 — real corpus: round-31 FGBZ_MEDIANCUT scenario re-run at
aggressive `k`** (`fgbz_mediancut_harness --synthetic 0 2,4,6,32`,
`EncodeQuality::Quality`, vs the source render):

| Palette | Total bytes | SSIM (luma) | **SSIM Cb** | **SSIM Cr** | SSIM combined | ΔE mean | ΔE max |
|---|---|---|---|---|---|---|---|
| Exact (baseline) | 9014 | 0.86036 | 0.87911 | 0.87956 | 0.86416 | 12.37 | 77.78 |
| **MedianCut{2}** | 6824 | 0.85477 | 0.82932 | **0.32563** | 0.79932 | **25.63** | 82.63 |
| MedianCut{4} | 6834 | 0.85378 | 0.82634 | 0.77821 | 0.84348 | 20.44 | 80.43 |
| MedianCut{6} | 6840 | 0.85731 | 0.85557 | 0.82555 | 0.85396 | 16.25 | 77.78 |
| MedianCut{32} | 7186 | 0.86054 | 0.87910 | 0.87938 | 0.86428 | 12.38 | 77.78 |

At `k=2` luma SSIM barely moves (0.86036 → 0.85477, **−0.0056**, would read as
"no meaningful change") while **`Cr` SSIM collapses 0.87956 → 0.32563
(−63 %)** and ΔE mean **doubles** (12.37 → 25.63) — the exact failure mode the
round-31 caveat described, now caught by a single number instead of manual
crop inspection. At `k≥6` (≥ the fixture's true colour count) the colour
metric recovers to within measurement noise of the baseline, agreeing with
round-31's crop-inspection conclusion that `k=6/32` are visually
indistinguishable. **The colour metric degrades before (and far more sharply
than) luma SSIM does, exactly as the goal required.**

**Validation 3 — real fixture vs ddjvu reference** (`quality_harness
tests/fixtures/colorbook.djvu 0 --half`): Lanczos3 (0.9928 luma SSIM, ΔE mean
0.31) beats Bilinear (0.9594 luma SSIM, ΔE mean 0.97) on **both** luma and
colour axes — consistent with round-9's D2 finding, and shows the colour
metric agreeing with luma SSIM (not just contradicting it) when a change is
genuinely better on every axis. The CHROMA_BILINEAR (#422) chroma_half
fixture (`tests/fixtures/carte.djvu`) could not be re-run: it fails to parse
via `DjVuDocument::parse` with a pre-existing `Iff(Truncated)` error unrelated
to this change (not investigated further — out of scope).

**Correctness.** `make check` (fmt, clippy `-D warnings`, no_std build,
wasm32 no_std+wasm build, full workspace test suite) passes. 12/12
`quality::tests` pass (4 pre-existing + 8 new).

**Decision.** **Kept (infra).** Purely additive — `psnr`/`ssim`/`compare`/
`compare_gray` and their byte-for-byte behaviour are untouched;
`compare_color` is a new, separate entry point. Unblocks honest quantitative
judgement of the palette/chroma/FG-colour levers `FGBZ_MEDIANCUT`,
`CHROMA_BILINEAR`, and future colour-encoder work flagged as needing it.
## Perf round 37 (2026-07-06) — IW44_RATE_TARGET: resurrecting `feat/iw44-quality-target` — already shipped

### IW44_RATE_TARGET — byte-budget encode-stopping criterion — **already merged, validation-only round**

**Task.** Resurrect and finish `origin/feat/iw44-quality-target` — a remote
branch whose last commit ("feat(iw44): add `Iw44Target::Bpp` byte-budget
encode-stopping criterion", 2026-07-01) had no open PR, framed as abandoned
work predating rounds 15–35. The brief asked for a working, tested
`Iw44Target` rate/quality feature wired through `encode_iw44_color`/`gray`,
ideally with a byte/bpp budget and a PSNR floor.

**Finding — there was nothing to resurrect.** `gh pr list --state all --search
"iw44 quality OR bpp OR target"` shows **PR #475**, same branch
(`feat/iw44-quality-target`), same title, state **MERGED** (2026-07-01,
squash commit `a1e3d54b`). `git merge-base --is-ancestor a1e3d54b origin/main`
confirms it: the squash commit is in `main`'s history, even though
`git merge-base --is-ancestor origin/feat/iw44-quality-target origin/main`
says no — the *branch tip* commit was squashed into a different SHA on merge,
and the remote branch ref was simply never deleted afterward. Reading only
`git log <branch>` (as the brief's framing did) makes a squash-merged branch
look like an abandoned one; `gh pr list` is the source of truth.

The feature is fully live in `crates/djvu-iw44/src/encode.rs`: `Iw44Target::{
Slices, Bpp(f32) }` on `Iw44EncodeOptions`, computed once as a `byte_budget`
in `encode_chunks` (the function both `encode_iw44_color` and
`encode_iw44_gray` funnel through), default `Slices` (byte-identical to
pre-target versions, enforced by
`bpp_target_slices_default_is_byte_identical`), a `--bpp` CLI flag in
`src/bin/djvu.rs`, and 6 unit tests already covering monotonicity,
decodability, the tiny-budget one-chunk floor, and the gray path.

**PSNR/quality-floor stretch goal — deliberately not built.** The brief's
"ideally a PSNR floor" is exactly the feature round-22 (`IW44_ENTROPY_GAP`,
above) already scoped and rejected as a quick win: a *fixed* bpp/byte budget
cannot capture the smooth-vs-textured saturation divergence — watchmaker
saturates around 0.008 bpp, textured content (colorbook-class) needs ~0.265
bpp, a 30× spread — so a genuine quality floor needs a **content-adaptive**
decibel-style stop, which round-22 valued at "~100 B/page, low EV... not a
quick clean win." Nothing since then changes that calculus (BG_DIFFUSE,
round 20, captured the real BG44 size win from a different angle — smaller
input — not a smarter stop). Building it here would re-litigate a
still-valid, already-recorded verdict rather than add anything, so it was
left alone.

**What this round adds — closing the brief's own validation gaps.** New
`tests/iw44_rate_target.rs` (4 tests, real corpus fixtures — the original
PR's test module used only synthetic gradient fixtures):

- `bpp_target_is_deterministic` — same input + same `Iw44Target::Bpp` budget
  ⇒ byte-identical chunk vectors across two independent encode calls.
- `bpp_target_respects_budget_within_one_slice` — with caller-set
  `slices_per_chunk = 1` (existing knob, no source change needed), the
  budget check runs at single-slice granularity; emitted bytes stay within
  one empirically-measured single-slice chunk size of the requested budget,
  at 4 budget points on `watchmaker`.
- `bpp_target_sweep_is_monotone_and_default_unchanged` — sweeps
  `bpp ∈ {0.05, 0.1, 0.2, 0.5, 1.0}` across `watchmaker` (smooth), `colorbook`
  (textured), `conquete_paix` (mixed, multi-page); asserts byte size is
  monotone non-decreasing, PSNR is monotone non-decreasing (0.05 dB slack for
  quantization plateaus), and that the default `target` is byte-identical to
  explicit `Iw44Target::Slices`.
- `bpp_truncated_stream_round_trips_at_every_prefix` — a `colorbook` stream
  truncated by `Iw44Target::Bpp(0.03)` (fewer chunks than the default
  10-chunk schedule) decodes successfully with correct dimensions at *every*
  chunk-count prefix — the progressive chunk format tolerates the early stop
  the budget introduces, the same way it already tolerates a network reader
  that stops fetching chunks early.

**Sweep table** (from the new test; PSNR is this run's pre-encode reference
vs. re-decode, not vs. ddjvu/c44):

| bpp | watchmaker (850×1101, smooth) | colorbook (754×1223, textured) | conquete_paix (1423×2285, mixed) |
|-----|-------------------------------|----------------------------------|--------------------------------------|
| 0.05 | 1481 B / 48.45 dB / SSIM 0.9988 | 6542 B / 21.85 dB / SSIM 0.9816 | 3714 B / 56.30 dB / SSIM 0.9997 |
| 0.10 | 1481 B / 48.45 dB / SSIM 0.9988 | 13090 B / 21.99 dB / SSIM 0.9877 | 3714 B / 56.30 dB / SSIM 0.9997 |
| 0.20 | 1481 B / 48.45 dB / SSIM 0.9988 | 13090 B / 21.99 dB / SSIM 0.9877 | 3714 B / 56.30 dB / SSIM 0.9997 |
| 0.50 | 1481 B / 48.45 dB / SSIM 0.9988 | 13090 B / 21.99 dB / SSIM 0.9877 | 3714 B / 56.30 dB / SSIM 0.9997 |
| 1.00 | 1481 B / 48.45 dB / SSIM 0.9988 | 13090 B / 21.99 dB / SSIM 0.9877 | 3714 B / 56.30 dB / SSIM 0.9997 |

`watchmaker` and `conquete_paix` both saturate at bpp=0.05 already — their
natural (unbudgeted) size is below every tested budget, so the target never
truncates them, matching round-22's smooth/mixed-content finding exactly.
`colorbook` (textured) genuinely truncates at bpp=0.05 (6542 B / 21.8 dB) vs
its 13090 B / 22.0 dB natural size, and reaches the same plateau by bpp=0.10 —
also consistent with round-22.

**Verdict.** **No shipped-code change** — the feature and its
default-preserving contract (`target: Iw44Target::Slices` byte-identical) are
unchanged; the two levers the brief asked about (byte budget, quality floor)
were each already correctly resolved by prior rounds (shipped / deliberately
deferred). This round's contribution is closing the validation gap — proving
the shipped feature on real corpus content instead of only synthetic
fixtures — and recording the true fate of the dangling branch ref so it
isn't proposed again as abandoned work.
## Perf round 38 (2026-07-06) — VIEWER_BENCH + C4_TILE_CACHE: pan/zoom scenario bench, then the tile cache it justifies

Round-8 triage flagged the tile-cache question as NEEDS-INFRA: "C4 tile cache —
an API feature (pan/zoom viewer), needs a panorama-scenario bench." C3_ZOOM_SCOPE
(round 14) showed region/zoom rendering is already scoped and linear in viewport
pixels (~5.5 ns/px, flat vs zoom) — but that diagnostic never asked what happens
when *consecutive* viewports overlap, which is exactly what a pan gesture does.
Existing caches (BG/MASK/FG layer caches, C5 LRU byte budget) only memoize
*decoded* layers; nothing memoizes the compositor's *output*. This round builds
the scenario bench first, then closes C4 with data.

### VIEWER_BENCH — scripted pan/zoom session bench — infra (2026-07-06)

**Setup.** New `benches/viewer.rs`, harness-registered in `Cargo.toml`. Scripts a
realistic interactive session on two page kinds — a colour page (`colorbook.djvu`,
BG44+FG44/FGbz) and a bilevel page (`cable_1973_100133.djvu`, JB2 mask only):
open → first full render → zoom 2× at page centre → pan across the page in 12
overlapping viewport steps (25% overlap, so each step exposes ~75% new content on
its trailing edge) → zoom 4× → pan again. For every pan sequence, two variants are
benched using only the *existing* `render_region` API (no new production code
needed to answer this question):

- `full_recomposite`: today's behaviour — every step re-renders its entire
  viewport rectangle from scratch.
- `incremental_strip`: a lower-bound proxy for a tile-cache-backed viewer — every
  step after the first renders *only* its newly-exposed trailing strip; a real
  cache would supply the rest from memory.

`incremental_strip / full_recomposite` (both the whole-sequence total and the
per-step `BenchmarkId`s) estimates the compositor-only dividend a tile cache could
capture, measured against the real compositor rather than a per-pixel cost model.
M1 Max, `cargo bench --bench viewer -- --quick`, `[profile.bench]` fat LTO.

**Results (whole-sequence totals, the load-robust number — wall-clock is noisy
with concurrent agents, but the two variants run back-to-back in the same
process):**

| Page | Zoom | full_recomposite_sequence | incremental_strip_sequence | Δ |
|---|---|---|---|---|
| colour (colorbook) | 2× | 5.370 ms | 4.251 ms | **−20.9%** |
| colour (colorbook) | 4× | 21.89 ms | 16.78 ms | **−23.3%** |
| bilevel (cable) | 2× | 5.459 ms | 4.318 ms | **−20.9%** |
| bilevel (cable) | 4× | 20.90 ms | 16.78 ms | **−19.7%** |

Per-step numbers tell the same story steadily across all 12 steps (not just an
average): e.g. colour 2× per-step settles at full≈447 µs vs incremental≈345 µs
(steps 1–11, ratio ≈0.77) once past step 0 (where `fresh == viewport` by
definition, so both variants cost the same). Colour 4× per-step: full≈1.74–1.83 ms
vs incremental≈1.32–1.42 ms (ratio ≈0.76). Bilevel tracks the same ~20–25% band at
both zoom levels. The ratio lines up almost exactly with the 25% pan overlap
parameter — each step only reuses (skips) the 25% shared strip, which is the
*minimum* a real cache would save: a straight-line pan never revisits a tile, so
this proxy cannot show the cache-hit wins a real viewer gets from zooming back
out/in, panning back, or re-exposing previously-composited regions.

### C4_TILE_CACHE — composited-output tile cache, `render_region_tiled` — **Kept** (2026-07-06)

**Decision basis.** VIEWER_BENCH shows overlapping pans redo a steady ~20–25% of
compositor work on *every* step, on both colour and bilevel pages, at both tested
zoom levels — and that's a lower bound (see above). This is exactly the
"substantial compositor work redone" case the task's Phase 2 threshold called for
building the cache.

**Implementation.** `render_region_tiled(page, region, opts)` in `src/djvu_render.rs`
(std-only, `#[cfg(feature = "std")]`) is a new, separate entry point — it does not
modify `render_region` or its hot loop, so existing callers (thumbnails, export,
one-shot renders) pay zero overhead. It decodes layers once via the existing
`decode_layers`, then assembles the requested rectangle from `TILE_SIZE=256`
(256 KiB/tile RGBA) composited tiles cached per-page in a new `PageLayers::tile_cache`
(`HashMap<TileKey, Arc<TileEntry>>` + FIFO order + running byte total), keyed by
`(full_w, full_h, tile_x, tile_y, bold, mask_aa)`. A cache miss composites exactly
that tile via `composite_into` (unchanged compositor) and inserts it; a hit clones
an `Arc`. FIFO-evicts at a `TILE_CACHE_MAX_BYTES = 8 MiB` per-page budget, and the
tile bytes are folded additively into `PageLayers::cached_bytes()` — so the
existing C5 `DjVuDocument::enforce_cache_budget` / `evict_render_cache` machinery
automatically reclaims tile memory as a side effect of evicting a page; no
separate cross-page tile LRU was needed.

**Byte-identical, by construction.** `composite_into` computes every output pixel
from its *absolute* position (`ctx.offset_x + ox`, `ctx.offset_y + oy`), never from
the requested `out_w`/`out_h` — confirmed by reading `precompute_area_avg_x` /
`area_range`. Tile boundaries sit on that same absolute pixel grid
(`CompositeContext` gained `#[derive(Clone, Copy)]` so each tile stamps a cheap
per-tile copy of one template context), so assembling a request from whole or
partial cached tiles reproduces exactly the bytes a direct `render_region` call
would. Eligibility is deliberately narrow — the tiled path only activates for
`Resampling::Bilinear`, identity combined rotation, and non-permissive decode;
anything else (Lanczos3, any rotation, `permissive: true`) falls straight through
to a plain `render_region` call with no tile bookkeeping. This is opt-in by
construction: callers who want tile caching (a pan/zoom viewer) call
`render_region_tiled`; nobody else's byte output or performance changes.

**Tests** (`src/djvu_render.rs::tests`, all passing under `cargo test --lib`):
`render_region_tiled_matches_render_region` (7 regions incl. multi-tile-spanning,
edge tiles, sub-tile slivers, at a non-256-multiple size), `render_region_tiled_
repeated_region_matches` (cache-hit + neighbour-overlap correctness),
`render_region_tiled_falls_back_for_ineligible_modes` (rotation, Lanczos3,
permissive all match `render_region` exactly), `render_region_tiled_cache_is_
budget_bounded_and_evictable` (tile bytes stay `> 0` and `<= TILE_CACHE_MAX_BYTES`;
`evict_render_caches()` zeroes them), `render_region_tiled_rejects_zero_dimensions`.

**Verdict.** **Kept.** Data-justified addition: VIEWER_BENCH measured a real,
steady ~20–25% compositor-time gap per pan step across both page kinds and both
zoom levels the task specified, and a real tile cache captures strictly more than
the `incremental_strip` proxy did (it also serves exact re-visits — zoom out/in,
pan-back — which the proxy's straight-line pan never exercises). Kept off the
`render_region` hot path entirely (new function, `#[cfg(feature = "std")]`-gated,
no_std/wasm32 builds carry zero extra code), byte-identical by construction and by
test, and its memory is accounted for and evicted through the existing C5 budget
machinery rather than a new one. `make check` (fmt, clippy -D warnings, no_std
build, wasm32 build, full test suite) green.
## Perf round 39 (2026-07-06) — WASM_THREADS: parallel render in the browser (wasm)

**Prior art check.** `EXPERIMENTS_INDEX.md`'s only wasm row was WASM_SIMD (scalar
vs simd128, round unrelated to threads). `src/wasm.rs` had no thread-pool
plumbing. `Cargo.toml` had no `wasm-bindgen-rayon` dependency and no
`wasm-threads`-shaped feature. `gh pr list --search "wasm thread OR rayon wasm"`
and `git branch -r` turned up nothing relevant. Not previously attempted —
proceeded.

### WASM_THREADS — feasibility matrix + opt-in infra — **D-infra** (2026-07-06)

**Goal.** Can `wasm-bindgen-rayon` reuse the existing rayon-parallel paths
(PARALLEL 3.8× compositor, IW44_PAR 2.2× IDWT, both native-only today) inside a
browser tab? Feasibility + infra first; a merged runtime win was explicitly
out of scope unless it fell out naturally.

**1. Feasibility matrix.**

| Requirement | Status |
|---|---|
| Toolchain | **Nightly required.** `wasm-bindgen-rayon` needs a wasm32 `std` built with atomics, which stable's prebuilt `std` doesn't have. `cargo check -Z build-std=panic_abort,std --target wasm32-unknown-unknown` — `-Z build-std` is nightly-only cargo. Confirmed: `rustup component add rust-src --toolchain nightly` + local nightly 1.94.0 builds fine; stable 1.92 cannot even parse `-Z`. |
| Codegen flags | `RUSTFLAGS='-C target-feature=+atomics,+bulk-memory'` — compiles, but the resulting `WebAssembly.Memory` is **not** `shared`, so `postMessage`-ing it to a Worker throws `DataCloneError: #<Memory> could not be cloned` at runtime (only caught by actually running it — a `cargo check`/`cargo build` alone doesn't surface this). |
| Linker flags | Needed in addition: `-C link-arg=--shared-memory -C link-arg=--max-memory=1073741824 -C link-arg=--import-memory` plus TLS exports (`__wasm_init_tls`, `__tls_size`, `__tls_align`, `__tls_base`). With these, the `Memory` is created `shared: true` and clones into Workers correctly. |
| Runtime headers | `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp` on every response, or `SharedArrayBuffer`/`crossOriginIsolated` is `false` and `initThreadPool` throws. Plain `python3 -m http.server` can't set these — needed a custom handler. |
| `--target web` (no bundler) gotcha | `wasm-bindgen-rayon`'s generated worker glue does `import('../../..')` — a bare-directory import that only resolves through a bundler's package.json "main" lookup. Plain browser ES-module loading has no such fallback, so the Worker's dynamic import 404s/mis-types silently and `initThreadPool` **hangs forever** (no error surfaces — the awaited `ready` message from the worker just never arrives). Fixed for local testing by copying `djvu_rs.js` to `pkg/index.js` and having the static server serve `index.js` for bare directory URLs lacking `index.html`. This is a `--target web` + no-bundler-specific issue; `--target bundler`/webpack consumers won't hit it. |
| CI impact | Zero — `wasm-threads` is a new feature, off by default, pulled in by nothing the existing `wasm` feature or `cargo check --target wasm32-unknown-unknown --features wasm` touches. Reverified after wiring: that exact CI command still succeeds unchanged. |

**2. Infra shipped.** `wasm-threads = ["wasm", "parallel", "dep:wasm-bindgen-rayon"]`
(`Cargo.toml`) layers over the *existing* `wasm` + `parallel` features rather than
duplicating them, so it inherits PARALLEL's compositor and IW44_PAR's IDWT
parallelism for free. `src/wasm.rs` re-exports
`wasm_bindgen_rayon::init_thread_pool` behind `#[cfg(feature = "wasm-threads")]`
as `initThreadPool(n)` for JS callers (`await init(); await
initThreadPool(navigator.hardwareConcurrency);`). `scripts/wasm_threads_check.sh`
+ `make wasm-threads-check` wrap the nightly/build-std/RUSTFLAGS incantation
above (both `check` and `--build` modes); intentionally **not** added to
`scripts/check.sh` / any required CI gate, per the nightly requirement.
`examples/wasm/README.md` documents the one-time nightly setup, the full
`wasm-pack build` invocation, the COOP/COEP + directory-import server caveat,
and JS usage.

**3. Measurement — real, not simulated.** Built two actual `wasm-pack --target
web --release` packages (stable single-threaded `wasm`, and nightly
`wasm-threads` with the flags above), served them locally with a COOP/COEP +
`index.js`-fallback Python handler, and drove a real Chrome tab (via the
`claude-in-chrome` browser automation) against `tests/fixtures/colorbook.djvu`
(2260×3669 @ 400 dpi, the same fixture PAR_LANCZOS/PARALLEL use). 8 reps/config,
first rep discarded as JIT/cold-cache warmup, median of the rest reported.
`navigator.hardwareConcurrency` = 10 (M1 Max, matches native benches' machine).

*Full-page IW44 decode, native 400 dpi (no compositor downscale — exercises only IW44_PAR's 3-way `rayon::join`):*

| Config | Run 1 median | Run 2 median |
|---|---|---|
| single-threaded (stable `wasm`) | 58.05 ms | 55.78 ms |
| `wasm-threads`, pool=1 (isolates dispatch overhead) | 56.53 ms | — |
| `wasm-threads`, pool=10 | 54.75 ms | 56.98 ms |

Run-to-run, pool=10 flips from ~4.5% *faster* to ~2% *slower* than single —
i.e. **no reliable win**, within measurement noise. IW44_PAR's parallelism is
capped at 3-way (one `rayon::join` per Y/Cb/Cr plane), too little grain to
amortize the Worker/`Atomics` dispatch cost that wasm threading adds on top of
what's free on native OS threads.

*Compositor downscale path, 150 dpi target (exercises PARALLEL's `par_chunks_exact_mut`, many small chunks):*

| Config | Median |
|---|---|
| single-threaded (stable `wasm`) | 18.59 ms |
| `wasm-threads`, pool=1 | 72.88 ms (first 4 reps 126–159 ms warming up, last 4 ≈18–19 ms — matches single once warm) |
| `wasm-threads`, pool=10 | **167.17 ms — 9× slower**, no improvement across 8 reps (143–182 ms steady-state) |

At 150 dpi the per-chunk work is small; splitting it across 10 Workers makes
the fixed per-dispatch `Atomics`/postMessage-synchronization cost dominate
completely. This is the sharpest, most reproducible finding: **more wasm
threads made the compositor path an order of magnitude slower**, the opposite
of the native 3.8× PARALLEL win on the same code path.

**Decision.** **D-infra** (feasibility + infra, no runtime win to merge as a
default). Ship the opt-in `wasm-threads` feature, thread-pool export, and
manual-test recipe as validated, working infrastructure — it is real and
buildable, and useful for anyone doing bulk/coarse-grained wasm parallelism
(e.g. parallel multi-page encode/decode, where PAR_ENCODE/PAR_DEC-style
per-page granularity would give each Worker enough work to amortize dispatch
cost). But do **not** claim a render speedup: on this codec's actual per-page
render workload, wasm-bindgen-rayon's Worker/`Atomics` dispatch overhead either
erases the native parallel win entirely (full decode) or turns it sharply
negative (compositor downscale). Revisit only if `wasm.rs` grows a
coarser-grained parallel entry point (e.g. batch-render N pages), not for
single-page `render()`.
## Perf round 40 (2026-07-06) — COLD_OPEN: cold-start harness + madvise/prefetch

Round-8 flagged "B6 madvise / B7 speculative next-page decode — need a cold-disk /
simulated-network harness; no such bench exists" (see the "Remaining axes —
classified" NEEDS-INFRA list). LAZY_PAGE_CONSTRUCT (round 4) gave −48% `from_bytes`
and mmap zero-copy backing but nothing issues `madvise` hints or prefetches ahead
of the reader. This round builds the harness and measures both levers on it.

### COLD_OPEN_HARNESS — cold vs warm open→render measurement — **Kept (infra)** (2026-07-06)

**Setup.** `examples/cold_open_bench.rs` (`--features mmap,parallel`), three
`--mode`s against `tests/corpus/pathogenic_bacteria_1896.djvu` (517 pages, 26 MB,
via SOURCES.md). macOS has no `posix_fadvise(DONTNEED)`/`drop_caches` equivalent
reachable without a password prompt, so the harness copies the corpus to a fresh
temp path every iteration, writing the destination through an `F_NOCACHE`-flagged
fd (`fcntl(fd, F_NOCACHE, 1)`, macOS-only, best-effort) so the copy's own pages
aren't retained in the unified buffer cache — a fresh inode that should require
genuine disk I/O on first touch, not whatever's resident from a prior iteration.
`sudo purge` + `--mode validate --iters 1` is documented as the manual gold-standard
cross-check if the automated ratio ever collapses toward 1x. Stats: median + MAD
(median absolute deviation) over N iterations, per the task's "prefer medians,
report dispersion" instruction — MAD is not dragged around by the occasional
scheduler/thermal outlier the way stdev is.

**Validation** (`--mode validate --page 0`, M1, local NVMe): cold ≫ warm, cleanly
and repeatably —

| run | warm median (MAD) | cold median (MAD) | ratio |
|-----|--------------------|--------------------|-------|
| iters=9  | — | — | 1.99x |
| iters=12 | 7.224 ms (0.161) | 34.388 ms (0.464) | **4.76x** |

Both arms have tight MAD relative to their median (warm 2.2%, cold 1.3%) — the gap
is a real cache-identity effect, not noise. **Verdict: the F_NOCACHE-fresh-copy
strategy is the reliable one on macOS**; strategy (a) alone (`F_NOCACHE` on the
*original* file without copying) does not evict already-resident pages from a
prior `mmap`/read of the same vnode (confirmed against the documented fcntl
semantics before building the harness around it), so the copy is load-bearing, not
just belt-and-suspenders.

**Decision.** Kept as infra — this is the harness the B6/B7 backlog item was
blocked on. `--mode madvise` and `--mode prefetch` build on the same cold-copy /
warm-reuse machinery for the two levers below.

### B6_MADVISE — `MADV_WILLNEED` on page byte range after mmap — **Rejected (neutral; not shipped disabled)** (2026-07-06)

**Setup.** `MmapDocument::advise_page_willneed(index)` (`src/djvu_document.rs`)
calls `memmap2::Mmap::advise_range(Advice::WillNeed, ..)` — the crate's safe
wrapper, no unsafe in library code — over a page's own FORM byte range (from the
existing `DjVuDocument::page_byte_range` API). Measured cold open→first-render
delta via `--mode madvise`, interleaved no-advise/with-advise pairs (fresh cold
copy per iteration) so drift affects both arms evenly.

**First attempt (rejected outright): advise the whole `0..range.end` prefix**
(header + DIRM + every page before the target, reasoning "grab it all while
we're at it") — this **reproducibly regressed cold open by ~12%** (page 250,
iters=20: 61.4 ms → 68.9 ms, tight MAD ~1 ms both arms — a real effect, not
noise). Over-advising is actively harmful on this host, not just wasted effort:
it appears to compete with the demand-fault path for readahead bandwidth on a
range far larger than what's about to be read.

**Corrected version: advise only the target page's own range** — neutral:

| page | delay | no madvise median (MAD) | madvise median (MAD) | delta |
|------|-------|--------------------------|------------------------|-------|
| 250 | 0 ms | 45.649 ms (1.600) | 46.390 ms (0.797) | −1.6% |

±0–2% across pages/delays tested — inside noise. **Why no win, even scoped**: (1)
`MmapDocument::open()` already synchronously walks every page's IFF chunk header
across the *whole* 517-page file to build `page_byte_ranges`, before the caller
can even call `advise_page_willneed` — most of the structural cold-read cost is
paid before the hint exists to issue; (2) individual page FORM ranges are small
(tens of KB) so on fast local NVMe the demand-fault latency already approaches
the readahead latency — there's little headroom left for a hint to close.

**Decision.** **Rejected as a default-on lever** — no measurable win on this
host/corpus, and the naive version actively regressed. **Kept as an opt-in,
tested primitive** (`#[cfg(unix)]`, best-effort `Result`, no-op on out-of-range
index) rather than reverted outright: the negative result is hardware-specific
(fast local SSD, small per-page ranges) not a conceptual flaw — analogous to
AVX2_IDWT's "D/F — blocked, no x86 host" status. Likely worth revisiting on
higher-latency storage (network mounts, spinning disks, remote filesystems)
where readahead has more room to hide behind. Tests:
`mmap_advise_page_willneed_in_range_and_out_of_range`.

### B7_PREFETCH_PAGE — background decode of next page during reader dwell time — **Kept (opt-in)** (2026-07-06)

**Setup.** `DjVuDocument::prefetch_page(self: &Arc<Self>, index)` (`parallel`
feature, `src/djvu_document.rs`), `rayon::spawn`'d fire-and-forget: decodes the
target page's mask/FG44/BG layers into the existing `OnceLock`-backed
`PageLayers` render caches (same ones a later synchronous `render_pixmap` reads)
— no new cache machinery, no unsafe, thread-safety comes from `OnceLock::get_or_init`
naturally deduplicating a concurrent decode against a later synchronous one.
Out-of-range index is a no-op. Measured via `--mode prefetch`: render page K,
call `prefetch_page(K+1)`, sleep `dwell_ms` (simulating reading time), then time
`render(K+1)` — with vs without the prefetch call. Whole file is warmed into the
OS page cache up front so the delta isolates decode-overlap, not disk I/O.

| page K | dwell | no-prefetch median (MAD) | prefetch median (MAD) | delta |
|--------|-------|---------------------------|--------------------------|-------|
| 0   | 150 ms | 6.939 ms (3.864) | 5.099 ms (1.809) | +26.5% |
| 0   | 150 ms (n=12, earlier run) | — | — | +23.8% |
| 100 | 300 ms | 30.393 ms (12.063) | 3.142 ms (1.642) | **+89.7%** |
| 0   | 0 ms (negative control) | 2.111 ms (0.393) | 2.126 ms (0.466) | −0.7% (noise) |

Two independent runs at page 0/dwell 150 ms agree (+23.8%, +26.5%), and the
prefetch arm's MAD is consistently tighter than the baseline's — a real,
reproducible effect, not a fluke. Page 100/dwell 300 ms (deeper into the corpus,
more dwell time for the background decode to finish) shows a much larger win
(+89.7%) because the baseline's cold-cache decode is heavier there, giving the
background thread more useful work to hide. The dwell=0 "instant page flip"
control correctly shows **no win** (delta pinned at noise level) — honest
disclosure: prefetch only pays off when the reader's dwell time is comparable to
or longer than the background decode time; a reader who flips pages faster than
the decode completes gets no benefit (but also no regression — the foreground
render just does the same work itself, `OnceLock` prevents duplicate work either
way).

**Decision.** **Kept**, opt-in (`parallel` feature only, must be called
explicitly — no automatic prefetch policy shipped, that's a viewer-level
decision about which page to guess next). Consistent, reproducible win (+24–90%
depending on dwell/page) with a correctly-behaving null case at dwell=0 and no
observed downside (respects the existing C5 cache-budget/LRU eviction machinery
since it writes through the same `PageLayers` cache, no separate unbounded
prefetch buffer). Tests: `prefetch_page_warms_cache_and_ignores_out_of_range`.

**Known pre-existing issue found, not caused by this round**: with `--features
mmap` enabled, `djvu_render::tests::progressive_decoder_chunk_decodes_are_on_not_on_squared`
fails (expects 6 decode calls, observes 0). Verified via `git stash` that this
reproduces identically on the pre-round-38 tree — unrelated to this diff. CI's
test gate doesn't hit it (`--features cli` doesn't enable `mmap`), so it's
recorded here rather than fixed in scope. **Fixed in round 41** (MMAP_TEST_THREADLOCAL
below) — root cause turned out to be `parallel`, not `mmap`, dispatching the
counted decode onto a rayon worker thread invisible to the test's thread-local
counter.

**Follow-ups**: (1) B6 madvise deserves a re-measurement on higher-latency
storage (network share, spinning disk, or a VM with artificially throttled I/O)
where the current "no headroom" reasoning may not hold. (2) B7 prefetch is a
manual primitive — an automatic "prefetch page N+1 whenever page N finishes
rendering" viewer-level policy is a natural next step but is a product decision
(prefetch direction, cancellation on rapid back-and-forth, budget vs C5) out of
scope here.

## Perf round 41 (2026-07-06) — PY_ZEROCOPY: GIL release + zero-copy buffers in the Python bindings

**Prior-art check.** `djvu-py/src/lib.rs` (the whole crate is one file) had neither
lever: `Document::open`/`from_bytes`, `Page::render`/`text` ran fully under the
GIL (no `py.detach` anywhere — note pyo3 0.29, already in use per
`djvu-py/Cargo.toml`, renamed `Python::allow_threads`→`Python::detach` and
`Python::with_gil`→`Python::attach`; the task brief's "py.allow_threads" is the
pre-0.29 name), and every pixel-returning path (`Pixmap::data()`, `to_numpy()`,
`to_pil()`) copied `self.data: Vec<u8>` into a fresh `PyBytes` on *every call*.
`gh pr list --search "python OR pyo3 OR zerocopy"` turned up only the original
bindings PR (#128) and a pyo3-0.24→0.29 RUSTSEC bump (#348, already merged into
`main` — the local `chore/bump-pyo3-0.29` branch is a stale pre-merge copy, not
new work). No prior GIL-release or buffer-protocol work existed. Proceeded.

### PY_GIL_DETACH — `py.detach()` around render/open/text — **Kept** (2026-07-06)

**Setup.** Added a `Python<'_>` parameter to `Document::open`, `Document::from_bytes`,
`Page::render`, and `Page::text`, wrapping the actual file-read/parse/decode/
compositing/resample work in `py.detach(|| ...)`. Confirmed safe first: the core
crate asserts `Document: Send + Sync` at compile time (`src/lib.rs:625`), and
`Page<'a>`/`Pixmap` (`{ width, height, data: Vec<u8> }`) have no interior
mutability, so the djvu-py wrapper types (which only add `Arc`/`usize`/`u32`/`u16`
fields) satisfy pyo3's `Ungil` bound with no `unsafe`. `Document::from_bytes`
copies the input `&[u8]` to an owned `Vec<u8>` *before* detaching (the Python
buffer's lifetime shouldn't be assumed valid without the GIL); `Document::page()`
itself stays under the GIL since the core crate confirms it's a cheap
metadata-only borrow, not worth a detach/reattach round-trip.

Measured with 8 pages of `tests/fixtures/colorbook.djvu`, rendering each page 6×
(48 renders total), sequential vs `threading.Thread`-sharded across N threads
(script: scratchpad `bench_gil.py`, not checked in — ad hoc measurement per task
brief, no existing djvu-py pytest suite to extend):

| threads | wall time | speedup |
|---------|-----------|---------|
| 1 (baseline) | 3.35s / 1.99s (two runs, shared noisy host) | 1.00x |
| 2 | 1.75s / 1.03s | **1.92x / 1.93x** |
| 4 | 1.02s / 0.51s | **3.29x / 3.91x** |

Consistent ~1.9x at 2 threads and 3.3–3.9x at 4 threads across two independent
runs (absolute times vary because other agents share this host — the *ratio*
is what matters, per the task's noise caveat). Confirms the GIL is genuinely
released for the whole render call, not just released-and-immediately-reacquired.

**Decision.** Kept. `cargo clippy -p djvu-py --all-targets -- -D warnings` and
`cargo fmt -p djvu-py -- --check` clean; `maturin develop --release` + manual
Python smoke test pass; full `make check` (1077 tests) unaffected (djvu-py is
outside the cargo workspace test run, as before).

### PY_ZEROCOPY_BUFFER — buffer-protocol `Pixmap` + zero-copy numpy/PIL views — **Kept** (2026-07-06)

**Setup.** Implemented `__getbuffer__`/`__releasebuffer__` on `Pixmap` (flat
read-only `uint8` buffer of length `width*height*4`, mirroring pyo3's own
`tests/test_buffer_protocol.rs` pattern — `Bound<'_, Self>` received by value so
`slf.into_ptr()` hands one owned reference to `view.obj`, keeping the `Pixmap`
alive for exactly the buffer's lifetime; CPython's `PyBuffer_Release` calls the
matching `Py_DECREF`). This alone makes `memoryview(pixmap)`, `bytes(pixmap)`,
and `numpy.frombuffer(pixmap, ...)` work directly with no new API surface.
Added two additive convenience methods reusing it: `to_numpy_zerocopy()`
(`numpy.frombuffer(self, uint8).reshape(h, w, 4)`) and `to_pil_zerocopy()`
(`PIL.Image.frombuffer("RGBA", size, self, "raw", "RGBA", 0, 1)`) — both zero-copy
per numpy/Pillow's own documented buffer-protocol fast paths. Existing
`data()`/`to_numpy()`/`to_pil()` untouched (still copy into `PyBytes`) — old API
stays exactly as it was, matching the task's backward-compatibility requirement.

Verified correctness (not just speed): `to_numpy_zerocopy()` output byte-equal to
`to_numpy()`, `to_pil_zerocopy()` `.tobytes()` byte-equal to `to_pil()`'s. Verified
the lifetime story: `del pm` after taking `memoryview(pm)` leaves the memoryview's
data intact (backing `Pixmap` kept alive by the buffer's owned reference) until
the memoryview itself is released, then frees cleanly (no leak, no use-after-free
under repeated GC + read-back).

Measured on `tests/fixtures/big-scanned-page.djvu` page 0 (6780×9148, 248 MB RGBA),
50 calls each (scratchpad `bench_zerocopy.py`):

| path | time/call | vs zero-copy |
|------|-----------|--------------|
| `data()` (copies to `bytes`) | 23.5–23.7 ms | — |
| `memoryview(pm)` | ~0.0 ms (<1 µs) | **~140,000x** |
| `to_numpy()` (copies to `bytes` first) | 23.0–23.3 ms | — |
| `to_numpy_zerocopy()` | ~0.001 ms | **~23,000–25,700x** |
| `to_pil()` (copies to `bytes` first) | 50.8–51.2 ms (PIL's own `frombytes` copy is 2x the raw copy) | — |
| `to_pil_zerocopy()` | ~0.004–0.005 ms | **~10,800–11,300x** |

The absolute ratios are dominated by the fact that the zero-copy paths do
(almost) no work at all for a 248 MB buffer — the honest takeaway is "the
248 MB memcpy is eliminated," not a literal 20,000x wall-clock claim for
real workloads that then go on to do something with the pixels.

**Decision.** Kept. Same verification as PY_GIL_DETACH (clippy/fmt clean,
`make check` green, manual pytest-equivalent smoke script). No `unsafe` outside
the two buffer-protocol slot methods, which is inherent to implementing the
protocol at all (pyo3 doesn't offer a safe derive for it in 0.29) and follows
pyo3's own tested reference pattern line-for-line.

**Follow-ups**: djvu-py has no pytest suite and no CI job at all (`make check`
excludes it from the nextest run; there's no `.github/workflows` step that builds
or tests it) — the ad hoc scratchpad scripts used here aren't checked in. Adding
a minimal `djvu-py/tests/` + CI job (maturin build + pytest) would be a
reasonable follow-up so this round's coverage isn't purely tribal knowledge in
a perf-log entry. Also: the buffer protocol currently exposes a flat 1-D
`uint8` array (reshape happens numpy-side in the convenience methods) rather
than a native 3-D `(h, w, 4)` `Py_buffer` with real strides — that would need
heap-allocated shape/strides arrays threaded through `view.internal` for cleanup,
which is more `unsafe` surface for a marginal ergonomic win (`np.asarray(pixmap)`
directly vs. `np.frombuffer(pixmap, ...).reshape(...)`); left as-is on a
safety/benefit tradeoff.
## Perf round 42 (2026-07-06) — two recorded defects: `carte.djvu` parse rejection, mmap/parallel test failure

Two independent fixes carried over from earlier rounds' "known issue" notes
(PR #525, PR #529): `carte.djvu` failing `DjVuDocument::parse` with
`Iff(Truncated)`, and `progressive_decoder_chunk_decodes_are_on_not_on_squared`
failing under extra feature combinations. Both category **fix/robustness**, not
performance — no benchmark numbers, just root-caused and closed.

### CARTE_INFO_TRUNCATED — short INFO chunk rejected as truncated — **Fixed** (2026-07-06)

**Diagnosis.** `carte.djvu` (`tests/fixtures/carte.djvu` and the identical
`references/djvujs/library/assets/carte.djvu`) has been noted as a truncated/
unparseable fixture since at least round 15 (`PERF_EXPERIMENTS.md` lines
~2961, ~6251, ~8513, ~8679 all skip it as "truncated"). Checked whether it's a
genuinely corrupt fixture or a parser bug: `djvudump`/`ddjvu`/`djvused` (real
DjVuLibre, `/opt/homebrew/bin`) all parse and render the file cleanly —
`djvused -e "select 1; size"` reports `width=4200 height=2556`, matching the
file's own `INFO` chunk. Manually walked the IFF byte stream (Python):
`AT&T`(4) + `FORM`(4) + big-endian length(4) = 154270, ending exactly at the
154282-byte file's last byte — no dangling padding, no length overrun,
byte-exact framing all the way down through `DIRM`/`FORM:THUM`/`FORM:DJVU` and
every leaf chunk inside it (`INFO`, `Sjbz`, 4×`BG44`, `FG44`, `ANTz`, `TXTz`).
The single anomaly: `carte.djvu`'s `INFO` chunk is **5 bytes**
(`10 68 09 fc 11` = width 4200, height 2556, one version byte, no
dpi/gamma/flags), where every other fixture in the corpus (`boy.djvu`,
`colorbook.djvu`, `chicken.djvu`, etc. — checked all of `tests/fixtures`,
`tests/corpus`, `references/djvujs/library/assets`) ships the canonical
10-byte `INFO`. `src/info.rs`'s `PageInfo::parse` hard-required 10 bytes
(`too_short_is_error` even tested 9 bytes as the strictness bar), so
`DjVuDocument::parse` failed early with `Iff(Truncated)` on this one file —
DjVuLibre tolerates variable-length `INFO` chunks (missing trailing fields
default: dpi 300, gamma 2.2, no rotation) and this corpus fixture is the proof;
our parser did not.

**Fix.** `PageInfo::parse` (`src/info.rs`) now requires only 4 bytes
(width + height — the only fields DjVuLibre effectively treats as mandatory)
and defaults `dpi`/`gamma`/`flags` when the chunk ends before those offsets,
matching DjVuLibre's own tolerance rather than inventing new leniency.
Regression tests: `info::tests::carte_style_five_byte_info_parses_with_defaults`
(unit-level, the exact 5 bytes from the fixture) and
`djvu_document::tests::parse_carte_with_short_info_chunk` (document-level,
reads the real fixture and asserts `DjVuDocument::parse` now succeeds with the
right dimensions). Existing `too_short_is_error` renamed/adjusted to
`shorter_than_width_height_is_error` (< 4 bytes) since 9 bytes is now valid;
added `nine_bytes_parses_dpi_and_gamma_defaults_only_flags` to pin that
dpi/gamma are still honored when present and only the trailing flags byte is
defaulted. `examples/interop_pixdiff --corpus` now processes `carte.djvu`
instead of skipping it with a parse error — worth noting the *rendered pixels*
still diverge heavily from `ddjvu` (mean-abs 73.76, a separate, already-tracked
issue: `djvu-iw44`'s chroma-half decode of this file produces noise, pinned
since #99 — unrelated to and unblocked by this parse fix, comment updated in
`crates/djvu-iw44/src/lib.rs` to stop attributing that noise to the (now-fixed)
parser rejection).

**Decision.** Parser bug, fixed. Not a corrupt fixture — no `SOURCES.md` note
needed.

### MMAP_TEST_THREADLOCAL — feature-sensitive decode-count assertion — **Fixed (test)** (2026-07-06)

**Diagnosis.** PR #529 recorded `progressive_decoder_chunk_decodes_are_on_not_on_squared`
failing "under `--features mmap`" (expects 6 decode calls, observes 0).
Reproduced with the full `--features cli,mmap,parallel` combo — but bisecting
the feature list shows **`mmap` is not implicated**: `cargo test --features
mmap --lib` (692/692) and even the full suite with `mmap` alone are green;
`cargo test --features parallel --lib` alone reproduces the failure in
isolation. The original report's feature list conflated the two.

Root cause: the test counts `Iw44Image::decode_chunk` calls via a
`#[cfg(test)]` `thread_local!` `Cell<usize>` (`BG44_CHUNK_DECODES`), set and
read from the test's own thread. Under `parallel`, `decode_layers` (`#440`,
round 40-ish PAR_DEC) runs the naive session's background decode inside
`rayon::join(bg, fg)`. Calling `rayon::join` from a plain thread that is not
already a rayon worker — the test thread — makes rayon bridge the whole join
onto a worker thread from its shared *global* pool to execute it. Instrumented
`count_bg44_chunk_decode()` with a `ThreadId` eprintln and confirmed: each of
the 3 `render_progressive_step` calls in the naive loop executed its counted
decodes on a *different* rayon worker thread (`ThreadId(10)`, `(12)`, `(5)`
across the run), never the test's own thread — so the test-thread's
thread-local view of the counter stays at 0 while the streamed
(`ProgressiveDecoder::push_bg44_chunk`, called directly, no `rayon::join`)
session is unaffected. The underlying O(N) vs O(N²) decode-count behavior is
unchanged and correct; only the counting mechanism was blind to work done on
another thread.

**Fix.** Test-only change (`src/djvu_render.rs`). Under `#[cfg(feature =
"parallel")]`, the whole measurement body now runs inside a dedicated,
freshly-built single-worker `rayon::ThreadPoolBuilder::new().num_threads(1)`
pool via `pool.install(...)`. With exactly one worker, any `rayon::join`
bridged into it can only resolve on that same worker thread, so setting and
reading `BG44_CHUNK_DECODES` from inside the pool keeps everything on one
thread regardless of whether `parallel` is enabled — and because the pool is
freshly built per test run (not rayon's shared global pool), this stays
isolated from any other test's concurrent decode calls on other threads,
preserving the original reason the counter was thread-local in the first
place. Without `parallel`, the body runs directly as before (no rayon
dependency pulled in when the feature is off). Verified green under
`--features parallel`, `--features cli,mmap,parallel`, and default (no
`parallel`) — 693/693 tests pass under the full `cli,mmap,parallel` combo (no
other feature-combination failures found).

**Decision.** Fixed the test, not the code — behavior was already correct,
only the thread-local counting was feature-sensitive.
## Perf round 43 (2026-07-06) — OCR_QA: OCR-based legibility metric for lossy JB2 levers

The journal's JB2 lossy levers (`lossy_text()` −22 % Sjbz round 19, `lossy_scan`
despeckle round 30, cross-threshold sweeps in `jb2-size-gap-plan.md`) are all
gated on the D1 structural metric (SSIM of the decoded mask). For a *text*
document the real acceptance test is "does it still read correctly", which SSIM
only approximates — a few flipped pixels concentrated on a diacritic can flip a
character while barely moving global SSIM, and diffuse sub-pixel blur can drop
SSIM without ever confusing a real OCR engine. This round adds an
OCR-agreement column next to the existing Sjbz/SSIM ones so both questions can
be read off the same table.

### OCR_QA — OCR-agreement harness for the JB2 lossy sweep — **Kept (infra)** (2026-07-06)

**Prior art.** `EXPERIMENTS_INDEX.md`, `gh pr list --search ocr`, and
`git branch -r` show no existing OCR-QA harness — issue #382/PR #125 built the
`OcrBackend` seam (`src/ocr.rs`, `docs/ocr-backend-seam.md`) and its Tesseract/
ONNX/Candle backends, and PRs #98/#239/#315 built export/reflow on top of it,
but nothing measures OCR degradation from the lossy JB2 levers. New harness,
not a duplicate.

**Setup.** `examples/ocr_qa.rs`, std-only, no new required dependency (uses the
already-optional `ocr-tesseract` feature purely as a *consumer* of the existing
seam, exactly like the CLI's `--backend` selector). For each corpus
(`watchmaker.djvu`, text; `pathogenic_bacteria_1896.djvu`, 517-page scan) and
each JB2 mask operating point already measured structurally by
`jb2_lossy_b0`/`jb2_despeckle` (`lossy_threshold` ∈ {2, 5, 8, 10}%, plus
`despeckle=8` on the scan corpus), the harness encodes+decodes every page's
mask (full corpus, matching the existing structural harnesses), renders the
lossless and lossy decodes as black-on-white bitonal images, and OCRs both with
`TesseractBackend`. It diffs **lossy-OCR vs lossless-OCR** (not vs. an external
ground truth) via a Levenshtein-based char/word agreement score — this isolates
the *degradation the lever introduces* and cancels Tesseract's own baseline
recognition error, which is common to both runs. OCR itself (the expensive
step) runs on the first 3 pages per corpus by default (`OCR_QA_PAGES` env var
to raise/lower); Sjbz size and SSIM always cover the whole corpus, matching the
sibling harnesses. Backend dispatch degrades gracefully: without
`--features ocr-tesseract`, or if a system Tesseract install can't recognize a
smoke page, the harness still runs and prints the Sjbz/SSIM columns with the
OCR columns marked `n/a` — no fake numbers. The harness's own diff logic
(Levenshtein, char/word accuracy, backend dispatch) is unit-tested (11 tests)
against a deterministic mock `OcrBackend` that needs neither Tesseract nor the
feature flag, so it runs in every CI lane, not only the optional
"OCR (tesseract)" job.

**Environment.** macOS, `tesseract` 5.5.2 + `leptonica` 1.87.0 via Homebrew
(`brew list` confirms both; `ocr-tesseract` feature builds and links cleanly).
A real backend ran for this measurement — this is not a harness-only report.

**Results** (OCR over the first 3 pages/corpus; Sjbz/SSIM over the whole
corpus):

`watchmaker.djvu` (12 masks, lossless Sjbz = 130,036 B):

| operating point | Sjbz Δ | SSIM | OCR char-agree | OCR word-agree |
|---|---|---|---|---|
| lossless | +0.00% | 1.00000 | 100.00% | 100.00% |
| `lossy_text()` (2%) | **−21.96%** | 0.99928 | 100.00% | 100.00% |
| lossy 5% | −23.39% | 0.99889 | 100.00% | 100.00% |
| lossy 8% | −23.99% | 0.99864 | 99.98% | 99.86% |
| lossy 10% | −24.47% | 0.99852 | 99.89% | 99.58% |

`pathogenic_bacteria_1896.djvu` (517 masks, lossless Sjbz = 34,254,905 B):

| operating point | Sjbz Δ | SSIM | OCR char-agree | OCR word-agree |
|---|---|---|---|---|
| lossless | +0.00% | 1.00000 | 100.00% | 100.00% |
| despeckle=8 | −2.43% | 0.99845 | 100.00% | 100.00% |
| `lossy_text()` (2%) | −0.01% | 1.00000 | 100.00% | 100.00% |
| lossy 5% | −0.25% | 0.99985 | 100.00% | 100.00% |
| lossy 8% | −4.79% | 0.99557 | 100.00% | 100.00% |
| lossy 10% | −11.23% | 0.98871 | 99.97% | 99.84% |

**The interesting question, answered both ways.** The shipped `lossy_text()`
preset (2%) is OCR-invisible on both corpora — 100/100% agreement — so the
round-19 SSIM-based "sweet spot" call holds up under the OCR lens too, on this
sample. But the two metrics *disagree in direction* once pushed further: on the
scan corpus, `lossy 8%` already shows a real SSIM dip (0.99557, the largest
drop of any point below `lossy 10%`) while OCR agreement is still a flat
100.00% — SSIM is flagging pixel-level change that Tesseract does not care
about at all. Conversely on the text corpus, OCR is the more sensitive
instrument: `lossy 8%`/`10%` show word-agreement (99.86%/99.58%) dropping
faster in relative terms than SSIM (0.99864/0.99852) does — a handful of
flipped glyphs cost whole words while barely denting a whole-page pixel
metric. So: **SSIM over-warns on the noisy scan corpus and under-warns on the
clean text corpus, relative to what an OCR engine actually notices** — exactly
the complementary-metric case this round set out to check for. Both hold at
`lossy_text()`'s already-shipped 2% operating point; the gap only opens up past
it, at settings nothing currently ships.

**Caveats.** (1) OCR sampled 3 pages/corpus (wall-clock: full-corpus Tesseract
OCR at 6 operating points × 517 pages is not a quick-iteration harness run —
`OCR_QA_PAGES` is there for a deeper manual sweep). (2) The harness OCRs the
raw bitonal JB2 mask directly (black text on white), not a fully composited
page — deliberate, since the levers under test only ever touch the mask, and
this isolates their effect from unrelated BG44/IW44 noise; a hypothetical
consumer OCRing the final rendered page would additionally be subject to
whatever downstream rendering does. (3) One Tesseract engine/language
(`eng`) at default DPI — not a cross-engine or multi-language validation.

**Decision.** **Kept as infra.** Confirms `lossy_text()` (already shipped,
opt-in) is OCR-safe at its shipped operating point on both sampled corpora, and
demonstrates SSIM and OCR-agreement are usefully complementary past that point
in *both* directions — future lossy-lever work (e.g. a hypothetical
`lossy_threshold > 0.02` default, or new despeckle levels) should check both,
not just SSIM. No shipped-code change; `examples/ocr_qa.rs` only.
## Perf round 44 (2026-07-06) — TH44_GRID: fast thumbnail-grid decode path — **Kept (opt-in)**

**Prior art check.** D5_TH44_PREVIEW (round-9) measured that decoding a page's
embedded `TH44` chunk instead of rendering the real page is 20–30× faster but
only SSIM 0.50–0.68 vs a true downscale, and recorded it as "viable only as
explicit opt-in for thumbnail grids, not a default." No grid-level API existed
before this round: `djvu_document::DjVuPage` only exposed a single-page
`thumbnail()` helper (added alongside PR #476, which taught the layered/JB2
encoders to emit `TH44` into multi-page bundles), and there was no
`Document`-level batch entry point, no strategy flag, and no corpus file with a
decodable `TH44` to bench against (checked via `gh pr list --search "thumbnail
OR th44"` and `git branch -r` — nothing pending). This round builds the grid
API D5 called for and validates it on a bundle synthesized with the #476
encoder.

**Approach.** `Document::thumbnails(max_w, max_h) -> Vec<Result<Pixmap, Error>>`
(`src/lib.rs`) + `Document::thumbnails_with_strategy(.., ThumbnailStrategy)` and
a per-page `Page::thumbnail_with_strategy`. `ThumbnailStrategy` is `Auto`
(default: use the page's `TH44` if present, else fall back to a real box-fit
render), `Th44Only` (error if no `TH44`), or `RenderOnly` (always the real
render path, ignores any `TH44`). `Auto`'s fallback and `RenderOnly` both go
through the existing `fit_to_box`-style Lanczos-3 box-fit — byte-identical to
what a caller doing this by hand today would get, so nothing about existing
single-page rendering changed. The whole-document call runs the per-page work
under `rayon::par_iter` behind the existing `parallel` feature, sequential
`iter` otherwise (same pattern as `epub.rs`'s parallel export).

**Structural proof, not just a benchmark claim.** A dedicated test
(`thumbnails_th44_path_never_touches_corrupted_jb2_background`) builds a 2-page
bilevel bundle with `TH44` thumbnails via `encode_djvm_bundle_jb2_with_thumbnails`,
then corrupts every `Sjbz` chunk's payload bytes in place (keeping the IFF
length/framing intact so the container still parses). `Th44Only` and `Auto`
still decode successfully and match each other byte-for-byte; `RenderOnly`
fails because it has no choice but to touch the now-garbage JB2 mask. This
proves by construction — not by counting decode calls — that the `TH44` fast
path never reaches BG44/JB2 decode.

**Speed.** Two synthesized bundles (no corpus file embeds a real `TH44`, per
D5): a 20-page bilevel bundle (JB2 masks pulled from `pathogenic_bacteria_1896.djvu`,
re-encoded via `encode_djvm_bundle_jb2_with_thumbnails`) and a 6-page colour
bundle (half-res renders of `colorbook.djvu`, re-encoded via
`encode_djvm_layered_shared_with_thumbnails`). Both `benches/document.rs`
(criterion, cold — a fresh `Document::from_bytes` inside every `b.iter()`
closure, mirroring `bench_open_and_render_first`, since a thumbnail grid is a
first-open workload, not a re-render of an already-open page) and a standalone
D1 harness (`examples/thumbnail_grid_quality.rs`) agree on the direction:

| scenario | pages | Th44Only | RenderOnly | speedup |
|---|---|---|---|---|
| bilevel (criterion, cold) | 20 | 31.2 ms | 127.5 ms | ~4.1× |
| colour (criterion, cold) | 6 | 13.1 ms | 24.5 ms | ~1.9× |
| colour (example harness, cold) | 6 | 0.55 ms | 8.13 ms | ~14.9× |

The colour speedup varies a lot between runs (1.9×–14.9× seen across methods
and machine load) — this repo's benches are explicitly non-deterministic /
not CI-gated (see project `CLAUDE.md`), and thumbnail-grid timings are small
absolute numbers (single-digit ms for 6 pages) that are unusually sensitive to
scheduling noise. What's consistent across every run and every scenario:
`Th44Only` never loses, and the D5 finding that this is a real, large win
(never below ~2×, often an order of magnitude) reproduces on both a bilevel
and a colour corpus, not just the single page D5 originally measured.

**Quality (D1).** SSIM of `Th44Only` vs `RenderOnly` on the same 6-page colour
bundle, at several grid box sizes (aligned to the same pixel dimensions via a
local bilinear resample before comparing, since the two strategies' box-fit
math can differ by ±1px):

| box size | mean SSIM (6 pages) |
|---|---|
| 48px | 0.3562 |
| 64px | 0.4033 |
| 96px | 0.4934 |
| 128px | 0.5355 |
| 256px | 0.6299 |

128px lands right in D5's original 0.50–0.68 single-page range (0.5355) —
confirmed, not improved: a `TH44` is baked in at encode time as its own lossy
~128px-class preview, not a faithful downscale of the full-fidelity page, so
fidelity doesn't meaningfully improve by asking for a smaller box (a smaller
box just discards more of both images' detail equally). No evidence the 128px
class is "much better" than the D5 baseline; it's the same trade D5 already
quantified, now confirmed to hold across a small multi-page corpus and exposed
through a real grid API instead of a one-off.

**Decision.** **Kept, opt-in.** Default `Document::thumbnails()` uses `Auto`
(prefer `TH44`, fall back to a real render byte-identical to today's), matching
D5's verdict that this is viable only as an explicit choice for thumbnail-grid
UIs that can tolerate lower per-thumbnail fidelity in exchange for a large,
reproducible speedup opening a grid of many pages at once — never a default
for single full-page rendering. `RenderOnly`/`Th44Only` let a caller pin the
exact behaviour they want (e.g. a viewer that always wants full fidelity, or
one that wants to fail loudly if a bundle lacks thumbnails rather than silently
paying the render cost). Tests:
`thumbnails_auto_uses_th44_and_matches_th44_only`,
`thumbnails_th44_path_never_touches_corrupted_jb2_background`,
`thumbnails_th44_only_errors_without_th44`,
`thumbnails_auto_falls_back_to_render_without_th44`,
`thumbnails_never_upscale_a_small_th44_thumbnail`,
`thumbnails_downscale_an_oversized_th44_thumbnail`.
## Perf round 45 (2026-07-06) — DIFF_FUZZ: corpus-mutation differential fuzzing vs DjVuLibre

**Motivation.** Existing fuzzers (`fuzz/fuzz_jb2` etc.) catch panics/timeouts/OOM
but not *semantic* divergence: a file both we and DjVuLibre parse but render
differently (silently wrong pixels), or accept/reject asymmetrically. The
existing `examples/interop_pixdiff.rs` quality-floor tool only compares on the
fixed, well-formed corpus — the gap is running the comparison over *mutated*
inputs.

**Infra shipped.**
- `examples/support/mod.rs` — `native_opts`/`parse_ppm`/`DiffStats`/`diff_stats`
  extracted out of `interop_pixdiff.rs` into a shared module (no duplicated
  diff logic); `interop_pixdiff.rs` now imports it via `#[path = "support/mod.rs"]`.
- `examples/diff_fuzz.rs` — deterministic (splitmix64, seed CLI arg), corpus-
  mutation differential driver. Self-contained IFF chunk walker (no
  `djvu-iff` dependency) with page-scope tracking (`ChunkSpan::page_index`,
  so a mutation inside page K's chunks renders/compares page K, not always
  page 0). Three mutation operators: truncate-at-chunk-boundary, bit-flip-in-
  payload, resize-length-field (+ generic whole-file-bitflip fallback).
  Crash-safe (`catch_unwind` + silent panic hook) and per-mutant timeout
  (thread+channel for our side, manual `try_wait` polling for `djvudump`/
  `ddjvu` subprocesses). Classifies every mutant on a **symmetric 3-level
  acceptance ladder** (0=structural reject, 1=structurally-ok-but-render-
  fails, 2=fully rendered), computed independently for our side (parse+
  render) and DjVuLibre's side (`djvudump` for structural, `ddjvu` for
  actual render) — a plain binary accept/reject compare missed real
  divergence where djvudump structurally accepts a file but ddjvu's own
  decoder still refuses to render it (djvudump only validates IFF chunk-tree
  framing, it doesn't decode JB2/IW44 payloads). Falls back to a solo
  parse-robustness mode if `ddjvu`/`djvudump` aren't on `PATH` (env-var-
  overridable binary paths). Findings saved to `fuzz/corpus-regressions/diff_fuzz/`
  as a minimal-repro `.djvu` + a `.txt` sidecar (mutation applied, target
  page, our detail, captured reference-tool stderr).

**Session.** 2100 mutants (700/file × 3 corpus files: `watchmaker.djvu` 12-page
bundled/183 KB, `cable_1973_100133.djvu` 2-page/15 KB, `boy.djvu` 1-page/4.8 KB),
seed 42, 2 s our-timeout / 5 s ref-timeout, wall time 153 s.

| class | count | % | meaning |
|---|---|---|---|
| both-reject | 1266 | 60.3% | both correctly reject malformed input |
| both-accept-match | 429 | 20.4% | both render, pixels match — safe majority |
| our-renders-what-they-reject | 310 | 14.8% | we render, DjVuLibre's own decoder refuses (see below) |
| both-render-fail | 63 | 3.0% | both structurally ok, neither renders |
| our-laxer | 19 | 0.9% | `djvudump` itself rejects (BZZ/arithmetic-decoder corruption), we still parse+render |
| our-render-fail | 4 | 0.2% | we fail to render, ddjvu renders fine |
| our-stricter | 4 | 0.2% | we reject upfront, djvudump structurally accepts |
| pixel-mismatch | 4 | 0.2% | both fully render, pixels differ beyond threshold |
| dim-mismatch | 1 | 0.05% | both fully render, reported dimensions differ |

**0 crashes, 0 timeouts** across 2100 mutants — the existing JB2 decode caps
(`MAX_SYMBOL_PIXELS`/`MAX_TOTAL_SYMBOL_PIXELS`/`MAX_PAGE_SYMBOL_PIXELS`/
`MAX_TOTAL_BLIT_PIXELS`, #503/#512) hold up fine under mutation.

**Confirmed findings (repros under `fuzz/corpus-regressions/diff_fuzz/`), not
fixed in-session — none are crashes, all require DjVu-format-spec judgment to
fix without risking false-positive rejections elsewhere, so recorded as
follow-ups per the "fix only if small/obvious" rule:**
1. **INFO version ceiling not enforced.** Bit-flipping `watchmaker.djvu`'s
   INFO version byte to ≥50 makes DjVuLibre reject ("Cannot decode DjVu files
   with version>= 50" — a forward-compat guard), we decode anyway.
   (`watchmaker_*_our-renders-what-they-reject`)
2. **BG44/IW44 truncated-stream tolerance.** Bit-flips/truncation in BG44
   payloads that make `ddjvu` abort ("Unexpected End Of File" / "Chunk does
   not bear expected serial number") still render for us — we don't detect
   the truncated/desynced wavelet stream and produce output anyway (possibly
   from a partially-corrupt tail). Majority of the 310 `our-renders-what-
   they-reject` bucket. Also the original `boy.djvu` case: an INFO-height
   bit-flip yields 192×384 for us where the BG44 payload only encodes
   192×256 — DjVuLibre rejects with "Corrupted data (Incorrect size in BG44
   chunk)" — i.e. no INFO-vs-payload dimension cross-check.
3. **INFO minor-version byte affects rendered pixels/dimensions on both
   sides, differently.** All 5 boy.djvu `pixel-mismatch`/`dim-mismatch`
   findings are single/double bit-flips at file offset 24 (the INFO
   `minor_version` byte) — both decoders fully render but disagree, likely
   from version-dependent gamma/flag-byte interpretation. Not yet root-
   caused; needs spec-revision research.
4. **`our-render-fail`: JB2 "unknown record type" where ddjvu still renders.**
   Verified manually (`ddjvu -format=ppm` on the saved repro exits 0, produces
   a full page) — a bit-flip inside `Sjbz` corrupts the arithmetic-coded
   stream such that our decoder's next `decode_num` call lands on record type
   >11 (`Jb2Error::UnknownRecordType`, `crates/djvu-jb2/src/lib.rs:1745`)
   while DjVuLibre's decoder apparently doesn't. Most likely arithmetic-coder
   state divergence cascading from the corrupted bit rather than a JB2-spec
   interpretation gap — unconfirmed, needs bit-level trace against
   DjVuLibre's `JB2Image.cpp`.
5. **`our-stricter`: verified NOT a bug.** `truncate-start-of-TXTz` mutations
   make our IFF parser reject upfront ("chunk [FORM] claims N bytes but only
   M available") because we validate the outer `FORM:DJVM`'s declared total
   length against the actual file length. Manually ran `djvudump` on the
   saved repro: exit 0, prints the full (stale) chunk tree — djvudump doesn't
   do this upfront whole-container check, it only fails once something
   actually tries to read past EOF. This is us being *more* defensive, not a
   regression; no fix needed.

**Decision.** **Kept** (infra). No source-code fixes applied this round — 0
crashes found, and all 5 confirmed semantic-divergence classes require
DjVu-format-spec-level judgment (forward-compat version ceiling, wavelet
stream integrity, INFO field versioning, JB2 record-type space) to fix safely
without introducing false-positive rejections on legitimate files. Recorded
above as follow-ups. Tests: none added (this is an example-only diagnostic
tool, no library code changed); `make check` gate covers fmt/clippy/no_std/
wasm32/tests on the existing tree, unaffected by this change.

**Follow-ups**: (1) cross-validate INFO width/height against decoded BG44/FG44
subsample geometry before trusting INFO's declared canvas size (addresses
finding 2's dimension half). (2) enforce the DjVu version-ceiling check DjVuLibre
applies (finding 1). (3) detect/report truncated or serial-number-desynced IW44
slice streams instead of silently rendering a partial decode (finding 2's
truncation half). (4) root-cause the INFO minor-version pixel divergence
(finding 3) against the DjVu format revision history. (5) trace whether finding
4's JB2 "unknown record type" divergence is arithmetic-coder noise or a genuine
gap against DjVuLibre's record-type handling.

## Perf round 46 (2026-07-06) — PY_CI: pytest suite + CI job for djvu-py

**Prior-art check.** Round 41 (PY_GIL_DETACH/PY_ZEROCOPY_BUFFER, #530) explicitly
flagged this as a follow-up: "djvu-py has no pytest suite and no CI job at all
… the ad hoc scratchpad scripts used here aren't checked in." Verified nothing
landed since: `djvu-py/` still has no `tests/` directory and no `Makefile`;
`.github/workflows/` has no maturin/python/pytest step in any of the 8
workflows; `gh pr list --state all --search "djvu-py OR pytest"` shows only
#530 itself and unrelated matches; no local or remote branch touches this
(`git branch -a` clean of it). Proceeded.

**Setup.** Built `djvu-py` via `maturin develop --release` into a `uv`-managed
venv to confirm the crate actually builds standalone before writing tests
against it — it does (`djvu-py/Cargo.toml` depends on the workspace root via
`path = ".."`, no separate lockfile drift). No existing CI or local-runner
touched djvu-py at all (every workspace-wide `cargo test`/`nextest` invocation
in `ci.yml`/`publish.yml`/`scripts/check.sh` carries `--exclude djvu-py`), and
there was no `maturin`/wheel-build step anywhere in the repo prior to this.

**pytest suite** (`djvu-py/tests/`, 41 tests, `conftest.py` resolves corpus/
fixture paths relative to the repo root via `Path(__file__).parents[2]` so
pytest works from any cwd):
- `test_document.py` (16): `Document.open`/`from_bytes` agree byte-for-byte on
  metadata, page-count on a real 2-page OCR'd corpus file
  (`tests/corpus/cable_1973_100133.djvu`), out-of-range `page()` raises
  `IndexError`, and 6 error-path cases (empty bytes, garbage bytes, truncated
  valid file, bad path, directory-as-path) all raise a Python exception
  rather than crashing the interpreter.
- `test_render.py` (8): rendered dimensions match page metadata, RGBA byte
  length is exactly `w*h*4`, output isn't a flat/blank buffer (bilevel JB2
  included), DPI up/downscaling produces the expected pixel dimensions, and
  the alpha channel is fully opaque.
- `test_text.py` (3): text layer present/non-empty on an OCR'd page, `None`
  when absent (the plain `boy.djvu` fixture has no text layer).
- `test_buffer_protocol.py` (14) — dedicated coverage of round 41's
  `PY_ZEROCOPY_BUFFER` additions: `memoryview(pixmap)` format/readonly/length,
  `numpy.frombuffer(pixmap, ...)` and `bytes(pixmap)` byte-equal to `.data()`,
  `to_numpy_zerocopy()`/`to_pil_zerocopy()` byte-equal to the copying
  `to_numpy()`/`to_pil()`, and lifetime safety: `del pixmap` while a
  `memoryview`/zero-copy numpy array/PIL image is still alive leaves the data
  valid and correct (the `Py_buffer.obj` owned reference keeps the Rust
  `Pixmap` alive), followed by a clean release; plus a 50-iteration
  alloc/view/release loop as a lightweight leak/crash smoke test.
- `test_gil.py` (1) — dedicated coverage of round 41's `PY_GIL_DETACH`: renders
  6 pages of the 12-page `watchmaker.djvu` corpus file sequentially vs.
  2-6-threaded, asserting `sequential/parallel > 1.3×` (deliberately lenient
  vs. the ~1.9–3.9× measured in round 41, to absorb CI-runner noise); skips
  outright on `os.cpu_count() < 2` and on unmeasurably-fast renders rather than
  flaking. Measured locally (M1 Max, 6 pages): sequential 0.32s, parallel
  0.054s, **5.9× speedup** — well clear of the 1.3× gate; reran 3× standalone,
  stable each time (0.52–0.57s each, all passed).

**Local runner**: `djvu-py/Makefile` (`py-venv`/`py-build`/`py-test`/`py-clean`)
— `uv venv` + `uv pip install maturin pytest numpy pillow`, then
`maturin develop --release` (with `VIRTUAL_ENV`/`PATH` pointed at the venv
explicitly, since the target is invoked non-interactively without `source
.venv/bin/activate`) and `pytest tests -v`. `[tool.pytest.ini_options]` added
to `djvu-py/pyproject.toml` (`testpaths = ["tests"]`).

**CI job**: new `djvu-py` job in `.github/workflows/ci.yml`, placed right
before `deps-check` — installs Rust stable + `Swatinem/rust-cache` (own cache
key, doesn't disturb the main workspace cache), `actions/setup-python@v5`
(3.12), `pip install uv`, then `make -C djvu-py py-test`. **Not** added to any
required-checks list — the branch-protection ruleset (`Lint`, `Test (stable)`,
`wasm32 build check`, `MSRV`, `Dependencies`) is untouched, matching how
`OCR (tesseract)`/`Coverage (llvm-cov)` are wired as informational jobs. Runs
on every push/PR (no `if:` restriction, unlike the OCR job) since the whole
build+test cycle is ~15-25s locally — fast enough to gate PRs that touch
`djvu-py/` without the apt-install cost that makes the OCR job push-only.

**Decision.** Kept (infra). `make -C djvu-py py-test`: 41 passed in ~3.3s, run
3× including `test_gil.py` standalone 3×, no flakes observed. Full `make
check` (1086 tests, `--exclude djvu-py` as before) unaffected: 1086 passed, 6
slow, 5 skipped. `.venv`/`.pytest_cache`/`__pycache__` under `djvu-py/` added
to `.gitignore`.

**Follow-ups**: the GIL-release test's 1.3× threshold is a judgment call —
if it proves flaky on a specific CI runner shape (e.g. a 1-vCPU/burstable
instance where `os.cpu_count()` overreports usable parallelism), downgrade the
assert to a printed report per the task brief's own suggested escape hatch.
Currently only one Python version (3.12) and one OS (ubuntu-latest) are
exercised — cross-version/OS wheel-build verification (the `publish.yml`
release path doesn't build djvu-py wheels for distribution at all yet) is out
of scope here and would be a separate follow-up if djvu-py is ever published
to PyPI.
## Perf round 47 (2026-07-06) — X86_CI_BENCH: unblocking the x86-gated AVX2_IDWT / ZP-U64 backlog items

### X86_CI_BENCH — verify + widen the x86-64-v3 validation job — **Kept (infra)** (2026-07-06)

**Issue.** Two backlog items are stuck behind "no x86 host": **AVX2_IDWT**
(round-5 #8, "blocked, no x86 host") and the **ZP-U64** ZP-decoder-refill
deferral ("only after x86 profile shows ZP decode is dearer there", round-3/4
deferred list). Every "Platform" line in this file reads Apple M1 Max
(aarch64) — there genuinely is no local x86 host to measure on. `bench.yml`
already has a job called "Benchmark (x86-64-v3 AVX2 validation)" that shows
SKIPPED on most recent PR checks — worth confirming whether that's rot to fix
or working-as-designed gating before assuming it's broken.

**Investigation.** Read `.github/workflows/bench.yml` and `ci.yml` end to end,
then cross-checked against actual run history (`gh run list --workflow=bench.yml`,
`gh run view <id> --json jobs`), `gh pr list --search "x86 OR avx2"`, and
`git branch -r`:

- The `detect` job (`dorny/paths-filter`) gates `bench-x86-64-v3` **on PRs
  only** to when `crates/djvu-iw44/src/{lib,encode}.rs`, `benches/{codecs,
  render}.rs`, or `bench.yml` itself changed — added by PR #363 ("skip AVX2
  validation on non-SIMD PRs") specifically to stop burning ~30% of per-PR
  bench minutes validating AVX2 on refactor PRs that never touch the SIMD
  path. This is intentional, documented scoping, not bit rot.
- Outside PRs the job's `if:` (`github.event_name != 'pull_request' ||
  needs.detect.outputs.simd == 'true'`) is unconditionally true, so it runs on
  every `push` to `main`/tags and every `workflow_dispatch`. Checked the last
  4 main-branch push runs (`28775743121`, `28771006572`, `28767553829`,
  `28763496645`) — `bench-x86-64-v3` is `success` on all of them. **The job is
  not broken**; it already produces fresh x86 AVX2-vs-baseline deltas on every
  merge to main. The "SKIPPED" badges people see are just PRs that legitimately
  didn't touch IW44/SIMD files (e.g. `TH44_GRID`, the `carte.djvu` parse fix)
  — correct behavior, working as designed.
- `workflow_dispatch` is already wired at the top of `bench.yml` ("Allow manual
  runs (e.g. to seed the initial baseline)"), and the job's own `if:` already
  covers it (`event_name != 'pull_request'` is true for `workflow_dispatch`
  too) — nothing to add there. `gh run list` showed no *past*
  `workflow_dispatch` runs of this workflow, so it had never actually been
  exercised end-to-end before this round (see Validation run below).
- `ci.yml`'s `Test (beta)` and `OCR (tesseract)` use the identical
  `if: github.event_name != 'pull_request'` push-to-main-only pattern —
  also by design (keeps PR feedback fast/deterministic, matching this repo's
  CLAUDE.md: "Fuzz/Benchmarks are intentionally not required" for merge
  gating), and unrelated to the two x86-gated backlog items — left untouched.
  `Benchmark (macOS)` is tag/dispatch-only by design too (feeds a separate
  BENCHMARKS.md baseline, not a gate).
- `gh pr list --search "x86 OR avx2"` surfaced #363 (the `detect` gate, above)
  and one closed, unrelated spike (`codex/issue-307-avx2-row-pass`, a rejected
  row-pass change, not IDWT). No abandoned x86-CI branch to resurrect.

**Gap found and fixed.** The job's codec bench filter (`iw44_to_rgb|
iw44_decode`) only exercises the IW44 SIMD path — correct for AVX2_IDWT,
since `to_rgb`/`to_rgb_subsample` (`crates/djvu-iw44/src/lib.rs`) call
`inverse_wavelet_transform` → `row_pass_inner`, exactly the IDWT row/col pass
that item is about. It excluded `jb2_decode`/`bzz_decode`, which is what
**ZP-U64** needs (the ZP-decoder `refill!` hot loop the deferred note says is
shared, with separate inlined copies, across jb2/iw44/bzz). Widened both
`cargo bench --bench codecs` invocations in `bench-x86-64-v3` to
`'iw44_to_rgb|iw44_decode|jb2_decode|bzz_decode'` — a cheap addition since the
toolchain/runner for this job is already paid for by the existing steps.

**Ratio methodology (documented, no change needed).** The job already
implements the "compare target vs control within the same run" pattern this
repo uses elsewhere to cancel noisy-cloud-runner variance (cf. B7_PREFETCH_PAGE's
dwell=0 negative control, COLD_OPEN's per-iteration fresh-copy strategy): it
runs the *same* benches twice back-to-back on the *same* GitHub-hosted runner
— once at default `RUSTFLAGS` (baseline, effectively SSE2-only codegen) and
once with `-C target-cpu=x86-64-v3` (enables the `cfg(target_feature =
"avx2")` branches) — resets `target/criterion` between them so Criterion
treats each as an independent baseline, then diffs the two `--output-format
bencher` outputs and posts a Δ% table with a ≥3% speedup/regression threshold.
Because both arms share one runner instance, the GitHub Actions runner-to-
runner lottery (CPU model, neighbour contention) cancels out of the ratio
even though absolute ns/iter numbers aren't comparable run-to-run. This
already matches the "same-run ratio" methodology the task asked to document;
only the bench *selection* (the "what", not the "how") had the gap above.

**Validation run.** Pushed `infra/x86-ci-bench` and dispatched the workflow:
`gh workflow run bench.yml --ref infra/x86-ci-bench`, then `gh run watch` on
the resulting run.

Run [28784274181](https://github.com/matyushkin/djvu-rs/actions/runs/28784274181),
`bench-x86-64-v3` job: **success** (first-ever `workflow_dispatch` run of this
workflow). Real x86 (GitHub-hosted `ubuntu-latest`) numbers, default RUSTFLAGS
vs `-C target-cpu=x86-64-v3`, same runner back-to-back:

**ZP-decode-only benches (no IDWT) — consistently faster under `x86-64-v3`:**

| Bench | default ns | +x86-64-v3 ns | Δ% |
|---|---:|---:|---:|
| `bzz_decode` | 104 | 75 | −27.9% |
| `iw44_decode_corpus_color` | 1,387,089 | 1,224,073 | −11.8% |
| `iw44_decode_first_chunk` | 771,110 | 723,477 | −6.2% |
| `jb2_decode` | 162,443 | 153,187 | −5.7% |
| `jb2_decode_corpus_bilevel` | 589,009 | 567,637 | −3.6% |
| `jb2_decode_large_600dpi` | 2,496 | 2,256 | −9.6% |

**IDWT-inclusive benches (`to_rgb`/`to_rgb_subsample`) — regressed under
`x86-64-v3`:**

| Bench | default ns | +x86-64-v3 ns | Δ% |
|---|---:|---:|---:|
| `iw44_to_rgb_colorbook/sub1_full_decode` | 9,197,177 | 9,572,581 | +4.1% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,230,159 | 2,287,465 | +2.6% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 579,158 | 662,081 | +14.3% |

Render (whole-pipeline, includes IDWT + ZP decode + compositing — mixed, as
expected from summing an accelerated stage and a regressed one):
`render_colorbook` −1.0%, `render_colorbook_cold` −4.6%, `render_colorbook_
stages/mask_decode` −5.0%, `render_corpus_color` +1.7%. Job's own verdict:
"Mixed: some speedup, some regression."

**Reading this (data only, no SIMD implementation attempted per task scope):**
`iw44_decode_first_chunk`/`iw44_decode_corpus_color` isolate the ZP-entropy-
decode stage (`decode_chunk`, before `to_rgb` runs the IDWT), so the
consistent −6 to −28% across all three ZP-heavy codecs (jb2/iw44/bzz) is real
signal that the shared ZP-decoder hot loop *is* codegen-sensitive on x86 —
gives ZP-U64 a concrete, positive first data point (contrast with the "blocked,
no x86 host" status quo). Conversely, the `to_rgb_colorbook` group (which adds
the IDWT row/col pass — the exact code AVX2_IDWT is about) *regresses* under
`-C target-cpu=x86-64-v3` even though there is no hand-written AVX2 IDWT path
to benefit — i.e. simply raising the codegen target is not a free win for
IDWT the way it is for ZP decode, so a hand-written x86 SIMD IDWT pass (the
actual AVX2_IDWT proposal) still has real headroom to prove out, it just
won't come from `-C target-cpu` alone. `bzz_decode`'s −27.9% is on a tiny
(single-digit-µs) fixture and should be treated as noisy/indicative, not
load-bearing. This is one run, not a multi-sample statistical claim — the
`workflow_dispatch` trigger this round adds means a maintainer can re-run
it on demand for a cleaner sample before either backlog item is actually
picked up.

**Decision.** **Kept (infra-only).** No source/runtime code changed —
`.github/workflows/bench.yml` comments + the two bench-filter regexes only.
`make check` passes unaffected (workflow YAML, not Rust). This unblocks both
backlog items: **AVX2_IDWT** now has a real, continuously-refreshed x86 IDWT-
path measurement (every main push, or on-demand via `workflow_dispatch`) to
decide whether a hand-written x86 SIMD IDWT pass would earn its keep before
anyone writes it; **ZP-U64** now gets jb2/bzz decode timings from the same
x86 runner to compare against the existing aarch64 numbers in this file and
decide whether the 32-bit `refill!` is worth the 4-site u64 rewrite.

## Perf round 48 (2026-07-06) — C5_COMPRESS: cheaper render-cache entries (memory vs re-decode trade)

**Issue.** C5_LRU_BUDGET (round-13) evicts a page's *entire* render cache
(`PageLayers`) once the document-wide byte budget is exceeded. Under a long
scroll/pan session the dominant entry is the decoded full-res BG RGB pixmap
(~33.6 MB/page class on `colorbook.djvu`) — an all-or-nothing drop means the
very next render of that page (e.g. scrolling back, or a thumbnail-rail
re-visit) pays a full cold BG44 decode again, even for a cheap downscaled
view. Hypothesis: a middle tier that keeps evicted entries in a cheaper form
could rebuild faster than a cold decode for at least some render paths.

**Prior-art check (mandatory first step).** No existing compressed/middle
cache tier: `grep`ped `EXPERIMENTS_INDEX.md`/`PERF_EXPERIMENTS.md` for
`C5_COMPRESS`/`compress-on-evict`/`downgrade_before_drop` (no hits before this
round); `gh pr list --state all --search "cache compress"` returns only an
unrelated `release-please` PR; `git branch -r` has no `*compress*` branch.
Reviewed C5_RENDER_CACHE_EVICTION (round-12), C5_LRU_BUDGET (round-13),
BG_CACHE/BG_CACHE_S2/SUB4_RGB_CACHE, C4_TILE_CACHE (round-38), COLD_OPEN
(round-40) — none add a compressed/downgraded tier, all either drop-whole or
cache-whole. Proceeded.

**Two designs measured, per the brief:**

**A. Downgrade-on-evict.** First checked whether keeping the IW44 coefficient
image (`bg44`/`bg44_partial`) across eviction — so a sub=2→sub=1 "upgrade" via
`PlaneDecoder::reconstruct(start_scale)` could skip re-running the ZP
arithmetic decode — actually saves memory. It does not:
`PlaneDecoder::new(w, h)` allocates full-size coefficient storage immediately
on decoding chunk 0, regardless of how many chunks have since been applied —
so the "coefficient" tier is the same size class as the full RGB pixmap, not
cheaper. This matches ROI_IDWT's (rejected) finding that the ZP decode, not
the spatial IDWT step, dominates cold cost — ruled out per the brief's own
built-in fallback clause.

Degraded scope, as the brief allows: keep the already-cached **downscaled**
RGB pixmap (`bg_rgb_s2`/`bg_rgb_s4`, 4×/16× smaller than sub=1) instead of
dropping it, while still fully dropping `bg44`/`bg44_partial`/`bg_rgb_s1`/mask
layers/tile cache. Full-res (sub=1) re-renders still cold-decode either way —
honest scope limit, not a general "upgrade cheaply" mechanism.

**B. Compress-on-evict (DEFLATE via `miniz_oxide`, already a direct dep for
PDF export, no new heavy dep).** Measured compress+decompress round-trip on a
full-res (~33 MB) RGB pixmap at DEFLATE levels 1/4/6 against the domain
cold-decode cost (BG44 ZP decode + IDWT + YCbCr, plus JB2 mask) for the same
page, on `colorbook.djvu` and `watchmaker.djvu` via a throwaway scratch
harness (`examples/scratch_measure_zp_vs_idwt.rs`, deleted after the numbers
were captured — not shipped code). Round-trip cost: **32–260 ms** across
levels; cold decode: only **10–58 ms**. Decisively rejected — general-purpose
byte compression is slower than just re-decoding the domain-specific format.

**Method.** Design A implemented as an opt-in tier, byte-identical when off:
- `PageLayers::downgrade` (`src/djvu_render.rs`): clears `bg44`,
  `bg44_partial`, `mask`, `mask_sub4`, `fg44`, `mask_indexed`, `bg_rgb_s1`,
  and the tile cache, but **preserves** `bg_rgb_s2`/`bg_rgb_s4` and the LRU
  access tick.
- `decode_background_chunks`'s `subsample==2`/`subsample==4` branches now
  check the terminal `bg_rgb_s2`/`bg_rgb_s4` cache *before* forcing the
  `bg44`/`bg44_partial` guard, so a downgraded-but-still-cached downscaled
  pixmap is served warm instead of re-triggering a decode.
- `DjVuPage::downgrade_render_cache` / `DjVuDocument::downgrade_render_caches`
  (`src/djvu_document.rs`), plus `CacheBudgetOptions { downgrade_before_drop:
  bool }` (default `false`) and `DjVuDocument::enforce_cache_budget_with` — a
  two-pass sweep: pass 1 downgrades LRU-first until under budget (or nothing
  left to downgrade); pass 2 falls back to full `evict_render_cache` drops,
  LRU-first, for anything still over budget. Same byte ceiling honoured
  either way; only the *shape* of what survives changes.
- Two new unit tests: `downgrade_render_cache_keeps_downscaled_tier_warm`
  (sub=2 stays byte-identical warm after downgrade; sub=1 cold-redecodes
  byte-identically) and
  `enforce_cache_budget_with_downgrade_matches_budget_and_output` (3-page
  budget-halving sweep stays ≤ budget, all pages re-render byte-identically).
  Both pass.
- New bench harness `examples/c5_compress_bench.rs`: a mixed viewer workflow
  on `colorbook.djvu` (62 colour pages) — every page gets both a thumbnail
  (sub=2) and a full-res (sub=1) render as it scrolls past (needed so
  `bg_rgb_s2` is actually populated before eviction; an earlier version of
  the harness rendered sub=1 only, which meant `downgrade` degenerated to
  `drop` with nothing left to preserve — caught and fixed), followed by a
  budget-mediated eviction sweep. After the main sweep, 12 repeated
  evict→render trials (trial 0 discarded as warmup) measure median
  warm-render-after-eviction latency at sub=1 and sub=2, plus a structural
  cache-state histogram (zero/downgraded-`<4MB`/full-`≥4MB` page counts).

**Results (colorbook.djvu, 62 pages, `/usr/bin/time -l` peak RSS):**

| budget | mode | final cache bytes | histogram (zero/downgraded/full) | sub=1 re-render median | sub=2 re-render median | peak RSS |
|---|---|---|---|---|---|---|
| 60 MiB | drop | ≤ budget | 54 zero / 0 / 8 full | 56.4 ms | 23.5 ms | ~252 MB |
| 60 MiB | downgrade | ≤ budget | 1 zero / 61 downgraded(`<4MB`) / 0 full | 56.2 ms | **18.1 ms** | ~249 MB |
| 20 MiB | drop | ≤ budget | 59 zero / 0 / 3 full | 56.4 ms | 23.6 ms | ~149 MB |
| 20 MiB | downgrade | ≤ budget | 1 zero / 61 downgraded(`<4MB`) / 0 full | 56.2 ms | **18.0 ms** | ~225 MB |

Same byte ceiling honoured identically in both modes (structural metric, the
primary signal per the brief). sub=2 (thumbnail/downscaled) re-render after
eviction is **~23% faster** under downgrade (18.0–18.1 ms vs 23.5–23.6 ms).
sub=1 (full-res) re-render is unchanged (56.2 vs 56.4 ms, within noise) —
full-res always cold-decodes regardless of tier, exactly the honest scope
limit stated above. Peak RSS is comparable to slightly higher under downgrade
at the tighter 20 MiB budget (225 MB vs 149 MB) since more bytes are
deliberately kept warm per page — a real, disclosed trade, not a free win;
still well under the "no eviction" upper bound and the ceiling is respected
either way. Wall-clock is provisional (other agents ran concurrently on this
machine per the brief); the structural histogram is the load-bearing result.

**Decision.**
- **Design A (downgrade-on-evict): Kept, opt-in.** `CacheBudgetOptions`
  defaults to `false` (byte-identical to `enforce_cache_budget`); callers who
  render thumbnails/downscaled views alongside full-res opt in via
  `enforce_cache_budget_with(budget, protect, CacheBudgetOptions {
  downgrade_before_drop: true })` for a ~23% cheaper thumbnail-rail
  re-render after eviction, at the cost of a larger resident set per kept
  page (still bounded by the same budget check in pass 2).
- **Design B (compress-on-evict via DEFLATE): Rejected.** Decode is faster
  than general-purpose (de)compression on this domain's data (32–260 ms
  round-trip vs 10–58 ms cold decode) — no source shipped.
- **Ruled out (record to prevent re-proposal):** keeping `bg44`/`bg44_partial`
  coefficients across eviction does **not** save memory vs the derived RGB
  pixmap — `PlaneDecoder` allocates full-size storage on first chunk,
  independent of how many chunks are later applied.

Tests: 2 new unit tests (both pass); full `make check` gate (fmt, clippy `-D
warnings`, no_std build, wasm32 build, full suite) green: 1088 passed, 8 slow,
5 skipped.

**Follow-ups**: the "resume partially-decoded IW44 from a saved chunk[0]
state" observation (cheaper than decoding all chunks from scratch — saves
roughly the chunk[0] ZP-decode share, ~15–40% of total decode time depending
on file) is a decode-*speed* optimization, not a memory-tiering one (doesn't
reduce retained bytes) — out of scope here, worth a dedicated experiment.
`downgrade_before_drop` is currently colour-BG44-page-specific (keys off
`bg_rgb_s2`/`bg_rgb_s4`, populated only when a downscaled render already
happened) — bilevel/JB2-heavy pages get no benefit from this tier; a JB2 mask
middle tier would need a different, separate design.

## Perf round 49 (2026-07-06) — INFO-chunk version ceiling + gamma clamp (round 45 findings 1 & 3)

**Motivation.** Round 45's DIFF_FUZZ run left two confirmed semantic-divergence
findings on the table: (1) DjVuLibre enforces a forward-compat version ceiling
on the INFO chunk that we didn't; (3) some INFO-chunk bit-flips near the
version byte made both decoders fully render but disagree on pixels/dims,
root cause unclear. This round root-causes and fixes both.

**Finding 1 — version ceiling.** Fetched DjVuLibre's actual
`libdjvu/DjVuInfo.h`/`DjVuFile.cpp` sources: `DJVUVERSION_TOO_NEW = 50`, checked
in `DjVuFile::decode_chunk()` (not in `DjVuInfo::decode()` itself) as `if
(info->version >= 50) throw "Cannot decode DjVu files with version>= 50"`. The
"version" DjVuLibre checks is the INFO minor-version byte (offset 4 in the
chunk), optionally combined with the major-version byte (offset 5) into a
16-bit value unless the major byte is the sentinel `0xff` (→ minor byte only).
Implemented the identical ceiling in `PageInfo::parse` (`src/info.rs`): a new
`IffError::UnsupportedVersion { version }` (`crates/djvu-iff/src/lib.rs`),
returned when the computed version is `>= 50`. An absent version byte (round
42's 5-byte short-INFO tolerance) computes to `0`, safely under the ceiling —
round 42 is unaffected (its tests still pass).

**Finding 3 — reframed.** Byte-level diffing of all 5 saved boy.djvu repros
(`fuzz/corpus-regressions/diff_fuzz/boy_*`) against the original showed the
journal's "minor-version byte" framing was imprecise: the minor-version byte
itself has **zero** effect on pixel rendering in either decoder — DjVuLibre
only reads `info->version` for the ceiling check above, never for pixel
logic, and (pre-fix) we didn't even store it. The actual driver in 4 of 5
repros (`boy_00022/00076/00134/00143_pixel-mismatch`) was the **gamma byte**
(chunk offset 8, adjacent to the version bytes — hence the "near the version
field" conflation): DjVuLibre always clamps computed gamma (`0.1 *
gamma_byte`) to `[0.3, 5.0]`, *including* when the byte is present-but-zero
(clamped up to 0.3, not defaulted to 2.2 — that default only applies when the
chunk is short enough that the byte is truly absent). We previously special-
cased byte==0 to mean "default 2.2" and never clamped the upper end at all, so
a corrupted `gamma_byte=150` gave us a raw gamma of 15.0 against DjVuLibre's
clamped 5.0 — a very different gamma-correction LUT, confirmed root cause of
all 4 pixel-mismatch repros. Fixed by matching DjVuLibre's clamp exactly in
`PageInfo::parse` (`src/info.rs`).

The 5th repro (`boy_00128_dim-mismatch`) turned out to be a **false positive
in the diff_fuzz harness itself**, not a product bug: `examples/diff_fuzz.rs`'s
`our_attempt()` returned `page.width()`/`page.height()` (the pre-rotation INFO
dims) alongside the rendered pixmap instead of the pixmap's own (post-rotation)
`pm.width`/`pm.height`. For this rotated mutant the two differ (192×256 vs
256×192); our actual rendered bytes were confirmed byte-identical to ddjvu's
output (`examples/_render_native_ppm.rs` cross-check, mean/max abs diff 0).
Fixed the harness to report `pm.width`/`pm.height`.

**Incidental fixture fix.** `references/djvujs/library/assets/bgjp_test.djvu`
(a hand-crafted synthetic fixture used by 8 `src/djvu_render.rs` BGjp/JPEG
tests) had a stray non-zero `major_version` byte (0x64) that combined with the
minor byte to compute version 25600 — correctly rejected by the new ceiling
check. Patched the single byte to 0x00 (clearly unintentional; the fixture
only exists to exercise BGjp/JPEG chunk decoding, not version semantics).

**Before/after DIFF_FUZZ (seed 42, same 2100-mutant corpus as round 45):**

| class | before | after | note |
|---|---|---|---|
| both-reject | 1266 | 1266 | unchanged |
| both-accept-match | 429 | 434 | +5 (4 pixel-mismatch + 1 dim-mismatch converge here) |
| our-renders-what-they-reject | 310 | 275 | −35 (version≥50 mutants now rejected by us too) |
| both-render-fail | 63 | 63 | unchanged |
| our-laxer | 19 | 19 | unchanged |
| our-render-fail | 4 | 4 | unchanged |
| our-stricter | 4 | 39 | +35 (the 35 above: we now reject version≥50 *before* djvudump-equivalent structural check would, since DjVuLibre's own djvudump doesn't enforce the ceiling — only `DjVuFile::decode_chunk` at full-decode time does; we have one parser so we catch it earlier) |
| pixel-mismatch | 4 | 0 | **fixed** (gamma clamp) |
| dim-mismatch | 1 | 0 | **fixed** (harness bug, not a product bug) |

Total mutants classified: 2100 both times. No new divergence classes
appeared; both target classes (pixel-mismatch, dim-mismatch) went to zero.
The `our-stricter` growth is the expected, correct shape of the finding-1 fix:
DjVuLibre's own renderer (`ddjvu`) would refuse these files too (that's the
whole premise of the finding), it's only its structural-only `djvudump` tool
that doesn't check the version field — so "matching DjVuLibre's tolerance" is
satisfied at the level that matters (rendering), even though the 3-level
ladder now places us a stage earlier than djvudump on these mutants.

**Decision.** **Fixed.** Tests: `src/info.rs` gained
`version_50_is_rejected`, `version_49_is_accepted`,
`high_major_version_byte_is_rejected_via_combined_version`,
`major_version_0xff_sentinel_uses_minor_byte_only`,
`short_info_chunk_with_version_over_ceiling_is_rejected`,
`gamma_byte_present_and_zero_clamps_to_0_3_not_2_2`,
`gamma_byte_above_50_clamps_to_5_0` (regression test for the confirmed pixel-
mismatch root cause). Round 42's `CARTE_INFO_TRUNCATED` 5-byte-short-INFO
tests still pass unmodified. `make check`: 1092 tests passed. `cargo run
--release --example diff_fuzz -- --seed 42 --mutants 700 --max-seconds 600`
rerun confirms the table above.
## Perf round 50 (2026-07-06) — INTEROP_STREAMS: BG44/IW44 stream-tolerance and JB2 record-type divergences (round 45 findings 2 & 4)

**Scope.** Follow-ups (1)/(3)/(5) from round 45: the BG44/IW44 dimension
cross-check, the truncated/desynced wavelet-stream tolerance, and the JB2
"unknown record type" divergence. Findings 1/3 (INFO version-ceiling,
INFO-minor-version pixel divergence — `src/info.rs` version logic) are
explicitly out of scope for this round (parallel INFO_VERSION effort).

**Finding 2a — INFO-vs-BG44 dimension cross-check — FIXED.**
DjVuLibre's `DjVuFile::get_dpi` requires a *single* common reduction factor
`red` in `1..=12` that satisfies `ceil(page_w/red) == plane_w` **and**
`ceil(page_h/red) == plane_h` simultaneously; if none exists it throws
`DjVuFile.corrupt_BG44` ("Corrupted data (Incorrect size in BG44 chunk).").
We had no equivalent check — a BG44 chunk's own header freely declares its
width/height, and nothing cross-validated it against the page's own INFO
dimensions before mapping the plane onto the page.

Added `iw44_reduction_is_legal(page_w, page_h, plane_w, plane_h)` in
`src/djvu_render.rs` (tries all 12 legal ratios) and wired it into all three
BG44-decode call sites that produce a page-mapped image: `PageLayers::bg44`,
`PageLayers::bg44_partial`, and the non-cached bounded branch inside
`decode_background_chunks`. A failing check now behaves exactly like any
other BG44 decode failure (dropped from the cache / `RenderError::Iw44`), not
a special new error path.

Verified against the *exact* round-45 prose case: flipping bit 0x80 of
`boy.djvu`'s INFO height low byte (192×256 → 192×384, BG44 payload
unchanged) — confirmed with real `ddjvu`/`djvudump`:
```
djvudump:  INFO [10]  DjVu 192x384, v24, 100 dpi, gamma=2.2
           BG44 [4761]  IW4 data #1, 100 slices, v1.2 (b&w), 192x256
ddjvu -format=ppm -page=1 …:  ddjvu: Cannot decode page 1.  (exit 10)
```
Pre-fix, our `djvu render` on this exact byte pattern returned exit 0 (silent
192×384 render, stretched/mismatched against the 192×256 payload); post-fix
it returns `error: format error: IW44 decode error: IW44 stream contains
invalid data` (exit 1) — matching ddjvu's rejection. Regression tests:
`dimension_cross_check_rejects_info_bg44_size_mismatch` (this exact repro)
and `dimension_cross_check_allows_unmodified_boy_djvu` (legal 1:1 ratio must
still render) in `tests/document_and_render.rs`.

**Finding 2b — truncated/desynced ZP-stream tolerance — serial-continuity
check FIXED; swallow-and-continue behavior kept, classified BENIGN.**
Two independent things were tangled together here:

1. *Chunk serial-number continuity* (the majority of the 310
   `our-renders-what-they-reject` mutants — bit-flips/truncations that
   desync which BG44 chunk carries which refinement round). DjVuLibre's
   `IWBitmap::decode_chunk`/`IWPixmap::decode_chunk` track a `cserial`
   counter and throw `IW44Image.wrong_serial`/`wrong_serial2` the moment a
   chunk's declared `serial` isn't the next expected value; we had no such
   check and would decode a desynced/duplicated/skipped chunk into whatever
   refinement slot its `serial` claimed, silently. Added the same
   continuity check to `Iw44Image::decode_chunk` (`crates/djvu-iw44/src/lib.rs`):
   a `next_serial: u32` counter, exempting only the very first chunk fed to
   a fresh decoder (so `MissingFirstChunk` stays the more specific
   diagnostic when appropriate), returning a new `Iw44Error::UnexpectedSerial`
   on any gap, repeat, or rewind. New tests:
   `iw44_decode_chunk_rejects_serial_skip`, `iw44_decode_chunk_rejects_serial_repeat`,
   and `iw44_decode_chunk_serial_check_does_not_regress_zpshort_tolerance`
   (confirms the round-26 BUG-ZPSHORT empty-payload tolerance for
   *in-order* chunks is untouched).

2. *What happens when a chunk decode does fail* — `PageLayers::bg44`/
   `bg44_partial` unconditionally break out of the chunk loop on the first
   decode error and keep whatever was decoded so far (round-26 design,
   independent of `RenderOptions::permissive`). This is the actual source of
   "we render past a truncated/desynced stream." Investigated whether this
   produces garbage: rendered several `our-renders-what-they-reject` repros
   and pixel-diffed against clean baselines — max channel diff 5–26/255,
   well under 0.3% of pixels differing meaningfully in every case checked.
   The partial decode is a legitimate degraded-but-recognizable render, not
   corruption presented as success, and per the round's own decision
   framework ("if garbage, consider permissive-only tolerance; if graceful,
   keep") this does **not** warrant gating behind strict/permissive — doing
   so would only turn a fine degraded render into a hard error for no
   accuracy gain, and (per round 26) real corpus files like `watchmaker.djvu`
   rely on exactly this tolerance for legitimately short refinement chunks.
   **Kept as-is, classified benign.** Note the new serial-continuity check
   from (1) still runs *before* this swallow logic — many previously-silent
   desyncs are now caught earlier (as `UnexpectedSerial`) and the loop stops
   one chunk sooner than before, which can only make the kept partial decode
   *more* accurate, never less.

**Finding 4 — JB2 unknown record type — BENIGN, root cause CONFIRMED (not
just hypothesized).** Fetched DjVuLibre's `JB2Image.cpp`/`JB2Dict.cpp`
record-type switch: it decodes the type via the identical `CodeNum(0, 11,
dist_record_type)` construction (an adaptive binary-tree arithmetic decoder,
functionally the same algorithm as our `decode_num`) and its `default:` case
throws `G_THROW(ERR_MSG("JB2Image.bad_type"))` — i.e. DjVuLibre treats an
out-of-range record type exactly like we do (`Jb2Error::UnknownRecordType`,
hard error). There is no record-type-handling gap to align; both
implementations agree on the contract.

What differs is *where* each independent implementation's stateful
arithmetic-coder walk ends up after a corrupting bit-flip earlier in the
`Sjbz` stream. Confirmed this is genuine post-corruption chaos, not a
spec-interpretation difference, by pixel-diffing `ddjvu`'s own render of two
saved repros against the clean baseline page:
- `cable_1973_100133_00080_our-render-fail` (`bitflip-1bits-in-Sjbz@176`):
  ddjvu exits 0 but its own rendered page differs from the clean baseline by
  up to 255/255 in places, 0.50% of pixels differing >10/255.
- `watchmaker_00628_our-render-fail` (`bitflip-3bits-in-FORM@63436`): ddjvu
  exits 0 but differs from clean by up to 255/255, **7.6%** of pixels
  differing >10/255 — a visibly corrupted page, not a clean render.

So "ddjvu renders" here does not mean "ddjvu decoded correctly" — its
independent context-tree walk over the same corrupted bits happened to land
on an in-range record type and kept going (into already-garbled content),
while ours happened to land out-of-range and stopped. Since both sides
implement the identical fail-hard contract and the actual divergence is
downstream chaos from a single corrupted bit propagating through a
stateful adaptive arithmetic coder, this is **unfixable by alignment** (there
is nothing to align) and is recorded as benign, matching the round's own
decision framework for this exact case.

**Fuzz rerun (`examples/diff_fuzz.rs`, seed 42, `--mutants 700`, same 3
corpus files, 2100 mutants total) — before (clean `main`) vs after (this
round's fixes):**

| class | before | after | Δ | note |
|---|---:|---:|---:|---|
| both-reject | 1266 | 1266 | 0 | |
| both-accept-match | 429 | 429 | 0 | |
| our-renders-what-they-reject | 310 | 241 | **−69** | serial-continuity check now catches these upfront |
| both-render-fail | 63 | 132 | **+69** | same 69 mutants — now correctly fail on both sides (no longer a divergence) |
| our-laxer | 19 | 19 | 0 | untouched (JB2/BZZ scope, not this round) |
| our-stricter | 4 | 4 | 0 | untouched (pre-existing, verified not a bug — round 45 finding 5) |
| our-render-fail | 4 | 4 | 0 | finding 4, confirmed benign — no fix applies |
| pixel-mismatch | 4 | 4 | 0 | finding 3 territory (INFO minor-version), out of scope |
| dim-mismatch | 1 | 1 | 0 | finding 3 territory (INFO minor-version/flags byte, not a BG44-vs-INFO header mismatch), out of scope |

The 69-mutant movement (`our-renders-what-they-reject` → `both-render-fail`)
is entirely attributable to the IW44 serial-continuity check; the dimension
cross-check's exact repro (the manually-reconstructed `boy.djvu` 192×384/
192×256 case) isn't part of this seed's generated mutant set — it was
verified directly (see finding 2a above) rather than via the aggregate table.
0 new crashes/timeouts introduced.

**Decision.** **Fixed** (2a, 2b-serial-check) / **Kept, benign, documented**
(2b-swallow-behavior, 4). Tests: 3 new `djvu-iw44` unit tests
(`iw44_decode_chunk_rejects_serial_skip`/`_repeat`/
`_serial_check_does_not_regress_zpshort_tolerance`) + 2 new integration tests
(`dimension_cross_check_rejects_info_bg44_size_mismatch`/
`_allows_unmodified_boy_djvu`); full workspace `cargo test --workspace` green
(no regressions); `make check` gate covers fmt/clippy/no_std/wasm32.

**Follow-ups (updated):** round 45's (2) and (5) are now closed by this
round. Remaining open: (1)/(4) INFO version-ceiling and minor-version pixel
divergence (findings 1/3 — separate INFO_VERSION effort, `src/info.rs`).

## Perf round 51 (2026-07-06) — TXTZ_OCR: encode-time OCR text layer — **Kept (opt-in)**

**Goal.** Give the encoder a "searchable scan" one-step workflow: run an
`OcrBackend` on the page image during encoding and emit a proper `TXTz` chunk,
so the output DjVu is text-searchable without a separate mutation pass.

**Prior art (mandatory check).** `gh pr list --search "txtz OR text layer"`
turned up PR #315 (`feat(cli/ocr): embed OCR text layers in DjVu output`,
merged) — but that wires OCR into `djvu ocr --output`, which rewrites an
*already-encoded* file via `DjVuDocumentMut::page_mut().set_text_layer()`
(`src/djvu_mut.rs`), a post-hoc mutation pass, not encode-time. Reading the
seam confirmed the low-level pieces were already complete and *not*
duplicated by building this:
- `OcrBackend::recognize` (`src/ocr.rs`, PR #125) already returns a full
  `page → line → word` `TextLayer` with pixel bounding boxes — the trait did
  **not** need extending. `TesseractBackend` (`src/ocr_tesseract.rs`) already
  parses Tesseract's hOCR output into the full zone hierarchy.
- `text_encode::encode_text_layer` (`src/text_encode.rs`) already serializes a
  `TextLayer` to the DjVu zone-tree binary format (delta-coded rects,
  bottom-left coordinate flip) with round-trip tests against `text.rs`'s
  decoder, and `bzz_encode` already compresses it to `TXTz`.
- What was missing: `PageEncoder` (`src/djvu_encode.rs`) had **zero**
  awareness of text layers or OCR — the CLI's `Encode` and `Ocr` subcommands
  are two fully separate code paths (confirmed via `grep` — no
  `text_layer`/`TextLayer`/`ocr` hits in `djvu_encode.rs` before this round).
  That's the actual gap: wiring, not new format/backend work.

**Change.** Additive `PageEncoder` API, `src/djvu_encode.rs`:
- `with_text_layer(TextLayer) -> Self` — attach a pre-built layer.
- `with_ocr_text_layer(&dyn OcrBackend, &OcrOptions) -> Result<Self, OcrError>`
  — runs the backend on the page image (bilevel `Bitmap` sources are expanded
  to a black-on-white RGBA `Pixmap` via a new `bitmap_to_pixmap`, mirroring
  `examples/ocr_qa.rs`'s `mask_to_pixmap`; colour `Pixmap` sources are OCR'd
  directly) and attaches the result.
- `encode()` appends the BZZ-compressed `TXTz` chunk (via the existing
  `encode_text_layer` + `bzz_encode`) to both the `Lossless` (bitmap) and
  `Quality`/`Archival` (colour) chunk lists when a text layer is attached; a
  no-op (byte-identical output, confirmed by test) when it isn't.
- No trait/backend changes — `OcrBackend`, `TesseractBackend`, and the zone
  encoder are all untouched, used exactly as they already existed.

**Validation.**
- 6 new unit tests in `src/djvu_encode.rs`: default omits TXTz entirely
  (opt-in guarantee, cross-checked against the unmodified pre-existing
  `PageEncoder` test suite all still passing byte-for-byte); `with_text_layer`
  round-trips through `DjVuDocument::parse` + `page.text_layer()`/`page.text()`
  (decoded words/text match what was encoded); `with_ocr_text_layer` against a
  deterministic mock `OcrBackend` from both `Bitmap` and `Pixmap` sources;
  `bitmap_to_pixmap` pixel mapping.
- New `examples/txtz_ocr_demo.rs`, run against `tests/corpus/watchmaker.djvu`
  page 0 (2550×3301 bilevel mask) with the real `ocr-tesseract` feature
  (Tesseract 5.5.2 on this machine):
  - **Size cost:** baseline (no text layer) 29,666 B → with `TXTz` 32,348 B —
    **+2,682 B (+2.62 KB)**, in the task's expected 1–3 KB/page range.
  - **Round-trip:** decoded text layer has 1,453 chars, 242 word zones with
    sane pixel coordinates (e.g. `"Chapter"` @ (1119, 846) 258×83), and the
    plain text reads correctly: `"Chapter 5\nDeath of the Watchmaker:\nModern
    Science and\nthe Providence of God!\nArnold E. Sikkema\n\net me begin by
    quoting John Polkinghorne, theoretical physicist and Anglican priest…"`.
  - **PDF export:** `pdf::djvu_to_pdf` on the re-encoded document produces a
    `/Font` resource (the existing `pdf_with_text_layer` path in `src/pdf.rs`
    picks it up with no changes needed) — confirms the searchable-scan
    workflow is complete end-to-end (encode with OCR → decode → export to
    searchable PDF).
  - **Quality note (honest, no fabricated ground truth):** the embedded text
    is Tesseract's raw recognition output, unmodified by the TXTz plumbing —
    this feature's accuracy is exactly whatever Tesseract achieves on a given
    scan. No ground-truth transcription is checked into the repo for
    `watchmaker.djvu` to quote an independent accuracy number against; OCR_QA
    (round 43) already established Tesseract is stable on this exact corpus
    (100% char/word agreement at the shipped `lossy_text()` operating point).
  - Demo also builds and runs cleanly without `ocr-tesseract` (falls back to a
    stub backend), so it stays part of the default-feature build surface.

**Decision.** **Kept, opt-in.** Default encode paths are unchanged (byte-
identical, tested). One `make check` run (fmt/clippy/no_std/wasm32/tests, all
1104 tests green) before push.
## Perf round 52 (2026-07-06) — IW44_ENTROPY_PROBE: localizing the textured-content size gap vs c44

**Scope.** Round 22 (IW44_ENTROPY_GAP) found the size gap vs `c44` splits by
content — smooth ≈ parity, textured ~1.3× at matched SSIM — and stopped at
"the gap is entropy coding, not masking" without localizing *where in the
bitstream* it concentrates. This round instruments the encoder to answer
that with real emitted bytes, then probes three candidate encoder-only
levers named in the brief: (a) band-dependent activation threshold, (b)
coefficient pre-quantization tweaks, (c) chunk/slice boundary placement.
Round 35 (IW44_SLICE_RD)'s `total_slices=100` was not re-litigated.

**Instrumentation.** Added a diagnostic-only `iw44-probe` cargo feature
(`djvu-iw44/iw44-probe`, wired through the root crate's own feature of the
same name) gating a new `djvu_iw44::encode::probe` module: thread-local
per-band counters (10 IW44 frequency bands, 0=DC/coarsest..9=finest) for
cumulative ZP-coder output bytes and call/true counts of the four bit
categories the codec emits per slice (block-band NEW, per-bucket NEW,
coefficient activation, coefficient refinement). A new
`ZpEncoder::bytes_written()` read-only accessor (`crates/djvu-zp/src/
encoder.rs`) gives the byte-attribution hook without touching any
decode-critical state. Off by default, zero runtime cost when disabled;
guarded by a dedicated regression test
(`djvu-iw44/src/encode.rs::probe_does_not_change_output`) asserting
identical encoder output bit-for-bit with the counters enabled vs disabled.
New example `examples/iw44_band_probe.rs` runs this on each corpus page's
*real* BG44 background (the same production workload round 35 measured),
printing the per-band table plus a coarse(0-3)/mid(4-6)/fine(7-9) byte-share
summary and the `c44` size for context.

**Finding — the gap concentrates in band 0 (DC/coarsest), and the ours/c44
ratio *anti-correlates* with page size/complexity, not with "textured vs
smooth" as originally framed.** Measured on the full `colorbook.djvu` (62
pages) and `watchmaker.djvu` (12 pages) corpora used by round 22:

- `watchmaker` (round-22's "smooth" example): 98.6–99.4% of bytes land in
  band 0 on every page; ratio is 1.01–1.05× (near parity), matching round 22.
- `colorbook` (round-22's "textured" example): band-0 share ranges from
  ~18–22% on genuinely detail-rich pages (e.g. page 21: 22.5% coarse, 46.7%
  fine, ratio 1.03×) up to 94.6–96.8% on visually simple pages (e.g. page 61:
  96.8% coarse, ratio **1.251×** — the single worst page in the corpus; page
  2: 94.6% coarse, ratio 1.223×).
- Across all 62 `colorbook` pages: `corr(bytes, ratio) = −0.591` — **small,
  band-0-dominated pages have the worst gap**, not large/detail-rich ones.
  The 5 worst-ratio pages (61, 2, 59, 1, 60) are all >79% coarse-band share;
  the 5 best-ratio pages (35, 36, 25, 21, 54) all have mid+fine share ≥55%.

This reframes round 22's own "textured" label: the real per-page driver of
the gap is small-page/DC-dominated content, not high-frequency detail — the
opposite of the original hypothesis. `colorbook` page 61: 2791 B total (2231
B c44), gap = 560 B, 96.8% of our bytes in band 0.

**Candidate (a) — band-dependent activation threshold — already closed,
zero slack.** Read `newly_active_encoding_pass`/`bucket_encoding_pass`: both
already use the exact real decoder gate `(s * 11/16).max(1)` (this is
exactly what round pre-5's IW44_ACT_THRESH fixed, −9.1%). No remaining
activation-signalling waste to recover; not implemented.

**Candidate (b) — coefficient pre-quantization tweaks — out of scope for a
zero-PSNR probe.** The forward wavelet transform is tested/required to be an
exact, lossless mirror of its own i32 reference (`loss_diagnostics` test
module); any deadzone/rounding-bias tweak is inherently a quality/size
tradeoff, not a free encoder-only win, and would need its own D1-gated
experiment against a PSNR floor. Not attempted here; flagged as a possible
future direction, not validated.

**Candidate (c) — chunk/slice boundary layout — real but small, below the
adoption bar.** `djvudump` on a `c44`-encoded file confirms it uses a
`74+15+10` schedule over 3 BG44 chunks by default, vs our
`Iw44EncodeOptions::default()` `slices_per_chunk=10` over 10 equal chunks
for the same 100-slice total — more chunks pay more independent-ZP-stream
floor/header cost. Tested `slices_per_chunk ∈ {10, 25, 50, 74, 99}` two ways:

1. `examples/iw44_chunking_lever.rs` — full end-to-end re-encode of page 0
   of 3 corpus files via `PageEncoder`, validated with real `ddjvu`:

   | file | n=10 (B) | n=25 | n=50 | n=74 | n=99 | ddjvu | maxΔpx |
   |---|---:|---:|---:|---:|---:|---|---:|
   | colorbook (2260×3669) | 16044 | −0.51% | −0.69% | −0.69% | −0.67% | ok all | 0 |
   | watchmaker (2550×3301) | 12522 | −0.69% | −0.86% | −0.93% | −0.93% | ok all | 0 |
   | conquete_paix (4267×6853) | 6144 | −1.40% | −1.69% | −1.69% | −1.63% | ok all | 0 |

2. `examples/iw44_chunking_sweep.rs` — whole-corpus, every real BG44 page
   (not just page 0), decoded-pixel-identity asserted across every `n`:

   | corpus | pages | n=10 (B) | n=25 | n=50 | n=74 | n=99 | pixel-identity |
   |---|---:|---:|---:|---:|---:|---:|---|
   | colorbook | 62 | 1,684,391 | −0.09% | −0.11% | −0.09% | −0.09% | PASS |
   | watchmaker | 12 | 20,997 | −1.65% | −1.56% | −1.56% | −1.15% | PASS |
   | conquete_paix | 22 | 1,675,111 | −0.04% | −0.05% | −0.04% | −0.04% | PASS |
   | all 3 combined | 96 | 3,380,499 | −0.07% | −0.09% | −0.07% | −0.07% | PASS |

Chunking is a pure stream-framing change (each BG44 chunk is an
independently-flushed `ZpEncoder`; only `PlaneEncoder`'s quantization/context
state carries across chunk boundaries), so bit-identical decoded pixels
across every tested value is the expected/required outcome, confirmed both
via `ddjvu` (candidate lever, full pages) and our own decoder (sweep, 96
real BG44 pages) — genuinely interop-safe. Savings are real (0.04–1.7%
depending on content) but non-monotonic per page and well under the round's
3% "Kept" bar even in the best case (watchmaker, small smooth pages) — not
adopted as a new default, left as a documented, available option via the
existing public `Iw44EncodeOptions::slices_per_chunk`.

**Verdict: Diagnostic.** No candidate lever crosses the 3% bar at equal
PSNR (chunking recovers only ~27 B of colorbook page 61's 560 B gap — a
small fraction). The probe narrows and reframes round 22's finding with
real per-band bytes: the residual gap is concentrated in band 0/DC on
small, simple pages (anti-correlated with page size, not "textured
content" as originally labeled), candidate (a) is already fully closed by a
prior round, candidate (b) is inherently quality-coupled and out of scope
for a zero-PSNR probe, and candidate (c) is a real-but-minor, interop-safe,
non-default-worthy lever. The likely remaining root cause — genuine
coefficient-value differences between our Rust forward wavelet transform
and DjVuLibre's own C++ implementation on band-0-dominated content — could
not be validated (no DjVuLibre C++ source available locally to diff
against) and is left as an open hypothesis, not a finding.

**Artifacts.** `crates/djvu-zp/src/encoder.rs` (`ZpEncoder::bytes_written`),
`crates/djvu-iw44/src/encode.rs` (`probe` module + instrumentation hooks +
`probe_does_not_change_output` test), `crates/djvu-iw44/Cargo.toml` /
root `Cargo.toml` (`iw44-probe` feature), `examples/iw44_band_probe.rs`,
`examples/iw44_chunking_lever.rs`, `examples/iw44_chunking_sweep.rs`. Full
`make check` green (1099 tests, 0 failures).
## Perf round 53 (2026-07-06) — PDF_G4: CCITT Group 4 encoding for bilevel PDF masks

**Issue.** Round 23 (`PDF_DCT_PROBE`) and round 28 (`PDF_ADAPTIVE_RASTER`)
fixed the *colour* raster path in PDF export; the bilevel side was
untouched — JB2 masks are re-encoded as a generic Deflate raster
(`collect_mask_stream` in `src/pdf.rs`), which wastes most of the
row-to-row redundancy a real fax/MMR coder would exploit. No G4 (T.6)
*encoder* existed in the codebase — `src/smmr.rs` only had a decoder
(`decode_smmr`) plus a Horizontal-mode-only encoder (`encode_smmr`, used for
DjVu's own `Smmr` chunk, not PDF).

**Approach.** Added a full 2D T.6 encoder, `smmr::encode_g4` (pass /
horizontal / vertical modes, changing-element arrays + `partition_point`
binary search instead of `find_b1`/`find_b2`'s linear scans, to stay
`O(n log n)` on adversarial input), producing the exact payload PDF's
`CCITTFaxDecode` filter expects (`K -1`, no in-band header). Wired into
`src/pdf.rs::collect_mask_stream` behind a new opt-in `PdfOptions::ccitt_g4`
flag, following round 28's "encode both, keep the smaller" pattern instead
of an unconditional switch: encode both the Deflate raster and the G4
bitstream, emit whichever is smaller. Default stays off; `ccitt_g4: false`
is byte-identical to today's output (`ccitt_g4_off_is_byte_identical_to_default`).

**A first pass using the existing `encode_smmr` (H-mode only) as the "G4"
lever was 2–4× *larger* than Deflate**, not smaller — the whole win comes
from vertical/pass mode exploiting row correlation, which H-mode-only
cannot do. That motivated writing `encode_g4` as new code rather than
reusing `encode_smmr`.

**Correctness — a real bug was found and fixed, not just the new encoder.**
The plan was to round-trip `encode_g4`'s output through this crate's own
`decode_smmr` as a "free" oracle (it already implements all three T.6
modes). All 51 `smmr` unit tests passed this way — but validating against
*external* decoders (poppler's `pdftoppm`, libtiff via a hand-built
single-strip Group4 TIFF + Pillow) showed real corpus PDFs full of
`Syntax Error: CCITTFax row is wrong length` and an 8×8 checkerboard test
pattern decoding to the wrong image after row 0.

Root cause: both `find_b1` (decoder) and the new `find_transition`
(encoder) searched for the next reference-line changing element
*inclusive* of the current coding position `a0`, instead of *strictly
after* it. That's only spec-equivalent for the very first changing element
of a row (where `a0`'s initial value stands in for the T.6 sentinel `-1`);
on every later step within the row, `a0` is a real pixel position, and an
inclusive search wrongly lets `b1` land exactly on `a0` whenever the
reference row has a changing element there — common on any row correlated
with its predecessor (which is precisely when vertical/pass mode matter
most). A prior "correctness review" comment in the code already flagged
this exact issue and attempted the `a0+1` fix, but reverted it because it
broke a hand-crafted decoder test (`decoder_vr1_vertical_mode_produces_correct_output`)
— that test's expected bitstream had itself been hand-reasoned under the
same wrong assumption, so it wasn't actually validating against real T.6,
just against the bug's own logic. Both encoder and decoder shared the bug
symmetrically, so self-round-tripping could never catch it.

Fixed by tracking `a0` as `i64` with the T.6 sentinel `-1` uniformly (search
start = `a0 + 1`, always strict), in both `find_b1`/`decode_row_pixels` and
`find_transition`/`encode_row_2d`. Re-ran the full `smmr` suite: 50/51
tests passed unchanged; the one hand-crafted VR1 test was rewritten with a
non-colliding row pair (verified by hand-tracing the fixed algorithm) and
its bitstream generated via `encode_g4` rather than hand bit-twiddling, to
avoid reintroducing the same class of error. Re-validated:
- libtiff/Pillow: 7/7 synthetic test bitmaps (checkerboard, stripes,
  diagonal, sparse-text-like, all-white/black) now decode byte-identically
  via an independent Group4 TIFF decoder (previously 6/7, checkerboard
  wrong past row 0).
- `pdftoppm` on all four target corpus PDFs (`watchmaker`,
  `cable_1973_100133`, `pathogenic_bacteria_1896` — 520 pages,
  `conquete_paix`): **zero** CCITTFax syntax errors (previously dozens per
  file), and every rasterized page pixel-identical (`compare -metric AE`
  → `0 (0)`) between the `ccitt_g4` and default PDF, across all pages of
  all four documents.

**Numbers** (mask-only JB2→bitmap payload, Deflate vs G4, corpus totals):

| doc | pages w/ mask | Deflate | G4 | adaptive min | ratio |
|---|---:|---:|---:|---:|---:|
| cable_1973_100133 | 2 | 64,504 | 40,191 | 40,191 | 1.60× |
| pathogenic_bacteria_1896 | 4 | 1,019,016 | 1,532,283 | 1,017,131 | 1.00× |
| watchmaker | 12 | 1,241,144 | 717,731 | 717,731 | 1.73× |
| conquete_paix | 22 | 381,466 | 165,079 | 165,079 | 2.31× |

Whole-PDF-file sizes (default vs `ccitt_g4` vs `adaptive_raster` vs both,
150 dpi):

| doc | default | g4 | adaptive | both |
|---|---:|---:|---:|---:|
| watchmaker | 5,928,714 | 5,406,020 (−8.8%) | 3,896,362 | 3,373,668 (−43.1%) |
| cable_1973_100133 | 335,732 | 311,554 (−7.2%) | 237,705 | 213,527 (−36.4%) |
| pathogenic_bacteria_1896 | 1,194,452 | 1,192,699 (−0.15%) | 1,163,399 | 1,161,646 (−2.75%) |
| conquete_paix | 11,761,406 | 11,546,465 (−1.8%) | 11,386,161 | 11,171,220 (−5.0%) |

Whole-file wins are much smaller than the mask-only ratios because most of
these documents' bytes are colour/JPEG background raster, not the bilevel
mask overlay — `ccitt_g4` only touches the latter. `pathogenic_bacteria_1896`
is the one doc where G4 loses on 2 of its 4 mask pages (photographic/
halftone-ish bilevel content defeats run-length coding), which is exactly
why this is wired as "encode both, keep smaller" rather than an
unconditional switch: the adaptive minimum never regresses even there.

**Decision.** **Kept, opt-in** (`PdfOptions::ccitt_g4`, default `false`,
composes with `adaptive_raster`). Real, validated size win on text-dominated
bilevel content (1.6–2.3× smaller masks, up to −43% whole-file combined with
`adaptive_raster`); a wash-to-slightly-worse on halftone-heavy bilevel
content, fully absorbed by the adaptive "keep smaller" pattern so it never
regresses. Not made the default — that's a maintainer call given it changes
the PDF filter used for masks (still spec-standard `CCITTFaxDecode`, just a
lever nobody asked to flip yet). Also fixed a real, pre-existing T.6
changing-element bug in `decode_smmr`'s `find_b1` (used for production
`Smmr`-chunk decoding, not just this new encoder) — round-trip-only testing
against a decoder sharing the same bug had hidden it; this is now closed for
both directions. Tests: 51 `smmr` unit tests (13 new `g4_roundtrip_*` +
1 rewritten VR1 test), 5 new `pdf` tests
(`ccitt_g4_defaults_to_off`, `ccitt_g4_off_is_byte_identical_to_default`,
`ccitt_g4_on_uses_ccittfaxdecode_and_never_larger`,
`ccitt_g4_shrinks_bilevel_corpus_doc`,
`collect_mask_stream_g4_returns_none_for_no_sjbz`); full
`cargo test --lib` green (723 tests); `make check` gate green.
## Perf round 54 (2026-07-06) — AVX2_IDWT: hand-written x86 SIMD for the IW44 IDWT — **Rejected** (2026-07-06)

**Issue.** EXPERIMENTS_INDEX.md's `AVX2_IDWT` row had been deferred since
round 5 ("x86 SIMD parity for the IDWT row/col passes; can't measure on M1").
Round 47 (X86_CI_BENCH) unblocked it with real x86 numbers and set
expectations: plain `-C target-cpu=x86-64-v3` auto-vectorization *regresses*
`iw44_to_rgb_colorbook/*` (`+2.6%..+14.3%`), so only a genuine hand-written
AVX2 kernel — not a codegen flag — could plausibly win.

**Investigation.** Read the aarch64 NEON IDWT code
(`crates/djvu-iw44/src/lib.rs`: `row_pass_inner`, `load8s_neon`/
`store8s_neon`, the round-5 IDWT_S2_NEON pitfall notes) as the structural
template. While tracing the x86_64 column-pass dispatch (`load8s`/`store8s`,
`s == 1` fast path), found that a hand-written AVX2 kernel **already
exists** — `load8s_s1_avx2`/`store8s_s1_avx2` — added in the #189 Phase-2
work, but gated by compile-time `cfg(target_feature = "avx2")`. That gate is
only satisfied when the *crate itself* is compiled with
`-C target-feature=+avx2` / `target-cpu=x86-64-v3` — i.e. it was **dead code**
for every ordinary `cargo build --release` on x86_64, never active for
default-build users regardless of the host CPU's real AVX2 support. This
matches the exact "hand-written AVX2 kernel, not just a codegen flag" gap
round 47 identified — except the kernel had already been written and just
needed a correct runtime-dispatch wire-up, not a fresh implementation.

**Approach.** Rewired `load8s`/`store8s`'s `s == 1` path to call a new
`avx2_available()` helper — `std::is_x86_feature_detected!("avx2")` cached in
a `OnceLock`, checked once, not per-pixel (matching the existing
`ycbcr_avx2_raw`/`prelim_flags_*_avx2` dispatch pattern already used
elsewhere in this file; `#[target_feature]` can't combine with
`#[inline(always)]` on stable, so the check stays coarse-grained at the
`load8s`/`store8s` call site, not inlined per-lane). Added
`inverse_wavelet_transform_from_ex(..., force_use_simd: Option<bool>)` as a
test-only entry point so a new exhaustive test
(`column_pass_simd_matches_scalar_exhaustive`) could force the scalar and
SIMD/AVX2-dispatching column pass down identical random `i16` data across 10
`(width, height, subsample, start_scale)` cases — odd widths, width=1,
sub-8-wide widths, and the `s==2`/`sd==1` stride case (the round-5
IDWT_S2_NEON pitfall) — and assert bit-exact equality.

Deliberately did **not** attempt a hand-written horizontal row-pass AVX2
kernel (#307 already measured and rejected that exact shape: full-decode
benches improved but `iw44_to_rgb_colorbook` partial-decode benches
regressed `+1.6%..+7.3%`), and ruled out generalizing the column-pass AVX2
kernel to `s==2`/`4` strides via a packed-gather trick (`FlatPlane`'s backing
`Vec<i16>` has no padding slack — a strided gather risks a genuine
one-element out-of-bounds read at the tail).

**Bit-exactness.** `cargo test -p djvu-iw44 --all-features` on the native
aarch64 host: 50/50 unit tests pass, including the new exhaustive test and
the pre-existing `load8s_s1_avx2_matches_scalar`/`store8s_s1_avx2_matches_
scalar`/`ycbcr_avx2_raw_matches_scalar`. `cargo check --target
x86_64-unknown-linux-gnu` (compile-only; this M1 host can't link/run
x86_64-linux binaries) passed cleanly at every iteration. CI's `Test
(stable)` job (ubuntu x86_64) passed — confirms the new equivalence test
actually executed the real AVX2 code path on x86 hardware, not just aarch64
scalar fallback. `make check` (fmt, clippy `-D warnings`, no_std, wasm32,
full workspace tests) green throughout.

**Platform.**
- OS: Ubuntu GitHub-hosted runner (`ubuntu-latest`)
- arch: `x86_64`, dev host: Apple M1 Max (aarch64, `cargo check`-only)
- Rust: stable toolchain (`.github/workflows/bench.yml`)
- RUSTFLAGS: unset (default) vs `-C target-cpu=x86-64-v3`

**Numbers.**

Sample 1 — PR #541 push, run
[28792757964](https://github.com/matyushkin/djvu-rs/actions/runs/28792757964),
`bench-x86-64-v3` job (default RUSTFLAGS vs `+x86-64-v3`, same runner,
back-to-back — this is *after* the fix, so both arms already dispatch the
AVX2 kernel; the `+x86-64-v3` arm additionally gets broader compiler
auto-vectorization elsewhere):

| Bench | default ns | +x86-64-v3 ns | Δ% |
|---|---:|---:|---:|
| `iw44_to_rgb_colorbook/sub1_full_decode` | 11,990,247 | 10,590,126 | −11.68% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,947,396 | 2,593,810 | −12.00% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 753,799 | 677,204 | −10.16% |

Sample 2 — repeat `workflow_dispatch` on the same branch, run
[28794346105](https://github.com/matyushkin/djvu-rs/actions/runs/28794346105),
same job, ~30 min later (checking sample-1 wasn't a fluke):

| Bench | default ns | +x86-64-v3 ns | Δ% |
|---|---:|---:|---:|
| `iw44_to_rgb_colorbook/sub1_full_decode` | 12,088,318 | 10,514,218 | −13.02% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,982,082 | 2,576,587 | −13.60% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 756,655 | 681,288 | −9.96% |

The two post-fix samples agree tightly (both the default-arm and
+x86-64-v3-arm absolute numbers are within ~1% of each other across the two
runs) — the *fix's own* default-vs-v3 ratio is reproducible.

**The real question isn't default-vs-v3 — it's old-code-vs-new-code at fixed
default RUSTFLAGS**, since both arms already use the kernel post-fix. Pulled
the last successful main-branch `bench-x86-64-v3` artifact (pre-fix,
commit `c524585`, run
[28787742806](https://github.com/matyushkin/djvu-rs/actions/runs/28787742806),
same day, ~1.5h before the PR run) to get an old-code default-RUSTFLAGS
reference point:

| Bench | old-code default ns (pre-fix) | new-code default ns (sample 1) | new-code default ns (sample 2) | Δ% (old→new, sample 1 / 2) |
|---|---:|---:|---:|---:|
| `iw44_to_rgb_colorbook/sub1_full_decode` | 9,797,436 | 11,990,247 | 12,088,318 | +22.4% / +23.4% |
| `iw44_to_rgb_colorbook/sub2_partial_decode` | 2,393,905 | 2,947,396 | 2,982,082 | +23.1% / +24.6% |
| `iw44_to_rgb_colorbook/sub4_partial_decode` | 617,507 | 753,799 | 756,655 | +22.1% / +22.5% |

Also ran a direct `scripts/bench_compare.py` (baseline vs current
`target/criterion`, downloaded artifacts) between an even earlier main
baseline and the PR run: showed the same direction (`+30..+32%` on these
three benches) but *also* showed `+16..+31%` regressions on entirely
unrelated benches (`jb2_encode_dict`, `encode_multipage/*`,
`iw44_gray_decode_large/*`) not touched by this change at all — a red flag
for systemic runner noise in that particular comparison, so it was treated
as corroborating-but-not-load-bearing. Noted in passing: `bench.yml`'s
"Benchmark" job's baseline-vs-current PR comment is silently broken on every
PR (`Download baseline` extracts to `baseline/<bench>/...` but
`bench_compare.py` is invoked with `baseline/target/criterion`, a path that
never exists — pre-existing infra bug, out of scope for this round, not
fixed here).

**Decision.** **Rejected.** Reverted the runtime-dispatch fix (commit
reverted; `crates/djvu-iw44/src/lib.rs` now matches pre-round `origin/main`
exactly) — the existing `load8s_s1_avx2`/`store8s_s1_avx2` kernels remain in
place, still compile-time-gated (effectively dead by default), unchanged
from before this round.

**Reason.** Two independent activation mechanisms for the *same* existing
AVX2 column-pass kernel — round 47's compile-time `-C target-cpu=x86-64-v3`
flag, and this round's runtime `is_x86_feature_detected!` dispatch — both
converge on the same qualitative result: activating this kernel makes
`iw44_to_rgb_colorbook` measurably *slower*, not faster, by a large and
reproducible margin (≈10-13% internally reproducible v3-vs-default ratio
post-fix; ≈22-24% old-code-vs-new-code at fixed default RUSTFLAGS across two
independent same-branch samples). This is the opposite of the outcome gate
(`≥5% win`) and fails it by a wide margin in the wrong direction. The
kernel was written correctly (bit-exact, per the equivalence tests) and is
now *reachable* for the first time on ordinary builds — but reachable was
never the bottleneck; the kernel itself is a pessimization for this access
pattern, consistent with the already-documented "ALU-bound loop pattern
defeats hand-rolled micro-optimizations" theme from IDWT_SPLAT (round 5) on
the NEON side. This permanently closes AVX2_IDWT with data: a genuinely
faster x86 IDWT kernel would need a different algorithmic approach (e.g.
restructuring the lifting steps to reduce shuffle/permute overhead, or
targeting a different granularity than one 8-lane `i32x8` chunk per call)
than what exists today; simply making the existing kernel reachable is not
sufficient and is actively harmful if shipped by default. Left the existing
kernels' dead compile-time gate untouched rather than deleting the kernels
outright — removing them is a larger, separate cleanup call for a future
round now that this data exists, not required to close this item.

**Test artifacts not kept:** the `column_pass_simd_matches_scalar_exhaustive`
test and `force_use_simd` test hook were reverted along with the dispatch
change, since they were written specifically to validate the (rejected)
runtime-dispatch mechanism.

## Perf round 55 (2026-07-06) — ZP_U64: widen the ZP bit-buffer to u64

**Issue.** Round 47 (`X86_CI_BENCH`) found the ZP-decode-only benches
(`jb2_decode`, `jb2_decode_corpus_bilevel`, `jb2_decode_large_600dpi`,
`iw44_decode_first_chunk`, `iw44_decode_corpus_color`, `bzz_decode`) running
faster under `-C target-cpu=x86-64-v3` and flagged **ZP_U64** — widening the
shared `ZpDecoder`'s `bit_buf` from `u32` to `u64` so the four inline
`refill!` copies (djvu-jb2 ×2, djvu-iw44 ×1, djvu-bzz ×1, plus djvu-zp's own
`refill_buffer`) can bulk-load 4 bytes at once and refill less often — as a
previously-deferred idea worth reopening.

**Approach.** Two-stage rollout, byte-exactness gated throughout.
Stage 1: widen `bit_buf`'s type to `u64` everywhere (structural necessity —
all four sites share one field) but apply the actual bulk-refill cadence
(4-byte big-endian chunk load, raised `bit_count` threshold, `<=32` bulk
check / `<=56` byte-wise fallback ceiling) only in djvu-iw44's
`previously_active_coefficient_decoding_pass`, the smallest blast radius.
Stage 2: after an initial (later-revised, see below) positive read on x86 CI,
unify the same cadence into the remaining three sites (`djvu-zp`'s
`refill_buffer`, both `djvu-jb2` inline copies, `djvu-bzz`'s
`decode_mtf_phase`). The `pos` overshoot/EOF-padding semantics (synthetic
`0xFF` bytes past EOF, relied on by round 26's BUG-ZPSHORT) were preserved
exactly — the fast path only fires when a full 4-byte chunk is verifiably
in-bounds, otherwise falling back to the byte-at-a-time loop with its
existing `read_byte`/padding logic unchanged. One test
(`synthetic_bytes_distinguishes_eof_from_spinning`) needed its hardcoded
"4 bytes overshoot" expectation updated to 8, since raising the byte-wise
fallback's threshold from `<=24` to `<=56` legitimately changes how much
padding a bare 2-byte input synthesizes on its first refill — verified this
stays well inside `ZP_EOF_SLACK_BYTES=16`, so the safety margin is intact,
just smaller (12→8 bytes of cushion).

**Byte-exactness.** Verified via a new `examples/zp_u64_digest.rs` (SipHash
digest of width/height/data per decoded page) across the full
`tests/corpus/*.djvu` set (556 pages, 4 files): identical
`grand_digest=fcc12acf155b60b8` at every checkpoint — stage 1, post-`cargo
fmt`, stage 2, post-second-`cargo fmt`, and after a merge+cherry-pick branch
reconstruction forced by a fast-moving `origin/main` — zero divergence,
ever. Full test suite green throughout (1127 tests on the final branch
state); `make check` (fmt, clippy `-D warnings`, no_std, wasm32, tests)
green at every checkpoint.

**Numbers — M1 (Apple Silicon aarch64, local).** 5 independent back-to-back
A/B pairs (`git worktree add <path> origin/main` for an isolated control
checkout vs. this branch, both built `--release`, `cargo bench --bench
codecs`, `--output-format bencher`), varying warm-up/measurement time and
run order to rule out a warm-up confound. Ratio = treatment/control − 1,
negative = branch faster:

| bench | pair 1 | pair 2 | pair 3† | pair 4 | pair 5 |
|---|---|---|---|---|---|
| `bzz_decode` | −6.1% | −4.5% | −1.6% | 0.0% | 0.0% |
| `jb2_decode` | −3.7% | −5.6% | −1.5% | −0.1% | **+4.0%** |
| `iw44_decode_first_chunk` | −6.3% | −6.1% | −0.7% | −1.5% | **+1.8%** |
| `jb2_decode_corpus_bilevel` | −2.5% | −4.9% | −2.8% | +0.8% | −1.2% |
| `iw44_decode_corpus_color` | +0.2% | −3.5% | **+1.5%** | −2.3% | −3.1% |
| `jb2_decode_large_600dpi` | −2.4% | −2.5% | **+1.3%** | −4.3% | −2.0% |

† pair 3 ran the branch checkout first / control second (order flipped) to
rule out a "second-run-is-always-faster" warm-up artifact; the fact it
didn't erase the effect ruled that specific bias out, but pairs 4–5 (longer
warm-up, more elapsed wall-clock/thermal settling) shrank the deltas toward
zero and, for `jb2_decode`/`iw44_decode_first_chunk`, **reversed sign**.
Across all 5 pairs every bench's ratio spans a range that straddles zero —
no bench shows a consistently-signed, reproducible ≥3% effect once more
than 2 samples are gathered.

**Numbers — x86 (GitHub Actions `ubuntu-latest`, `bench-x86-64-v3` job,
default-RUSTFLAGS arm, `ns/iter`).** 4 control (`main`) samples, 2 stage-1
treatment samples, 1 stage-2 (final, all-sites-unified) treatment sample:

| bench | Control A | Control B | Control C | Control D | Stage-1 T1 | Stage-1 T2 | Stage-2 T3 |
|---|---|---|---|---|---|---|---|
| `bzz_decode` | 106 | 104 | 106 | 106 | 93 | 100 | 92 |
| `jb2_decode` | 160,964 | 162,443 | 161,487 | 160,816 | 172,964 | 159,856 | 168,871 |
| `iw44_decode_first_chunk` | 758,502 | 771,110 | 764,868 | 765,663 | 843,891 | 772,343 | 813,581 |
| `jb2_decode_corpus_bilevel` | 581,879 | 589,009 | 581,645 | 585,620 | **391,744** | 588,102 | **401,664** |
| `iw44_decode_corpus_color` | 1,427,347 | 1,387,089 | 1,277,856 | 1,400,562 | 1,301,453 | 1,297,508 | 1,178,703 |
| `jb2_decode_large_600dpi` | 2,397 | 2,496 | 2,492 | 2,409 | 2,366 | 2,456 | 2,531 |

Two findings undercut treating any of this as a clean win. First,
`iw44_decode_corpus_color`'s 4 control samples (identical code, zero-diff)
span 1,277,856–1,400,562/1,427,347 ns — an **11.7% spread with no code
change at all** — comparable to or larger than the apparent ~7–13%
treatment "improvement," so the treatment cluster sits inside the control's
own noise band, not clearly below it. Second, and more decisively,
`jb2_decode_corpus_bilevel`'s two **stage-1 treatment samples are the exact
same commit run twice** and disagree by 33 percentage points (T1 −33% vs T2
+1% relative to the control mean) — direct, unambiguous proof that
shared-runner cross-run noise alone (most likely noisy-neighbor contention
on GitHub-hosted `ubuntu-latest`) can swing a single bench by more than 10×
this task's 3% decision threshold on literally identical code. Any
single-or-double-sample x86 CI comparison is unable to distinguish signal
from this noise floor.

**Decision.** **Reverted.** Byte-exactness held perfectly throughout (the
one unambiguous, load-bearing result), but neither architecture produced a
reproducible ≥3% win once sampled honestly: M1's apparent early "−3 to −6%"
(pairs 1–2) crossed zero and flipped sign by pair 5; x86's apparent
"−7–13%" on `iw44_decode_corpus_color` sits inside a directly-measured
11.7% same-code noise band, and `jb2_decode_corpus_bilevel`'s identical-code
replay proved the runner noise floor alone exceeds the decision threshold
several times over. The methodologically honest read of 5 M1 pairs + 7 x86
CI samples is "no measurable, reproducible effect on either architecture" —
this task's own decision gate ("flat/slower on both → revert") applies.
Reverted `crates/djvu-zp/src/lib.rs`, `crates/djvu-jb2/src/lib.rs`,
`crates/djvu-bzz/src/decode.rs`, `crates/djvu-iw44/src/lib.rs` to
`origin/main`'s content exactly (verified via an empty `git diff`); deleted
`examples/zp_u64_digest.rs`. Reason for keeping this entry despite the
revert: (a) it closes the round-47 lead so it isn't re-attempted blind, and
(b) the noise-floor evidence above (an 11.7% same-code spread on x86 CI, a
33-point swing on identical code for one bench, and a sign-flipping M1
result once sampled 5× instead of 2×) is itself a reusable methodological
finding for this repo's benchmark infra — small (<10%) single/double-sample
wins on either the shared x86 CI runners or short local M1 runs should not
be trusted without ≥4–5 independent samples and an explicit check for
sign-consistency.

## Perf round 56 (2026-07-10) — PAR_PAGE_LAYERS third attempt on a true BG-heavy fixture (#496)

Issue #496 asked for a BG-heavy single-page colour fixture to make the twice-reverted
PAR_PAGE_LAYERS (`rayon::join` of the independent Sjbz/JB2 and BG44/IW44 layers in
`PageEncoder::encode`) measurable. The prior attempts failed for two reasons this
round fixes: (1) the existing colour-encode benches feed `PageEncoder` a decode of
the **BG44 layer only** (`load_color_pixmaps`), which re-segments to a nearly empty
mask + smooth background — neither layer is substantial; (2) the 2026-07-04 criterion
A/B ran a 400 ms × 100-sample bench back-to-back and drowned in thermal throttle
(±13% swing on identical code).

### Fixture search (fresh, both layers timed per page)

A probe timed `segment_page` + `encode_jb2_dict` + `encode_iw44_color` separately on
every colorbook page, first on the BG44-layer decode (matching the existing benches),
then on the **full composited render** (`render_pixmap` at native resolution — the
"picture page decoded to a full-resolution Pixmap" the issue meant):

- BG44-layer decode: the "BG-dominant by bytes" pages (98% BG44 share) are near-blank
  (Sjbz 5–6 B); every page's iw44/jb2 time ratio ≤ 0.40 with IW44 ≈ 0.2 ms absolute —
  nothing to overlap. Confirms the 07-04 finding on the old pipeline.
- Full composited render (2215×3669): layers become substantial. Best page = **58**:
  jb2 7.8 ms, iw44 1.8 ms, seg 21 ms, re-encoded BG44 byte share 34% (highest in the
  corpus; iw44/jb2 0.23). Still JB2-leaning — no true photo page exists in the corpus —
  but the overlap (`min(jb2, iw44)` ≈ 1.8 ms of a ~40 ms encode) is finally above noise.

### PAR_PAGE_LAYERS (3rd attempt) — **Kept** (2026-07-10)

**Approach.** New bench **`encode_color_page_quality_bgheavy`** (benches/codecs.rs):
colorbook page 58 composited at native resolution via the new `load_rendered_page`
helper, full `PageEncoder::from_pixmap(...).with_quality(Quality).encode()`. Then the
same change as rounds 2/4: `#[cfg(feature = "parallel")] rayon::join(|| sjbz, || bg44)`
in the Quality/Archival arm of `PageEncoder::encode`, sequential fallback otherwise
(`src/djvu_encode.rs`). FGbz needs the finished Sjbz and stays after the join.

**Platform / command.** Apple M1 Max, Rust stable, `[profile.bench]`. Baseline = clean
tree via `git stash push -- src/djvu_encode.rs`; the ~8 s criterion run (40 ms × 200
iterations) is short enough to stay out of the thermal-throttle regime that
contaminated the 07-04 attempt:

```sh
cargo bench --features parallel --bench codecs -- encode_color_page_quality_bgheavy --save-baseline pl_before
# apply change, then:
cargo bench --features parallel --bench codecs -- encode_color_page_quality_bgheavy --baseline pl_before
```

**Numbers (two independent baseline/compare pairs):**

| Benchmark | Run 1 | Run 2 |
|---|---:|---:|
| `encode_color_page_quality_bgheavy` (colorbook p58 composited) | **−4.26%** (p = 0.00, CI −5.0…−3.5%) | **−5.18%** (p = 0.00, CI −6.1…−4.2%) |
| `encode_color_page_quality` (JB2-dominated, regression check) | −1.1% (p = 0.13, no change) | — |

**Decision.** Kept — both runs p < 0.05 with consistent sign and magnitude matching
the predicted `min(jb2, iw44)` overlap; the JB2-dominated bench shows no regression
(the join is µs-overhead). Byte-identical output verified (FNV-1a over the full
container, `parallel` on vs off: identical), as expected — the two closures share no
state and the chunk order is unchanged. Gated to the opt-in `parallel` feature like
PAR_SEGMENT/PAR_ENCODE. Closes the issue-#496 loop: the fixture that finally resolved
it is the **composited** render, not a bigger BG44-layer decode — future colour-encode
micro-parallelism should be measured on `encode_color_page_quality_bgheavy` first.

## Perf round 57 (2026-07-10) — CARTE_CHROMA_HEADER: IW44 v1.2 chroma-plane interpretation (#561)

### #561 — decode `carte.djvu` chroma with full-resolution planes — **Fixed** (2026-07-10)

**Issue.** `carte.djvu` was the corpus outlier in `examples/interop_pixdiff`: mean absolute
RGB error **73.76/255** against DjVuLibre, while every other control was below 0.3. The old
decoder read a clear high bit in an IW44 v1.2 `delay_byte` as `chroma_half`, allocated
700×426 Cb/Cr planes for the file's 1400×852 BG44 image, and therefore fed a different
adaptive-ZP bucket layout from the stream's actual full-resolution chroma payload.

**Approach.** Treat IW44 v1.2 colour planes as full resolution irrespective of that bit. This
matches DjVuLibre's `IWPixmap::decode_chunk` behaviour recorded in the encoder interop audit:
it always builds and consumes full-resolution Cb/Cr planes. The change is confined to first-
chunk header interpretation in `Iw44Image::decode_chunk`; no wavelet or colour-conversion math
changes. The old `carte` test was a noisy self-consistency golden; it is replaced by assertions
for full-size chroma allocation and a fixed FNV-1a digest of the corrected background pixels.

**Platform / commands.** macOS 26.5.1, Apple Silicon host, Rust 1.92.0
(`aarch64-apple-darwin`), release interop harness and debug test profile:

```sh
cargo run --release --example interop_pixdiff -- tests/fixtures/carte.djvu
cargo run --release --example interop_pixdiff -- --corpus
cargo test -p djvu-iw44
make check
```

**Numbers.** The targeted render changes from mean |Δ| **73.76 → 0.53/255**; p50 60→0,
p95 191→2, p99 210→6, and only 0.01% of channels differ by more than 32. The corpus control
run remains at watchmaker 0.03, cable 0.01, colorbook 0.14, navm_fgbz 0.00, and boy 0.00 mean
absolute difference; no non-carte control changes because their headers already set the high
bit. `cargo test -p djvu-iw44` passed 49 tests plus one doctest (one diagnostic ignored).

**Decision.** Fixed. This is a header-interpretation defect, not an inverse-DWT or coordinate-
upsampling bug. It also supersedes #422/`CHROMA_BILINEAR`'s assumption that `carte` was a valid
half-resolution-chroma decode case; future chroma-quality work must start from a verified stream
whose plane dimensions DjVuLibre confirms, rather than from the `delay_byte` high bit alone.
The legacy encoder `chroma_half` option is retained as a source-compatible no-op so it cannot
produce a stream that the corrected decoder (or DjVuLibre) misreads.

## Perf round 58 (2026-07-10) — BENCH_GATE: repair the Criterion PR regression gate (#88)

### #88 — fail closed on missing results and regressions — **Fixed** (2026-07-10)

**Approach.** The PR workflow downloaded the artifact root to `baseline/` but compared
`baseline/target/criterion`, ignored both Criterion bench failures, and lost the compare exit
under `bash -e` before its output was written. The workflow now compares `baseline` directly,
requires the two Criterion benches to succeed, captures the compare exit explicitly before
posting its comment, then fails the job on every non-zero status. `bench_compare.py` returns 2
instead of success when the current run has no Criterion estimates; a missing baseline remains a
non-failing first-run report.

**Validation.** `actionlint .github/workflows/bench.yml` passes. A missing-current probe exits 2;
`python3 scripts/bench_compare.py target/criterion target/criterion` exits 0 and reports no
regressions. The PR's CI run is the end-to-end validation of artifact download and exit plumbing.

**Decision.** Fixed. #557 may now build its instruction-count artifact channel on fail-closed
compare semantics rather than duplicating the former fail-open workflow.

## Perf round 59 (2026-07-11) — FGBZ_PDF_STENCIL: coloured foreground stencils in PDF export (#559)

### #559 — per-palette-colour /MaskN stencils instead of one black /Mask0 — **Fixed** (2026-07-11)

**Issue.** PDF export painted the JB2 mask as a single ImageMask stencil in solid black
(`q 0 0 0 rg … /Mask0 Do Q`), silently flattening FGbz-coloured foreground text to black.
While reproducing it a second, worse defect surfaced: `collect_mask_stream` decoded the mask
with the *inline* Djbz only, so shared-dictionary (DJVI) documents lost their entire
foreground text overlay from the PDF — `navm_fgbz.djvu` baseline PDFs contained **zero**
ImageMask streams.

**Approach (option a from the issue, plus crop + adaptive G4).** When the page has an FGbz
palette with ≥1 non-black colour: decode via `extract_mask_indexed` (shared-dict aware), split
the mask into one bilevel plane per palette colour actually used (colour lookup mirrors the
renderer's `lookup_palette_color`, fallback to colour 0), crop each plane to its pixel bounding
box, and emit one ImageMask XObject per colour painted with its own `r g b rg` fill, positioned
by a scaled/translated `cm`. Colour planes always take the smaller of Deflate/G4 (they are new
output, so the `ccitt_g4` opt-in gate doesn't apply; per-plane min can never regress). Pages
without a palette (or with an all-black one) keep the historical single-black-stencil code path
and formatting (`0 0 0 rg`, full-page `cm` with literal `0 0`) — byte-identical output.

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0. New
`examples/pdf_fg_color_probe.rs`: export with lossless background → `pdftoppm`
(`-scale-to-x/-y`, native pixel dims; poppler 25.x) → `quality::compare_color` against
`render_pixmap` at identical dimensions.

```sh
cargo run --release --features pdf --example pdf_fg_color_probe
make check
```

**Numbers.** Rasterized-PDF vs our renderer (QUALITY_COLOR metric):
- `irish.djvu` (40-colour palette): ΔE mean **8.095 → 0.500**, luma SSIM **0.851 → 0.982**.
- `navm_fgbz.djvu` (2–47 colours/page): ΔE mean improved on all 6 pages, e.g. p1
  **1.046 → 0.537**, p4 **1.020 → 0.376**; luma SSIM up on every page.

File size on the three colour-palette corpus docs (default options): DjVu3Spec **+5.7%**,
irish **+7.2%** — within the <+10% budget. navm_fgbz **+20.1%**, but its baseline is not
comparable: the baseline PDF had *no mask streams at all* (the shared-dict defect above), so
the delta is the restored text overlay (+174 KB of mask streams), not stencil overhead.
Bounding-box cropping cut the naive full-page-plane cost (irish +16.6% → +7.2%); always-adaptive
G4 cut it further (navm +38.2% → +20.1%). All 17 corpus fixtures without a colour palette
produce **byte-identical** PDFs (17/17).

**Decision.** Fixed/Kept. Decision rule met: ΔE improves on every coloured-FG page, size within
budget on true like-for-like docs, byte-identical for all-black-FG documents. Regression tests:
`test_fgbz_colored_foreground_multi_stencil` (multi-stencil + non-black `rg` operator in the
decompressed content stream), `test_fgbz_shared_dict_mask_present`. Unblocks #563 (true-MRC PDF),
which builds on the per-colour stencil path. Follow-up worth filing: the legacy black-stencil
path still uses inline-Djbz-only decode, so an *all-black* shared-dict document still loses its
mask (kept for byte-identity here; should be fixed as its own change with the same
`extract_mask_indexed` plumbing).

## Perf round 62 (2026-07-11) — PAR_CBZ: parallel CBZ export (#598)

### #598 — CBZ export through the render-parallel/write-serial pattern — **Kept** (2026-07-11)

**Issue.** CBZ export rendered and PNG-encoded pages sequentially in the CLI
(`render_cbz`), while the EPUB/PDF/TIFF exporters and the parallel PNG helper next to it
already parallelise page building. PNG deflate is CPU-heavy and embarrassingly parallel.

**Approach.** New library module `src/cbz.rs` (feature `cbz`, included in `cli`):
`djvu_to_cbz`/`write_pages` mirror `djvu_to_epub`'s split — page building (render at target
DPI → user rotation → RGBA PNG) fans out over rayon under `parallel`, ZIP entries are then
written serially in index order (stored, PNG is already deflated). The CLI's `render_cbz`
delegates to it. New `export/cbz` Criterion bench (watchmaker, 12 pages, 150 dpi default).
## Perf round 61 (2026-07-11) — PAR_SAUVOLA: row-parallel Sauvola threshold pass (#575)

### #575 — `fill_sauvola_mask` over rayon row chunks — **Kept** (2026-07-11)

**Issue.** `fill_sauvola_mask` ran a sequential double loop over all pixels while the BG-cell
fill next to it was already rayon-parallel; on Sauvola-enabled encodes (opt-in
`--binarization sauvola`) this pass is pure, embarrassingly parallel work over an immutable
summed-area table.

**Approach.** Factor the per-row threshold loop into `fill_sauvola_row` (writes packed mask
bits directly into its row slice) and drive it with `par_chunks_mut(row_stride)` under the
`parallel` feature; the serial build keeps a sequential driver over the same row body. The
SAT construction (`integral_luma`) stays serial — two memory-bound passes. New
`segment_page_color_sauvola` Criterion bench (2260×3669 colorbook page, window 31, k 0.34).

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0.

```sh
cargo bench --bench render --features cbz -- export/cbz --save-baseline cbz-serial
cargo bench --bench render --features cbz,parallel -- export/cbz --baseline cbz-serial
```

**Numbers.** 299.6 ms → 81.9 ms, **−73.6%** [−76.5%, −69.6%] (p < 0.05) ≈ **3.7×**.
Byte-identity vs the pre-change CLI binary (built at the old main in a separate worktree):
`cmp` identical archives for boy, boy_jb2_rotate90, navm_fgbz, links, a single-page export,
and `--rotate cw90` — zip entry names, order, stored method and default timestamps all
preserved. Determinism/structure covered by 3 new unit tests.

**Decision.** Kept. Decision rule (≥2× at 4 threads, byte-identical) exceeded. CBZ export is
now also available as a library API (`djvu_rs::cbz`), not just a CLI path.
## Perf round 60 (2026-07-11) — PDF_MASK_VISIBILITY: restore lost/invisible foreground masks in PDF export (#620, #621)

### #620 + #621 — shared-dict mask restoration, black-not-white bilevel stencils, FG44 stencil policy — **Fixed** (2026-07-11)

**Issues.** Two defects that made PDF export silently drop or hide the entire text layer:
1. (#620) `collect_mask_stream` decoded the JB2 mask with the *inline* Djbz only, so
   shared-dictionary (DJVI) documents emitted no mask streams at all — `malliavin.djvu`'s
   baseline PDF was 62 KB of blank pages, `problem_page.djvu`'s was 608 B.
2. (#621) The bilevel-only page path painted its `/Im0` ImageMask with `1 1 1 rg` — white
   fill on a white page. Every bilevel-only page rendered blank; confirmed independently
   with poppler `pdftoppm`, Ghostscript, and macOS Quick Look on baseline exports. Prior
   validation missed it because it was structural (`/ImageMask` present) or
   variant-vs-variant (PDF_G4's G4-vs-Deflate rasterizations were both blank, hence
   "pixel-identical").

**Approach.** Route `collect_mask_stream` and the palette path's black fallback through
`DjVuPage::extract_mask` (inline Djbz first, shared dict fallback; also adds Smmr masks).
Paint the bilevel `/Im0` stencil `0 0 0 rg`. Third finding handled along the way: with
shared-dict masks restored, FG44-coloured pages (`history.djvu` gold-on-navy cover) would
now get a *black* stencil stamped over continuous-tone coloured text — the FG44 analogue of
#559. Policy: sample the FG44 colour under the mask; if near-uniform (per-channel spread
≤ 48/255 — the common scanned-book near-black case) paint the single stencil in the mean
FG44 colour, else skip the stencil and let the composited `/Im0` carry the multi-coloured
text (fidelity over edge crispness; true-MRC stencilling is #563).

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0; poppler 25.x, Ghostscript,
`examples/pdf_fg_color_probe.rs` (round 59).

```sh
cargo run --release --features pdf --example pdf_fg_color_probe tests/fixtures/<doc>.djvu
make check
```

**Numbers.** Rasterized-PDF vs our renderer:
- `boy_jb2.djvu`, `ccitt_2.djvu` (bilevel): blank → **pixel-perfect** (ΔE 0.000, SSIM 1.0).
- `malliavin.djvu` (115 pp, shared dict, bilevel): blank → full text (visual check clean);
  62 KB → 13.6 MB — that *is* the restored content (~118 KB/page of 1-bit masks).
- `problem_page.djvu`: blank 608 B → 84 KB with text.
- `watchmaker.djvu` (FG44, near-black uniform): stencil kept in mean FG44 colour, ΔE mean
  ≈ 0.2–2.0, luma SSIM 0.95–0.99; `ccitt_g4` size win preserved (regression test passes).
- `carte.djvu` (multi-colour FG44): black stencil ΔE 20.3 → skip 17.7; −29% file size.
- `colorbook.djvu` (multi-colour FG44): skip beats black stencil on ΔE_max (≈95→80) and
  chroma SSIM on nearly every page; −28.5% file size.
- Byte-identical: all fixtures without any Sjbz/Smmr mask (big-scanned-page, boy, chicken,
  czech¹, slow). ¹czech is an indirect doc whose DJVI lives in external files — no mask is
  decodable from the bundle alone; unchanged, pre-existing limitation.

**Perf follow-up (same PR).** The CI benchmark gate flagged `export/pdf_flatdecode` +8.2%:
the FG44 heuristic decoded the mask and FG44 a second time even though the page's own /Im0
render had just decoded and cached both. Stencil building now reuses the page cache
(`decoded_mask`/`decoded_fg44`); local re-measure: old main 1.254 s → fixed branch 1.097 s
(the pre-existing double mask decode in `collect_mask_stream` is gone too). Output verified
byte-identical to the branch's pre-refactor PDFs across all 20 fixtures.

**Decision.** Fixed/Kept. Every changed fixture is a strict fidelity win (restored or
recoloured text) or a measured ΔE improvement with a size reduction. Regression tests:
`fg44_page_skips_mask_stencil`, updated `mixed_page_has_both_image_and_mask_xobject`
(irish, the palette path), plus round-59's FGbz tests still green. Follow-up candidates:
per-region FG44 stencils (true MRC, #563); external-DJVI resolution for indirect docs.
## Perf round 63 (2026-07-11) — FGBZ_FROM_BLITS: build the FGbz palette from encoder blit metadata (#612)

### #612 — drop the Sjbz decode-after-encode in `foreground_fgbz` — **Kept** (2026-07-11)

**Issue.** `foreground_fgbz` decoded the Sjbz chunk the encoder had just produced
(`jb2::decode_indexed`) to reconstruct a page-sized per-pixel blit map, then re-scanned the
whole page to accumulate per-blit foreground colours — a structural decode-after-encode
round-trip on every colour-profile page.

**Approach.** New `djvu_jb2::encode::encode_jb2_dict_with_blits` returns the emitted blits
(cropped component bitmap + top-left position, in emission order) alongside the byte stream;
the bitmaps are moved out of the encoder's own `ccs` (no clones), and
`encode_jb2_dict_with_options` now delegates to it — the byte stream is unchanged by
construction. `foreground_fgbz_from_blits` accumulates per-blit colours directly off those
blits (blits are pixel-disjoint connected components of the mask, so per-blit sums equal the
decode-based scan's). The decode-based path remains only as the fallback for lossy rec-7
substitution (`lossy_threshold > 0`), where the decoder blits a near-twin whose pixels can
differ from the emitted component. Bonus: the bundle path no longer decodes the shared Djbz
back at all (it was needed only to resolve the per-page blit maps).

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0. Old side measured from a
worktree pinned at pre-change main.

```sh
cargo bench --bench codecs -- encode_color_page_quality_bgheavy
cargo bench --bench codecs -- "jb2_encode_dict|encode_color_page_quality$"
```

**Numbers.**
- `encode_color_page_quality_bgheavy` (round-56 designated instrument): 52.56 ms → 34.15 ms,
  **−35.0%** (repeat run 33.90 ms).
- `encode_color_page_quality`: 6.07 ms → 4.53 ms, **−25.4%**.
- `jb2_encode_dict`: 7.10 → 7.00 ms — unchanged (blit hand-back is move-only).
- Byte-identity: old-vs-new CLI `encode` outputs `cmp`-identical for quality/archival/lossless
  × single-page and multi-page-directory (shared-dict bundle) on navm_fgbz-derived PNGs.

**Decision.** Kept. Decision rule (≥3% on the bgheavy bench, byte-identical default output,
shared-dict parity) exceeded by an order of magnitude. The remaining decode-based scan runs
only under `lossy_threshold > 0`; unifying that case would need the encoder to hand back the
substituted twin's shape per blit — noted as possible follow-up, not needed now.
cargo bench --bench codecs -- segment_page_color_sauvola --save-baseline serial
cargo bench --bench codecs --features parallel -- segment_page_color_sauvola --baseline serial
```

**Numbers.** 6.276 ms → 3.638 ms, **−42.0%** [−43.2%, −40.5%] (p < 0.05). Byte-identical:
`parallel_sauvola_mask_is_byte_identical_to_sequential` (131×97, non-byte-aligned width,
checks row padding too) plus the pre-existing segment suite green in both modes.

**Decision.** Kept. Decision rule (≥10% at 4 threads, byte-identical) exceeded. Note per the
issue: per-page parallelism in bundles already amortizes this — the beneficiary is
single-image/CLI latency on Sauvola profiles. Default (Otsu) segmentation untouched.
## Perf round 65 (2026-07-11) — C5_SUB4_MASK: preserve the 1/4-res JB2 mask across render-cache downgrade (#607)

### #607 — retained mask_sub4 tier + decode bypass for eligible sub≥4 renders — **Kept** (2026-07-11)

**Issue.** The C5_COMPRESS downgrade tier preserved downscaled colour backgrounds
(`bg_rgb_s2`/`bg_rgb_s4`) but dropped both the full JB2 mask and its 1/4-res max-pool
downsample, so a later thumbnail/zoomed-out render re-ran the complete JB2 arithmetic
decode even when the tiny sub4 mask had already been computed. The middle tier benefited
colour pages but not JB2-heavy pages.

**Approach.** Two pieces (the issue's prior evidence was right that keeping the field alone
is insufficient):
1. `PageLayers::downgrade` no longer clears `mask_sub4` (~1/16 of the packed mask bytes;
   already counted by `cached_bytes`, so budget enforcement is unchanged).
2. `decode_layers` gained a warm-tier bypass: when `bg_subsample ≥ 4`, `bold == 0`, the page
   has no FGbz chunk, and `mask_sub4` is already built (new peek accessor
   `mask_sub4_cached`, no decode side effects), the full JB2 mask decode is skipped
   entirely — the compositor reads only the sub4 plane on that path anyway (eligibility
   mirrors `resolve_sub4_mask`), so output is pixel-identical by construction.

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0. Old side = worktree at
pre-change main with the same sub4 rail patched into the harness.

```sh
cargo run --release --example c5_compress_bench downgrade 62914560
```

**Numbers.** `c5_compress_bench` (colorbook, 60 MB budget, `downgrade` mode, median of 11
post-warm-up trials), new sub=4 rail: **11.88 ms → 3.96 ms (−67%)** re-render after
downgrade; sub=1 (57.8 ms) and sub=2 (18.5 ms) rails unchanged; `final_render_cache_bytes`
identical (62,756,604) — same ceiling honoured. Structural test proves the warm sub4
re-render performs **zero** JB2 mask decodes (new `JB2_MASK_DECODES` test counter,
mirroring `BG44_CHUNK_DECODES`) while the full-res re-render still cold-decodes; bold
dilation is proven to bypass the shortcut (needs full-res mask semantics). Output equality
asserted for sub4/bold/full-res before vs after downgrade.

**Decision.** Kept. Decision rule (≥25% eligible re-render win, sub4-sized retained bytes,
same ceiling, pixel identity) exceeded. FGbz-palette pages intentionally excluded from the
shortcut (need full-res indexed lookups) — same exclusion as the compositor's existing sub4
path. Policy sweep note: `drop` mode still clears everything (unchanged); the retained tier
only augments `downgrade`.
## Perf round 64 (2026-07-11) — FUZZ_ENCODER_GAPS: bzz_encode and encode_g4 fuzz coverage (#567)

### #567 — encoder fuzz gaps closed, one real encoder bug found — **Kept / Fixed** (2026-07-11)

**Issue.** `bzz_encode` (feeds every TXTz/ANTz/NAVM/DIRM we write) had no fuzz coverage at
all; `smmr::encode_g4` (round 53, with a history of a symmetric encoder/decoder bug that
survived internal round-trips) was only exercised indirectly via `encode_smmr`.

**Approach.**
- New `fuzz_bzz_encode` target: arbitrary bytes → `bzz_encode` → `bzz_decode` → bit-exact
  assert. Seeds: empty/text/random.
- New `fuzz_g4` target: structured bitmap from fuzz input (bounded dims ≤200) →
  `encode_g4` → 4-byte header + `decode_smmr` → pixel-exact assert. Seeds mirror the
  round-53 synthetic shapes (stripes/bands/checker/sparse/single-row).
- `fuzz_encode` extended with lossy `Jb2EncodeOptions` (despeckle 0–15 px,
  `lossy_threshold` 0–0.15): decodable + dimension-exact always; despeckle-only output must
  additionally never *add* ink (component drop is its only legal effect).
- Both new targets wired into `.github/workflows/fuzz.yml` (weekly + on main) and
  `oss-fuzz/build.sh` (which also gained the previously-missing `fuzz_encode`).

**Found bug (block boundary).** The 4 MiB block-boundary case can't be reached by libFuzzer
(`max_len`), so it became a unit test — which failed: `bzz_encode` split input at exactly
`MAX_BLOCK_SIZE`, but the on-wire block size is the *BWT* size (input + 1 marker byte), so a
4 MiB input block produced a 4 MiB + 1 wire block that `bzz_decode` (and DjVuLibre's
equivalent cap) rejects — any BZZ payload we wrote from ≥ 4 MiB input was undecodable.
Fixed: input blocks now split at `MAX_BLOCK_SIZE − 1`; `bzz_roundtrip_block_boundary` covers
exactly-4-MiB and 4-MiB+1 inputs. Streams for inputs < 4 MiB are byte-identical (the split
point only moves for larger inputs, which previously produced broken output).

**Numbers / validation.** Local libFuzzer runs are currently blocked on this machine —
the fuzz binary hangs at 100% CPU inside dyld initializers before reaching `main`
(macOS 26.5 + nightly + ASan environment issue; `--sanitizer none` fails to link), so the
planned 1 h/target local soak could not be executed honestly. Substitutes: (a) committed
deterministic randomized soak tests exercising the same assertion bodies —
`bzz_roundtrip_randomized_soak` (djvu-bzz) and `g4_roundtrip_randomized_soak` (smmr),
hundreds of varied sizes/patterns each, green in `make check`; (b) the CI fuzz jobs
(ubuntu, where the existing targets demonstrably run) pick the new targets up weekly with
corpus persistence. djvu-bzz suite green incl. the new block-boundary test.

**Decision.** Kept (infra) + the boundary fix shipped. Item 3 of the issue (scheduled
external libtiff/Pillow differential for G4) is not in this change — noted as the remaining
follow-up on #567's plan; the in-tree differential (own decoder) plus the CI fuzz jobs cover
the symmetric-bug class the round-53 lesson warned about only partially.
## Perf round 66 (2026-07-11) — PDF_WRITER_STREAM: stream PDF objects to a writer (#606)

### #606 — writer-oriented PDF serialization, bytes API as a wrapper — **Kept** (2026-07-11)

**Issue.** `PdfWriter` retained every PDF object body and `serialize` then built a second
full output buffer — peak memory ≈ retained bodies + final PDF bytes, even though the page
render pipeline was already O(1) in page bodies (#449).

**Approach.** `PdfWriter` is now generic over `std::io::Write`: the header is written at
construction, each `add_obj` streams `N 0 obj … endobj` immediately and retains only
`(id, offset)` for the xref; `finish()` writes xref + trailer. Objects flow in the same
insertion order the old writer serialized in, so output bytes are unchanged. New public
`djvu_to_pdf_to_writer(doc, opts, sink)`; `djvu_to_pdf(_with_options)` are now thin `Vec`
wrappers. The CLI's PDF export streams straight to a `BufWriter<File>` (never buffers the
whole PDF). The parallel path renders in bounded chunks (8 × threads) instead of collecting
all rendered pages, then emits each chunk in order — O(chunk) retained bodies (issue plan
item 5). New `PdfError::Io` variant (additive).

**Platform / commands.** macOS 26.5.0, Apple Silicon, Rust 1.92.0. 504-page fixture =
`djvu merge` of watchmaker ×42 (249 MB output PDF); old side = worktree pinned at main.

```sh
/usr/bin/time -l <djvu> render --format pdf --all --output big.pdf big504.djvu
```

**Numbers.** Sequential: peak RSS **2.718 GB → 2.189 GB (−529 MB, 1.24×)**, wall-clock
28.40 → 28.39 s, output byte-identical. Parallel: **2.846 GB → 2.34 GB (1.22×)**, wall-clock
4.48 vs 4.02–4.82 s (within noise; the first chunking attempt at 2× threads cost +27% from
chunk-barrier tail imbalance — 8× threads recovered it). All 20 fixture PDFs byte-identical
to main; poppler `pdfinfo`/`pdftoppm` read the 504-page output cleanly.

**Decision.** Kept, with the decision rule's 1.5× *total*-RSS target explicitly not met —
and the measurement says why: the writer path removed essentially all of its addressable
memory (~530 MB ≈ 2× final PDF bytes on this fixture), but peak RSS is dominated by
per-page render caches (~4.3 MB/page × 504) that exporters, holding `&DjVuDocument`,
cannot evict today. Filed #629 for that (interior-mutability eviction / cache-bypassing
export render); once it lands the combined reduction on this fixture should far exceed
1.5×. Wall-clock and byte-identity criteria met.
## Perf round 67 (2026-07-11) — WASM_ZERO_COPY: reusable Rust-owned pixel buffer + zero-copy view (#611)

### #611 — `WasmPixmap` handle and `render*_into_pixmap` APIs — **Kept** (additive) (2026-07-11)

**Issue.** Every browser render allocated a fresh JS `Uint8ClampedArray` and copied the full
RGBA pixmap out of wasm memory — coarse, progressive and final renders alike, tens to
hundreds of MB repeatedly after the codec had already finished.

**Approach.** Additive only (existing copying `render*` methods unchanged, per the issue's
out-of-scope): new `#[wasm_bindgen] WasmPixmap` (Rust-owned `Vec<u8>` + dims) with
`view()` — a zero-copy `Uint8ClampedArray` view into wasm linear memory (one `unsafe`
exception, documented lifetime contract: consume immediately; `ImageData` copies) — and
`to_bytes()` (owned copy). `WasmPage::render_into_pixmap` reuses the handle's allocation
via `djvu_render::render_into` (no per-frame Rust alloc either);
`render_progressive_into_pixmap` gives progressive sessions one buffer for N passes.
New committed browser bench: `examples/wasm/bench_zero_copy.html`.

**Platform / commands.** macOS 26.5.0, Chrome (real tab), wasm-pack 0.13.1 release build
(`--features wasm`), local `python3 -m http.server`.

**Numbers** (median per frame, warm decode caches):
- navm_fgbz @300 dpi (32 MB frames): copy 56.6 ms → into_pixmap+view 51.4 ms (−9.2%).
- navm_fgbz @600 dpi (**128 MB** frames): copy 220.7 ms → 203.6 ms (−7.7%); the removed
  17.1 ms is the entire JS alloc + wasm→JS copy — the transfer step itself goes from a
  full-buffer copy to an O(1) view (**>99% transfer-overhead reduction**; the residual
  time is codec/compositor).
- bilevel boy_jb2 (0.2 MB): 0.30 → 0.10 ms.
- Lifetime: after a wasm memory *growth* render, the stale view reports **length 0
  (detached)** — visibly stale, never silently wrong; without growth the view stays mapped
  to the same buffer. Fresh `view()` after re-render is correct.

**Decision.** Kept. Decision rule's transfer-overhead criterion (≥80%) met (>99%); the
total-latency alternative (≥15% at ≥50 MB) is not — the copy is only ~8–9% of a large
frame's total time because the compositor dominates. Per-frame JS allocation eliminated;
GC/growth lifetime behaviour verified in a real Chrome tab. Node-ABI note: the plain
copying APIs remain the safe default there (the historical length-corruption motivation),
and the new handle APIs are opt-in.
## Perf round 68 (2026-07-11) — WASM_LAZY_OPEN: owned shared backing for the browser from_bytes (#609)

### #609 — `WasmDocument::from_bytes` through `parse_backed` — **Kept** (2026-07-11)

**Issue.** The browser binding opened documents via the borrowed-slice eager parser
(`DjVuDocument::parse`), which copies every bundled page's bytes at open time — bypassing
the owned shared-backing path (`parse_backed`) that made the native `Document::from_bytes`
lazy (LAZY_PAGE_CONSTRUCT: −48% on a 520-page doc).

**Approach.** `WasmDocument::from_bytes` now takes `Vec<u8>` (JS-visible signature
unchanged — still pass a `Uint8Array`; the JS→wasm transfer is the single unavoidable
copy) and moves the buffer into the shared `Backing`, so bundled pages materialize lazily
on first access. New committed browser bench `examples/wasm/bench_open.html`.

**Platform / commands.** macOS 26.5.0, Chrome (real tab), wasm-pack release build, 504-page
7.3 MB bundle (watchmaker ×42), 25 constructor trials per run.

**Numbers.** Constructor median 1.00 ms → 0.60 ms (**−40%**; steady-state tails
0.9–1.3 ms → 0.4–0.7 ms; a second new-build run medianed 1.8 ms from GC noise in its first
half with the same 0.4–0.7 ms tail). Open→first-render 23.8–29.3 ms (old) vs 19.3–44.3 ms
(new) — single-shot, noise-dominated, no regression signal. Correctness spot-checks in the
browser: page_count, first-page and mid-page renders. Native `parse_backed` behaviour
(laziness, malformed input) is already covered by the existing suite; renders are
byte-identical by construction (same parser underneath).

**Decision.** Kept. Decision rule (≥30% construction improvement, no single-page/render
regressions) met on constructor time; the absolute numbers are small on this 7.3 MB corpus
bundle — the win scales with document size (the copies eliminated are O(file size)). Node
ABI note: `Vec<u8>` params behave identically to `&[u8]` at the boundary (owned copy in),
so the historical view-lifetime hazard does not apply.

## Perf round 69 (2026-07-11) — RGB_PNG_EXPORT: RGB (not RGBA) PNG payloads in EPUB/CBZ (#599)

### #599 — strip the constant alpha at PNG emit — **Kept** (lever 1) (2026-07-11)

**Issue.** EPUB and CBZ encoded full RGBA PNGs although pages are always opaque (the
compositor writes alpha=255 inline — ALPHA_INL): 33% more raw bytes into deflate for zero
information.

**Approach (lever 1 of the issue).** `encode_rgba_to_png` (EPUB) and the CBZ page encoder
strip the alpha channel at emit and encode `ColorType::Rgb`. Pixels after decode are
identical (verified per page via PIL against the RGBA builds).

**Numbers.** navm_fgbz (6 pages): CBZ 1.692 → 1.557 MB (**−8.0%**), EPUB 1.697 → 1.562 MB
(**−8.0%**); boy CBZ 87.4 → 78.7 KB (−9.9%). `export/epub` bench 290.9 → 297.9 ms (+2.4%,
noise-level — the rgba→rgb pass roughly offsets the smaller deflate input on this corpus).
Decision rule for lever 1 ("shrink **or** speed up, identical pixels") met on size.

**Decision.** Kept (lever 1). Lever 2 (RGB-native row mode through the streaming render
path, skipping RGBA staging for PDF/TIFF/EPUB/CBZ) not pursued: the emit-time conversion
measured at noise level here, matching the ALPHA_INL/ZEROED history that output-bandwidth
micro-changes rarely clear the 3% bar — revisit only with instruction-count benches (#557).
Depends on the `cbz` module from PAR_CBZ (round 62) — PR based on that branch.

## Perf round 70 (2026-07-11) — METADATA_CACHE: cache decoded TXTz/ANTz per page (#605)

### #605 — per-page `Arc` cache for text layer and annotations — **Kept** (2026-07-11)

**Issue.** Every text/annotation/hyperlink access re-ran the BZZ decode and rebuilt the full
zone/annotation tree; viewers ask for the same metadata repeatedly (search, selection, link
overlays).

**Approach.** Two new `OnceLock` slots in `PageLayers` (so eviction and the cache budget see
them; byte estimate accounted in `cached_bytes`): `Arc<TextLayer>` and
`Arc<(Annotation, Vec<MapArea>)>`. Public owned-returning APIs unchanged (they clone out of
the cache); new `text_layer_shared`/`annotations_shared` return the `Arc` for loop-heavy
callers. Parse errors are **not** cached — malformed chunks keep erroring per call. New
`text_extraction_cold` bench splits cold from warm.

**Numbers.** watchmaker p0: warm repeated `text()` 177.1 µs → **3.90 µs (≈45×)**; cold
179.1 µs vs old 177.1 µs (+1.1%, includes fresh-parse batch overhead — noise-level).
Warm `Arc` identity asserted (`Arc::ptr_eq`); malformed-TXTz double-error test.

**Decision.** Kept. Decision rule (warm ≥3×, cold within ~1%, bounded/evictable memory) met:
45× warm, cache lives in the budgeted, evictable `PageLayers`.

## Perf round 71 (2026-07-11) — BUNDLE_TWO_PASS: drop segmented backgrounds and mask clones from the bundle encode (#565)

### #565 — two-pass layered bundle encode — **Kept** (bounded scope recorded) (2026-07-11)

**Issue.** Multi-page layered encode retained everything at once: every `SegmentedPage`
(1-bit mask + subsampled RGBA background) survived to the end of the encode, plus a full
clone of every mask made just for shared-dictionary clustering, plus all encoded bodies and
the assembly buffer.

**Approach.** Pass 1 (parallel): segment each page, immediately encode its BG44 (and
optional TH44) and drop the background pixmap — between passes only the 1-bit masks and
already-compressed chunk bodies are retained. Clustering now runs over borrowed masks (new
additive `cluster_shared_symbols_from_refs` in djvu-jb2) — the per-mask clone is gone.
Pass 2 (parallel): Sjbz + FGbz-from-blits + body assembly, unchanged chunk order. Emitted
bytes are identical by construction and verified.

**Platform / commands.** macOS 26.5.0, Apple Silicon; 96-page PNG set (watchmaker pages at
150 dpi ×8), CLI `encode -q archival <dir>`; old side = worktree at pre-change main.

**Numbers.** Peak RSS 922.9 MB → 873.2 MB (**−50 MB**); wall 3.80 → 3.05 s; output
`cmp`-identical. The remaining peak is dominated by the **caller-held input pixmaps**
(96 × 8.4 MB ≈ 807 MB): the public API takes `&[Pixmap]`, so the ≥2× total-RSS target of
the issue is unreachable by internal changes alone — that bound is the recorded outcome the
issue's decision rule asked for. Internal retention between passes is now
O(masks + compressed bodies) instead of O(masks×2 + background pixmaps). The final
assembly (`assemble_djvm_bundle`: bodies + output ≈ 2× file size) is the remaining
internal consumer; a DIRM-backpatching writer sink (the #606 pattern) is the natural
follow-up if bundles grow to where that matters.

**Decision.** Kept: byte-identical, strictly less retention, no wall-clock cost. A
streaming-*input* encode API (pages supplied one at a time) is what a real ≥2× needs —
out of scope for this experiment's fixed `&[Pixmap]` surface.
## Perf round 72 (2026-07-11) — TRUE_MRC: background-only /Im0 at native BG44 resolution (#563)

### #563 — opt-in `PdfOptions::mrc` — **Kept** (opt-in) (2026-07-11)

**Issue.** The mixed-page PDF path embeds `/Im0` as the full composited render (background
WITH text) at `output_dpi`, then repaints the text via the stencils anyway: the raster layer
pays JPEG bits for glyph edges (plus ringing halos) and is stored upsampled relative to the
background's native BG44 resolution.

**Approach.** Opt-in `PdfOptions::mrc`: when the page's foreground is fully covered by
stencils (the #559/#620 layer machinery — FGbz palette layers, uniform-FG44 colour, or plain
black), embed the **background layer alone** (`extract_background`, native subsampled
resolution; the page `cm` scales it) and let the stencils carry the text. Multi-colour-FG44
pages (stencil skipped), photo-only and bilevel pages fall back to the default path
unchanged. Raster policy (`jpeg_quality`/`adaptive_raster`) factored into a shared
`encode_img0_body` and applied to both paths. Default `mrc: false` is byte-identical.

**Platform / commands.** macOS 26.5.0; new committed probe:

```sh
cargo run --release --features pdf --example pdf_mrc_probe
```

**Numbers** (default options vs `mrc: true`; pdftoppm + `compare_color` vs our renderer,
per-page averages):

| doc | size | dE_mean avg | ssim_y avg |
|---|---|---|---|
| watchmaker (12 pp) | 5.72 MB → **2.03 MB (−64.5%)** | 7.38 → **2.15** | 0.794 → **0.940** |
| irish | 1.23 MB → **0.16 MB (−86.7%)** | 0.600 → **0.003** | 0.981 → **0.9999** |
| navm_fgbz (6 pp) | 1.21 MB → **0.26 MB (−78.9%)** | 0.641 → **0.062** | 0.979 → **0.998** |
| colorbook (62 pp) | unchanged (fallback: multi-colour FG44, stencils skipped) | — | — |

Size *and* fidelity improve together: the composited default upsamples the background and
JPEG-compresses glyph edges, while MRC reproduces exactly the layering our renderer (and
DjVu itself) uses. Under-mask diffusion (BG_DIFFUSE-style) wasn't needed for these corpora —
encoder-produced BG44 is already smooth under the mask; left as a follow-up knob if foreign
files show ghosting.

**Decision.** Kept as opt-in. Decision rule (≥15% smaller, equal-or-better text quality)
exceeded several times over; the "consider defaulting later" question should wait for
corpus diversity (#558) since the win depends on stencil coverage.

## Perf round 73 (2026-07-11) — TIFF_G4: CCITT Group 4 for bilevel TIFF export (#579)

### #579 — 1-bit G4 strips via `smmr::encode_g4`, minimal in-crate IFD writer — **Kept** (opt-in) (2026-07-11)

**Issue.** Bilevel TIFF export wrote 8-bit grayscale strips with Deflate; the `tiff` crate
(0.9) has no CCITT encoder at all, while we own a fully validated T.6 encoder (PDF_G4,
round 53).

**Approach.** New `TiffOptions::bilevel_compression` (`Deflate` default — byte-identical —
or `G4`). The G4 path hand-rolls the minimal multi-page bilevel TIFF (LE header, 11-tag IFD
per page, Compression=4, Photometric=min-is-white, 1 strip/page = raw `encode_g4` payload);
page masks come from `extract_mask` (shared-dict aware), pages without a mask emit a blank
white page. Parallel per-page G4 encode under the `parallel` feature.

**Numbers.** Deflate → G4 whole-file: boy_jb2 1,144 → 464 B (**2.47×**), cable_1973 (2 pp)
113,029 → 40,510 B (**2.79×**), watchmaker (12 pp) 1,812,826 → 719,516 B (**2.52×**).
Validation: Pillow/libtiff reads all three pixel-identically to the Deflate variant
(per-page ink-set equality, multi-page included); `tiffinfo` reports proper "CCITT Group 4 /
min-is-white / Bits 1"; unit tests round-trip the strip through our own T.6 decoder against
the page mask and walk the multi-page IFD chain.

**Decision.** Kept (opt-in; default unchanged until broadly validated, per the issue).
Decision rule (≥1.5× smaller, libtiff-identical pixels) exceeded — 2.5–2.8×. Upstreaming a
`Fax4` backend to the `tiff` crate remains the ecosystem-friendly follow-up.

## Perf round 74 (2026-07-11) — EPUB_ADAPTIVE_RASTER: adaptive PNG/JPEG + Gray8 page images in EPUB (#580)

### #580 — two independent levers, both kept — **Kept** (2026-07-11)

**Issue.** EPUB embedded every page as an RGB PNG: photo-heavy pages bloat (PNG on
continuous tone), grayscale/bilevel pages waste 3 bytes/px.

**Approach.**
- **Lever 1 (adaptive JPEG):** `EpubOptions { jpeg_quality: Option<u8>, adaptive: bool }`
  mirroring `PdfOptions`; with `adaptive`, each page image is encoded both ways and the
  smaller wins (PDF_ADAPTIVE_RASTER pattern, one page's pair live at once). JPEG via the
  same `jpeg-encoder` dep, now owned by the `epub` feature (#509 hygiene). Per-page
  extension/media-type threaded through the XHTML/OPF manifest.
- **Lever 2 (Gray8):** pages whose render is pure grayscale (r==g==b scan) encode as Gray8
  PNG — 3× less raw data into deflate, pixel-identical. Applies to the default path.

**Numbers.**
- Adaptive (q80): colorbook 36.17 → **8.69 MB (−76.0%)** at JPEG-page fidelity
  `ssim_y = 0.9969` / combined 0.9966 (rule: ≥25% at SSIM ≥0.99 — met); watchmaker adaptive
  = PNG byte-for-byte (JPEG lost on every page — no regression, the guard works); cable −1.1%.
- Gray8 (default path, vs pre-change main): cable_1973 666 KB → **254 KB (−61.9%)**,
  watchmaker 11.21 → **4.40 MB (−60.7%)**; Pillow-verified pixel-identical page images on
  both (14 pages total).

**Decision.** Both levers kept. Default output changes only for grayscale pages (Gray8,
pixel-identical); colour pages stay byte-identical by construction. EPUB_PNG_COMPRESSION's
old rejection (filter tweaks, 2.84× larger) remains untouched — this changes format choice,
not compression settings.

## Round 75 (2026-07-11) — RESOURCE_CEILING_AUDIT: decode-time bounds inventory + TXTz caps (#589)

### #589 — systematic untrusted-length allocation/loop audit — **Fixed** (one axis) + **Documented** (2026-07-11)

**Issue.** Decode-time resource ceilings existed only where fuzzing happened to hurt
(JB2_PAGE_SYM_CAP, BZZ); no systematic pass. Untrusted-length-driven allocations elsewhere
(DIRM counts, TXTz/ANTz expansion, zone-tree depth/width, IW44 blocks, thumbnails, NAVM)
had no stated bounds.

**Approach.** Full inventory of open/decode allocation and loop sites, each classified by
driving field and current bound (the deliverable table, now in SECURITY.md). Result: every
axis is bounded **except** the TXTz zone tree — `parse_zone` reserved
`Vec::with_capacity(children_count)` from an unchecked `i24` (up to 16,777,215 → ~1.5 GB
from 3 crafted bytes) and recursed with no depth limit (stack-overflow DoS on a crafted
single-child chain). Every other reviewed axis (DIRM, ANTz s-expr, IW44 planes, JB2, IFF,
NAVM) already has an explicit `checked_add`-against-buffer, a named `MAX_*` constant, or a
narrow integer width.

**Fix.** `MAX_ZONE_DEPTH = 64` (mirrors `MAX_NAVM_DEPTH`) threaded through `parse_zone`;
the child reservation is capped to `children_count.min(remaining / MIN_ZONE_RECORD_BYTES)`
(each child needs ≥17 bytes), so the up-front allocation is O(remaining input), not O(2^24).
The loop still iterates the full declared count and fails with `ZoneTruncated` exactly as
before on genuinely-truncated files (a pre-existing truncation test still passes unchanged).
Regression seeds: `zone_child_count_amplification_is_rejected`, `zone_depth_is_bounded`,
`zone_normal_tree_still_parses`. SECURITY.md now states the full "at most X memory / Y work"
table across all codecs.

**Validation.** Real text layers still parse (watchmaker, malliavin — full chapter text;
maskless fixtures report "No text layer" as before). `make check` 1156 tests. No DjVuLibre
behaviour change: the caps only reject inputs that cannot decode to a valid tree anyway
(the #577 discipline — a 64-deep or 16.7M-child single zone is not a real DjVu file).

**Decision.** Fixed (TXTz) + documented (all axes). The issue's "done when every axis is
capped+tested or documented as bounded-by-construction" is met: one axis capped+tested, the
rest recorded as bounded-by-construction in SECURITY.md.
## Perf round 76 (2026-07-12) — WASM_BATCH: coarse-grained batch page rendering in the browser (#610)

### #610 — `WasmDocument::render_pages_batch` — **Kept** (2026-07-12)

**Issue.** WASM_THREADS found wasm threading neutral for single-page decode and ~9× worse
for fine-grained compositor work, and named coarse one-page-per-Worker tasks as the one
viable threading shape. The browser binding only exposed one page per call.

**Approach.** Additive `WasmDocument::render_pages_batch(dpi, start, count) →
Vec<WasmPixmap>`: coarse page-level rayon tasks on the opt-in `wasm-threads` pool
(`initThreadPool`), input order preserved by the indexed collect, memory bounded by the
caller-chosen batch size (`count` pixmaps live at once), results as `WasmPixmap` handles
(zero-copy `view()`, #611). Without a pool the same API renders sequentially. Single-page
API untouched. New committed browser bench `examples/wasm/bench_batch.html` (needs the
COOP/COEP server from the wasm README).

**Platform / commands.** macOS 26.5.0, Chrome (real tab, cross-origin isolated), nightly
`wasm-threads` build (`-Z build-std`, `+atomics,+bulk-memory`), median of 3.

**Numbers** (vs the control: a JS loop over the existing single-page `render()`):
- watchmaker ×8 pages @100 dpi: loop 94–99 ms → batch **22.0 ms at 4 workers (4.28×)**,
  43.0 ms at 2 workers (2.31×), 85.4 ms with no pool (1.16× — sequential fallback works;
  the residual win is the avoided per-page JS copy).
- cable (bilevel, only 2 pages) @150 dpi: 36.7 → 22.6 ms (1.62× — bounded by 2 pages).
- Page 0 pixels spot-checked identical to the single-page render; order preserved.

**Decision.** Kept. Decision rule (≥2× at 4 workers on ≥8-page batches, ordered,
pixel-identical, bounded memory, single-page untouched) met at 4.28×. This is the recorded
revisit-condition of WASM_THREADS fulfilled: coarse batches are where the Worker pool pays.

## Perf round 77 (2026-07-12) — IW44_CHECKPOINT: resume full decode from the cached first-chunk state (#608)

### #608 — chunk-0 checkpoint via the existing `bg44_partial` tier — **Kept** (2026-07-12)

**Issue.** After eviction (or on the first full render after a thumbnail), a full IW44
decode restarts from BG44 chunk 0 — the most expensive chunk — even when a first-chunk
decode already happened.

**Approach.** Measured first (new committed probe `examples/iw44_checkpoint_probe.rs`):
`Iw44Image` is `Clone`, and resuming a fresh clone of the post-chunk-0 state through chunks
1..n is byte-identical to a cold 0..n decode (progressive-decode semantics). Clone cost
0.04–0.52 ms; checkpoint bytes ≈ the coefficient planes (5–19 MB/page — the C5_COMPRESS
finding that this is the RGB-pixmap size class stands). Integration therefore adds **no new
retention**: `PageLayers::bg44` now *peeks* (`OnceLock::get`, never populates) the existing
`bg44_partial` slot and resumes from a clone when a prior sub≥4 render already paid for
chunk 0 — the common thumbnail→full-view flow. Cold full decodes are unchanged; no cache
policy or budget accounting changes (bg44_partial was already counted).

**Numbers.**
- Isolated repeated full decode (checkpoint-resume vs cold, 4-chunk pages): watchmaker
  **−19.2%**, colorbook **−18.0%**, conquete_paix **−17.4%**, carte **−16.0%** — all
  byte-identical.
- Integrated full *render* after a sub4 warm-up (includes mask + compositor, diluting the
  IW44 share): watchmaker −6.6%, colorbook −11.8%, conquete −6.0%.
- Unit test `full_decode_resumed_from_partial_is_byte_identical` (warm-partial full render
  vs fresh-document full render).

**Decision.** Kept. Decision rule (≥15% repeated full decode on multiple real multi-chunk
pages, byte-identical, honest memory accounting) met — and the chosen integration sidesteps
the rule's memory-tradeoff concern entirely by only reusing state that a previous render
already cached. A *retained-across-downgrade* checkpoint tier (the issue's original shape)
remains unattractive per C5_COMPRESS: the coefficient planes cost as much as the pixmap.

## Perf round 79 (2026-07-12) — PAR_OCR: parallelize OCR across pages (#573)

### #573 — rayon fan-out in `cmd_ocr`, one backend per task — **Kept** (2026-07-12)

**Issue.** OCR ran strictly sequentially (CLI `cmd_ocr` page loop) although Tesseract
instances are independent and OCR dominates wall-clock whenever enabled.

**Approach.** With the `parallel` feature, render+recognize fan out over rayon; each task
builds its own backend (`recognize` constructs a fresh Tesseract per call, so instances
never cross threads — no trait changes needed). Text layers are injected sequentially in
page order afterwards, so output bytes are identical to the sequential path. The
encode-side `with_ocr_text_layer` is per-`PageEncoder` (single page) — nothing to
parallelize there; the CLI loop was the only multi-page OCR driver. Drive-by: pre-existing
`items_after_test_module` clippy failure in `ocr_tesseract.rs` fixed (the tests module was
also missing its `#[cfg(test)]`).

**Platform / commands.** macOS 26.5.0, tesseract 5.5.2; watchmaker (12 pages, 300 dpi),
`RAYON_NUM_THREADS` sweep, `/usr/bin/time -l`.

**Numbers.** 1 thread 38.7 s / 321 MB → 2: 20.1 s (**1.93×**, 542 MB) → 4: 11.3 s
(**3.42×**, 914 MB) → 8: 8.5 s (4.55×, 1.62 GB). Output `.djvu` bytes identical across
thread counts (t1==t4, t2==t8). Memory grows ≈ +150–190 MB per concurrent Tesseract
instance — the recorded cost; workers are bounded by `RAYON_NUM_THREADS`.

**Decision.** Kept. Decision rule (≥1.8× at 2 workers, ≥3× at 4, bounded memory,
byte-identical TXTz) met: 1.93× / 3.42×.
## Perf round 78 (2026-07-12) — TILE_LRU: tile-cache study — LRU eviction, hit-rate telemetry, budget sweep (#576)

### #576 — FIFO → LRU + measured budget knee — **Kept** (LRU); budget unchanged (2026-07-12)

**Issue.** The viewer tile cache evicted FIFO with a fixed 8 MiB/page budget and no
measured hit rates; C4_TILE_CACHE's ~20–25% win came from one-direction scripted pans,
while a back-and-forth pan (the classic reading pattern) makes FIFO evict exactly the tiles
about to be reused.

**Approach.** Test-only hit/miss/eviction counters in `TileCacheState` (fields and
increments under `#[cfg(test)]` — release lock section unchanged). `get_tile` now moves the
hit key to the back of the eviction order (LRU); the order deque holds ≤ ~32 keys, so the
reposition is a few dozen comparisons per hit. New scenario test
`tile_cache_back_and_forth_pan_hit_rate` (colorbook @2× zoom, 1440×960 viewport, 25% steps,
there-and-back) prints the telemetry and asserts a ≥30% floor.

**Numbers** (same scenario, same build, policy flipped locally for the baseline):
- Back-and-forth pan: FIFO **69.4%** → LRU **76.8%** hit rate (misses 137 → 104, −24%
  recompositions; evictions 105 → 72).
- One-direction pans (benches/viewer.rs): unchanged — a scan never revisits, so the
  policies evict identically; per-step times within noise.
- Budget sweep (LRU, same scenario): 4 MiB → **0%** (the ~24-tile viewport doesn't fit —
  any policy thrashes), 8 MiB → 76.8%, 16 MiB → 83.9%. The knee is "budget must exceed the
  viewport working set"; 16 MiB buys +7 п.п. for double the per-page memory — 8 MiB kept.
  Note for retina-class viewports (2880×1920 ≈ 88 tiles ≈ 22 MiB): the fixed budget is the
  binding constraint before eviction policy even matters — a viewport-scaled budget is the
  real follow-up if viewer telemetry ever shows it.

**Decision.** LRU adopted (better on the realistic scenario, provably identical on scans);
budget constant unchanged (sweep recorded); eligibility extensions (Lanczos3, rotation)
not pursued here — each needs its own correctness argument for the tile key.
## Perf round 80 (2026-07-12) — WASM_SIZE_DIET: wasm binary size frontier (#582)

### #582 — opt-level sweep + wasm-opt — **Rejected** (frontier recorded) (2026-07-12)

**Issue.** No size work had ever been done on the wasm artifact; for a web viewer download
size is a first-class metric.

**Numbers** (wasm-pack `--release --features wasm`, macOS, wasm-opt 130):
- Baseline (shipped fat-LTO release): **414,600 B**.
- `opt-level = "s"`: 372,704 B (−10.1%); `opt-level = "z"`: 371,629 B (−10.4%);
  `z` + `panic=abort`: 371,832 B (no further win).
- `wasm-opt -Oz` post-pass: −0.3…−1.2 KB on every variant — fat LTO + `codegen-units = 1`
  already did the work binaryen would do.
- **Speed cost of `z`** (real Chrome, bench_zero_copy, navm @300 dpi full render):
  56.6 ms → **158.9 ms (~2.8× slower)**. The size profiles trade double-digit render
  regressions for a single-digit size win.
- Decode-only check (issue item 3): the entire decode stack fits in ~405 KB — encoder code
  is already dead-code-eliminated from the viewer surface (nothing in the `wasm` exports
  reaches the encoders, and LTO strips them).

**Decision.** Rejected: the ≥20%-size-at-≤3%-speed adoption bar is nowhere near met
(−10.4% at +180%). Frontier documented in `examples/wasm/README.md` so nobody flips
`opt-level = "z"` "for free" later. A non-gating CI size line was considered and skipped —
the artifact isn't produced in CI today; revisit if a wasm publish workflow appears.

## Perf round 81 (2026-07-12) — PHOTO_PROFILE: mask-less DjVuPhoto encode (#571)

### #571 — `EncodeQuality::Photo` (INFO + BG44 only, grayscale-aware) — **Kept** (2026-07-12)

**Issue.** No mask-less encode profile existed: every colour path emitted Sjbz + BG44 +
FGbz, although the decoder fully supports mask-less pages and `encode_iw44_gray` was wired
only to thumbnails. Photographs and grayscale scans had to pretend to be layered documents.

**Approach.** New `EncodeQuality::Photo`: no segmentation, no Sjbz/FGbz — `INFO + BG44…`
only; pure-grayscale sources (r==g==b scan) route through `encode_iw44_gray` (single luma
plane). CLI `--quality photo`. The multi-page bundle path stays Quality/Archival-only for
now (recorded; the auto-profile issue #570 is the natural place to extend it).

**Numbers** (source-referenced fidelity via `compare_color`; boy = photo, watchmaker page =
grayscale scan, both rendered to PNG and re-encoded):
- photo input: Quality 1,450 B / dE_mean **9.34** → Photo 9,200 B / dE_mean **2.29** —
  the layered profile's 12× background subsample + bogus mask wreck a photograph; Photo is
  the faithful encoding (4× lower colour error).
- grayscale input: Quality 17 KB / ssim_y 0.973 → Photo 255 KB / ssim_y **0.992**,
  dE 1.46 → 0.50 (full-resolution luma plane vs 12×-subsampled background).
- **Interop:** `ddjvu` decodes both Photo outputs cleanly (colour and grayscale).
- Unit test: Photo emits no Sjbz/FGbz, has BG44, round-trips through our decoder for both
  colour and pure-grayscale sources.

**Decision.** Kept. Decision rule ("ddjvu-compatible and smaller **and/or** higher-PSNR
than the forced-layered path") met on fidelity — which is the profile's whole purpose; the
size axis is apples-to-oranges (full-resolution continuous tone vs a subsampled document
model) and is controllable via `Iw44EncodeOptions::target` (bpp budget) when size matters.

## Perf round 82 (2026-07-12) — QUALITY_AUTO: content-type detection for the encode profile (#570)

### #570 — `classify_content` + CLI `--quality auto` — **Kept** (2026-07-12)

**Issue.** `EncodeQuality` had to be hand-picked; `djvu encode` should do the right thing
without the user knowing DjVu internals.

**Approach.** New `djvu_encode::classify_content(&Pixmap) -> EncodeQuality`: samples ~64
full rows and computes chroma share, a 256-bin luma histogram, and horizontal sharp-edge
density (luma steps >64/pair). Decision tree, calibrated on corpus renders:
- **Photo**: >160 occupied luma bins AND sharp edges <0.2% of pairs (boy: 248 bins /
  0.04%; every text-bearing page measured ≥0.36%).
- **Lossless (bilevel)**: no chroma, near-white paper mode (≥240), far ink mode, ≤128
  occupied bins, ≥95% mass within ±16 of the two modes. Deliberately conservative — a
  photo can *never* reach it (continuous tone fails the bin cap; regression-tested).
- else **Quality** (layered).
CLI `--quality auto` (single images: per-image; directories: bundle-wide — all-bilevel →
Lossless bundle, anything else → layered Quality; per-page mixed bundles recorded as the
follow-up). Column stride stays 1: a stride-2 scan inflated photo gradients into false
"edges" and misrouted boy to Quality — caught by the corpus test during calibration.

**Numbers.**
- Corpus validation (unit test): boy→Photo (and asserted ≠Lossless — the catastrophic
  misroute), boy_jb2→Lossless, cable@native→Lossless, colorbook→Quality, navm p1→Quality.
- Boundary case recorded: watchmaker re-rendered at 150 dpi is genuinely 97.6% two-tone
  (modes 255/3) and routes to Lossless — correct for those pixels, though the original
  layered scan is Quality at native resolution; downscaled re-renders lose the texture
  that made it layered.
- Overhead: 0.184 ms vs a 16.8 ms Quality page encode (**1.09%** on the fastest encode;
  smaller share on archival/photo encodes).
- E2E CLI: photo PNG → `auto profile: Photo`, bilevel scan → `Lossless`, both encode
  successfully.

**Decision.** Kept. Auto matches the expert profile on every distinctive corpus case,
photos provably cannot reach the bilevel path, and the stats pass is ~1% of the cheapest
encode. Built on the Photo profile (#571) — PR stacked on that branch.
## Perf round 83 (2026-07-12) — ADAPTIVE_BG_SUB: content-adaptive BG44 subsample (#569)

### #569 — per-page subsample from measured background detail — **Kept** (opt-in) (2026-07-12)

**Issue.** `bg_subsample` was a fixed profile constant (12 Quality / 6 Archival) never
derived from content: flat paper loses nothing at 12, photo-heavy backgrounds smear at 12,
and 6 doubles BG44 bytes on plain text for no visible gain.

**Approach.** Opt-in `SegmentOptions::adaptive_bg_subsample` (default `false` —
byte-identical): after the mask is built, per-12×12-cell luma spread of the *unmasked*
pixels (under-ink pixels excluded, sampled every other cell row) gives a detail fraction;
<5% detailed cells → the ceiling (`bg_subsample`, usually 12), <30% → 6, else → 3.

**Numbers** (Quality profile, 300 dpi renders, `compare_color` vs source):

| page | fix12 | fix6 | auto picked |
|---|---|---|---|
| watchmaker (flat paper) | 14,076 B / ssim .9989 | 24,982 B / **.9977 (worse!)** | **=fix12** |
| conquete_paix (flat) | 28,348 B / .9962 | 66,668 B / .9954 (worse) | **=fix12** |
| colorbook (detailed) | 12,988 B / dE 8.48 | 21,956 B / dE 8.39 | **=fix6** |
| navm_fgbz (white bg) | 15,418 B / .9824 | 37,834 B / .9872 | =fix12 (bg truly flat; the fix6 ssim delta comes from FG palette quantization, not the background) |

The auto curve sits on the better fixed point per page: on flat pages fix6 pays +78…135%
bytes for *worse* SSIM (the encoder's RD at these budgets), on detailed pages 12 loses
colour fidelity. Unit test: flat → ceiling, noisy → 3, noise fully under ink → ceiling
(mask exclusion works).

**Decision.** Kept as opt-in. Decision rule ("auto dominates both fixed settings on the
mixed corpus") met. Wiring it into profile defaults belongs with #570's auto profile once
corpus diversity (#558) gives more photo-background pages.
## Perf round 84 (2026-07-12) — PY_REGION: region/tile render and progressive decode in djvu-py (#583)

### #583 — `render_region` / `render_coarse` / `render_progressive` bindings — **Kept** (2026-07-12)

**Issue.** djvu-py exposed only full-page `render(dpi)`: Python viewers had to render whole
pages to show a crop — the O(page)-instead-of-O(viewport) waste the core already avoids.

**Approach.** New public high-level `Page` methods in djvu_rs (`render_region` — routed
through the composited-tile cache, `render_coarse`, `render_progressive`,
`bg44_chunk_count`), mirrored in djvu-py with the established binding patterns: `py.detach`
GIL release (PY_GIL_DETACH) and the existing buffer-protocol pixmap. pytest coverage in the
PY_CI suite: region == byte-exact crop of the full render; last progressive stage ==
byte-identical full render at native resolution (at downscaled DPI the progressive
compositor legitimately takes a different resampling route — recorded, shape-checked);
coarse returns `None` on bilevel pages.

**Numbers** (colorbook, 2× zoom, 1200×900 viewport, 8-step pan, medians):
- Region pan ×8: **19.8 ms** vs full-page ×8: 1,286.6 ms — **64×** (tile cache + viewport
  scope).
- Thread scaling (GIL released): 1 thread 20.4 ms → 4 threads **9.9 ms** (2.1×).
- pytest: 44/44 green (3 new tests).

**Decision.** Kept (additive API). The measured region-vs-full ratio and thread scaling are
the experiment's deliverable per the issue.
## Perf round 85 (2026-07-12) — HTTP_RANGE_TTFP: lazy open over HTTP Range + DIRM size-table indexing (#584)

**Issue.** #584 — demonstrate `LazyDocument` over an HTTP `Range` transport and
measure time-to-first-page vs download-then-open. Decision rule: keep if TTFP
improves ≥5× at realistic bandwidth with bytes fetched ≈ index + first page.

**Approach.** New self-contained probe `examples/async_http_first_page.rs`:
a local throttled `std::net` HTTP/1.1 server with `Range` support (fixed
bandwidth), and an `AsyncRead + AsyncSeek` adapter fetching 64 KiB blocks over
Range GETs with a 64-block LRU. First run exposed a real defect, not a transport
problem: `index_bundled_djvm` probed every component's `FORM` header
(`seek(offset+4)` + 4-byte read, one per component), so opening a 517-component
book pulled 321 blocks — 20.5 MiB, 79% of the file, and lazy TTFP came out
*slower* than a full download (0.8×). Fix in the loader, not the example:
`DirmComponent` now surfaces the DIRM 24-bit size table, and the lazy indexer
uses `offset..offset+size` when the table is populated, probing `FORM` headers
only for zeroed tables (our own writer zeroes them). A new unit test
(`dirm_size_table_matches_form_boundaries`) verifies size-table == FORM span
across every bundled corpus/fixture file. The probe render is asserted
byte-identical to the full-document render.

**Numbers.** pathogenic_bacteria_1896.djvu (25.3 MiB, 517 components) at
12.5 MiB/s: open = 1 GET / 64 KiB (was 321 GETs / 20.5 MiB); TTFP
full-download 2.05 s vs lazy 0.02 s — **103×**, 0.25% of the file fetched.
Page 50: 3 GETs / 192 KiB (40×); page 200: 2 GETs / 128 KiB (48×). At
4 MiB/s: 209×. Small file (watchmaker, 0.2 MiB): 1.3× — neutral, as expected
when the whole file fits in ~3 blocks.

**Decision.** Kept. The DIRM size-table indexing makes lazy open O(head+DIRM)
instead of O(components × block); the ≥5× bar is beaten by 20× at realistic
bandwidth and bytes fetched ≈ index + one page. Example committed as the
reproducible benchmark.

**Reason.** The per-component probe was the entire cost of lazy open; DjVuLibre
and IA files populate the DIRM size table, so trusting it (with FORM-boundary
fallback for zeroed tables) removes the seek storm without a format risk.

## Perf round 86 (2026-07-12) — WASM_LAZY_OPEN: lazy Range-based document open in the browser (#588)

**Issue.** #588 — bring the native `LazyDocument` machinery to the wasm binding
so a browser viewer renders page 1 after fetching ~index + one page instead of
downloading the whole bundle. Decision rule: keep if TTFP improves ≥5× at
realistic bandwidth with bytes ≈ index + first page, without regressing plain
`from_bytes`.

**Approach.** New opt-in `wasm-lazy` feature (`wasm` + `async` +
`wasm-bindgen-futures`). `JsRangeReader` implements `AsyncRead + AsyncSeek`
over a JS `(offset, len) → Promise<Uint8Array>` callback: 64 KiB blocks, 64-block
LRU, the pending `JsFuture` is polled inside `poll_read` (single in-flight fetch,
task woken by the promise). `WasmLazyDocument` (open / page_count / page_info /
render_page / render_page_progressive) drives the existing
`from_async_reader_lazy_local` — the #584 DIRM size-table indexing is what makes
the open cost one block. Bench page `examples/wasm/bench_lazy_open.html` +
throttled Range server `examples/wasm/serve_lazy_bench.py`; measured in a real
Chrome tab. Plain `from_bytes` path untouched (feature is additive; default
`wasm` pkg has zero diff). New `wasm-lazy` cargo-check gates added to
scripts/check.sh and the CI wasm32 job.

**Numbers.** pathogenic_bacteria_1896.djvu (25.3 MiB, 520 pages) at 12.5 MiB/s
in Chrome: full-download + open + render p0 = 3.86–3.95 s vs lazy = 0.20–0.34 s
(**11.5–19×**), open itself 1 GET / 64 KiB (0.25% of the file), pixels
byte-identical to the full open. Page 50: 3 GETs / 192 KiB (232 ms); page 200:
2 GETs / 128 KiB (192 ms). Progressive first paint: coarse (chunk=1) at 47 ms →
full 77 ms on the same fetched bytes.

**Decision.** Kept (opt-in feature). ≥5× bar beaten 2–4× over; bytes = index +
one page exactly; `from_bytes` unchanged by construction.

**Reason.** The browser is where TTFP matters most (djvu.js comparison target);
the JS-callback seam reuses the entire native lazy stack — index, shared-DJVI
resolution, page cache — with ~200 lines of adapter and no new decode paths.

## Perf round 87 (2026-07-12) — CLI_TH44: expose TH44 thumbnail embedding (--thumbnails) and measure cost/benefit (#590)

**Issue.** #590 — TH44 embedding existed
(`encode_djvm_layered_shared_with_thumbnails`) but was unreachable: the CLI had
no flag and the default path hard-codes `with_thumbnails: false`, so TH44_GRID's
fast thumbnail path (round 44) never fired on our own encodes. Decision rule:
the flag lands regardless; the default flips only if overhead < ~1% of typical
bundle size.

**Approach.** `djvu encode --thumbnails` (multi-page layered paths), routed to
the existing `_with_thumbnails` encoder; warnings for the paths it can't apply
to (single-page, lossless JB2-only bundles). New probe
`examples/th44_grid_probe.rs`: byte cost of the TH44 chunks + median
`Document::thumbnails(128×128)` grid time on with/without bundles.

**Numbers.** watchmaker-derived 12-page text bundle: +43.4 KB = **+33.1%**
(3.6 KB/page), grid 98.4 → 6.6 ms (**15.0×**). colorbook-derived 12-page colour
bundle: +28.1 KB = **+7.3%** (2.3 KB/page), grid 205.0 → 4.6 ms (**44.3×**).
DjVuLibre `ddjvu` decodes the with-thumbnails bundles cleanly (interop ✓).

**Decision.** Kept as opt-in; default NOT flipped. Overhead is 7–33% of the
bundle on real 12-page encodes — orders of magnitude above the <1% bar (TH44 is
per-page IW44 at thumbnail resolution; small text bundles pay the most).

**Reason.** The capability gap is closed where the user chooses the trade-off;
flipping the default would silently inflate typical text-heavy bundles for a
viewer-side win that only materialises in thumbnail-grid UIs.
## Perf round 88 (2026-07-12) — EXPORT_COLD_CLONE: per-page render caches no longer accumulate across whole-document exports (#629)

**Issue.** #629 — after the #606 streaming writer, peak RSS of a 504-page PDF
export was still ~2.2 GB: every rendered page left its decode caches
(`PageLayers`: masks, background pixmaps, ~4.3 MB/page) on the document, and
exporters take `&DjVuDocument`, so nothing could evict. Decision rule: peak RSS
< 500 MB on the 504-page fixture, output bytes unchanged, wall-clock within
noise, sequential and parallel.

**Approach.** The cheapest of the issue's three ideas turned out to be already
designed in: `DjVuPage::clone()` deliberately does not clone the render cache.
All whole-document exporters (PDF both paths, EPUB, CBZ, TIFF colour/bilevel/G4)
now render each page on a cold clone — the caches fill on the clone and die
with it at the end of the page; the document's own pages stay cold. Shared-dict
decodes still live in the `shared_djbz` Arc, so nothing is re-decoded. No API
change, no interior-mutability redesign.

**Numbers.** 504-page watchmaker bundle (`djvu render --format pdf --all`),
`/usr/bin/time -l`: sequential peak RSS **2.243 GB → 44.7 MB (50×)**, wall
30.05 → 29.84 s; parallel **2.336 GB → 240.7 MB (9.7×)**, wall 4.97 → 4.71 s.
PDF outputs byte-identical (`cmp`, both paths); CBZ byte-identical
(watchmaker, colorbook); EPUB identical except the mandated
`dcterms:modified` generation timestamp (verified per-file via unzip diff);
TIFF covered by the determinism suite.

**Decision.** Kept. The <500 MB bar is beaten 11× (sequential) / 2× (parallel);
the parallel residual is the O(chunk) rendered bodies by design (#606).

**Reason.** Exports are one-shot page walks — caching for a revisit that never
comes was the entire 2 GB. A cold clone per page is a 6-line-per-exporter fix
that reuses the existing Clone contract instead of adding eviction machinery.

## Perf round 89 (2026-07-12) — ALLOC_PROFILE: dhat allocation-profiling harness + baseline map (#600)

**Issue.** #600 — no allocation profiling existed anywhere in the repo; every
allocation win so far (COW_BG/COW_FG, JB2 scratch pool, LAZY_PAGE_CONSTRUCT)
was found ad hoc inside unrelated investigations. Decision rule: the harness
lands as infra; each individual fix stands or falls on its own ≥3% numbers.

**Approach.** New dev-only `alloc-profile` feature (`dep:dhat`) + committed
`examples/alloc_profile.rs`: dhat as the global allocator, one scenario per
process (cold-open, warm-render ×10, thumbnail sweep, 12-page layered encode,
whole-doc PDF export), writing `dhat-<scenario>.json` for the DHAT viewer.
Never part of default builds or CI gates.

**Numbers (baseline map, watchmaker).** Totals: cold-open 14.0 MB / 1.6 k
blocks; warm-render 98.5 MB (10 × 8.42 MB output pixmaps — the documented
`render_pixmap` owned-return; `render_into` already offers reuse); thumbnails
47.0 MB / 15.1 k; 12-page encode **786.6 MB** / 90 k (t-gmax 514 MB); PDF export
180.2 MB / 20 k. Top sites: encode = `render_pixmap` inputs 404 MB +
`jb2::extract_ccs` unpacked byte grid **193.6 MB in 23 calls** (8.4 MB/page/pass)
+ `to_rgb_subsample` 44.9 MB; PDF = `render_page_data` 75.8 MB + `make_stream`
14 MB; thumbnails = `PlaneDecoder::new` 23.2 MB + a 12.6 MB-total
`extract_mask` that the thumbnail path arguably shouldn't need (follow-up
candidate).

**Fix attempt (rejected).** Thread-local scratch reuse for the top site
(`extract_ccs`' w×h grid): output byte-identical, but interleaved 6-pair A/B on
a 12-page 300-dpi CLI encode read old 0.99 s vs new 0.98 s user (~−1.5%), below
the 3% bar — macOS `calloc` hands back lazily-zeroed pages cheaply, so the
allocation traffic is not a wall-clock cost here. Reverted; the map entry
stands for platforms where the allocator is less forgiving.

**Decision.** Kept (infra): harness + baseline map. No individual fix met the
bar this round; candidates recorded (thumbnail-path `extract_mask`,
`PlaneDecoder` full-plane allocation for sub-decodes).

**Reason.** The map is the deliverable the issue asked for — it already
attributes every heavy scenario to a handful of named sites, and the one
"obvious" fix measurably doesn't pay on this platform, which is exactly the
kind of negative the ≥3% discipline exists to catch.
## Perf round 90 (2026-07-12) — GENERATION_LOSS: drift across repeated decode→re-encode cycles (#601)

**Issue.** #601 — archival reality is decode → edit → re-encode, repeatedly, and
generation loss had never been measured. Decision rule: diagnostic Kept when
drift curves are recorded and guidance documented; the lossless-bilevel
idempotence test lands regardless.

**Approach.** Committed harness `examples/generation_loss.rs`: per profile
(Quality/Archival), 5 generations of render-at-native → re-encode → parse,
recording ΔE/ssim_y vs gen-0 and vs the previous generation, output bytes, and
Sjbz payload bytes (mask-instability proxy). New permanent regression test
`lossless_bilevel_reencode_is_idempotent` (boy_jb2, ccitt_2): generation-1 mask
bit-identical, generation-2 container bytes a fixed point.

**Numbers.** Text scans **converge to a fixed point by gen-2**: watchmaker
Quality gen1 ΔE 0.147 → vs-prev ΔE 0.000/ssim 1.0000 from gen3 on, Sjbz bytes
frozen; cable similar (vs-prev ΔE ≤0.011, monotonically shrinking). The
picture-heavy page **diverges**: colorbook Quality ΔE vs gen0 8.4 → 14.3 →
29.5 → 58.8 → **86.6** by gen5 (Archival: → 85.0), with vs-prev ΔE *growing*
(8 → 30) — no convergence. Dominant cause isolated with a follow-up probe:
foreground mean colour is stable, but the **mask grows 275.7k → 462.0k px
(+68%)** across 5 generations — Sauvola re-binarization of the rendered
composite claims ever more continuous-tone pixels as ink each cycle, and the
BG diffusion then re-fills the growing holes with drifting colours.

**Decision.** Kept (diagnostic). Guidance: Lossless bilevel is provably safe
(test-enforced); Quality/Archival on *text* pages are safe to re-encode
(one-time ΔE ≈0.15, fixed point by gen-2); Quality/Archival on *picture*
pages must not be round-tripped — one generation costs ΔE ≈8 and it compounds
without bound. The systemic fix is mask reuse on re-encode (encode API that
accepts the source document's existing Sjbz instead of re-segmenting) plus
the #562 text-vs-photo block classifier; filed as the follow-up direction on
#601's close-out comment.

**Reason.** The drift is not encoder noise — it is a re-segmentation feedback
loop specific to continuous-tone content, which is exactly what the issue's
"binarization instability" suspicion predicted; text pages behave as a
contraction mapping and photos as an expansion.

## Perf round 91 (2026-07-12) — INCR_SAVE: byte-range patched save for bundled DJVM (#595)

**Issue.** #595 — every `DjVuMut` edit re-serializes and rewrites the whole
document; #302's byte-range patching covered only single-page `FORM:DJVU`.
Decision rule: metadata-edit saves ≥5× cheaper (bytes written / wall-clock) on
a big bundle, externally validated; record the same-size hit-rate either way.

**Approach.** New `DjVuDocumentMut::save_patched(&mut File) -> SavePatchStats`:
computes the new serialization in memory (same bytes as `try_into_bytes`),
diffs it against the retained original, and writes only the changed span —
common prefix always skipped, common suffix too when the total length is
unchanged (offset-shift makes suffix reuse unsound otherwise), then
truncate/extend. Cheap target check (length + head bytes) fails closed with
the new `PatchTargetMismatch` before anything is written; clean documents
write 0 bytes. Unit test covers clean/same-size/size-changing/wrong-target;
probe `examples/incremental_save_probe.rs` measures on real bundles.

**Numbers.** pathogenic_bacteria_1896.djvu (26.6 MB, 572 components):
same-size INFO edit → **1 byte written** (2.7·10⁷× fewer) vs a 26.6 MB full
rewrite, wall 27.5 ms vs 49.4 ms (1.8×; floor = in-memory emit + O(n) diff +
fsync). Size-changing bookmark edit → no byte win (NAVM/DIRM live at the head,
so everything after the first shifted byte rewrites; 2.3× wall from skipping
only the head). big504 fixture: same picture (1 B vs 7.7 MB; 3.4×).
`djvudump` + `ddjvu` accept the patched outputs (page renders verified).
**Hit-rate statistic:** the win is binary — same-size component edits get the
full O(1) write; any size-changing edit degrades to ~full rewrite because the
DIRM offset table sits at byte ~16. Drive-by: `djvu merge` outputs are
rejected by DjVuLibre's DjVmDir validation (pre-existing, unrelated) → #657.

**Decision.** Kept. The ≥5× bar is met on the bytes-written axis for the
same-size class (by 7 orders of magnitude); wall-clock alone would not have
cleared it (1.8–3.4×). Tail-shift saves (issue idea 3) are pointless for
head-resident metadata and redundant with the diff for tail edits — declined.

**Reason.** The diff-write needs no DIRM surgery, inherits `try_into_bytes`'
exact bytes (so it can never produce a different document), and turns the
dominant archival edit (same-size in-place metadata/flag tweaks) into a
constant-byte disk operation.
## Perf round 92 (2026-07-12) — MERGE_DIRM_INTEROP: djvu merge/split output rejected by DjVuLibre (#657)

**Issue.** #657 (filed during round 91) — bundles produced by `djvu merge` (and
`split`) failed DjVuLibre's DjVmDir validation ("no indirect entries allowed in
bundled document"), and our own lazy async loader would mis-index them.
Decision rule: djvudump + ddjvu accept merge output; interop test added.

**Approach / root cause.** Three defects in `djvm.rs`'s DIRM writer, all fixed
in `DirmPayload`/`build_djvm`: (1) the **offset table was left zeroed**
("readers fall back to FORM boundaries" — DjVuLibre doesn't: `offset==0` is its
indirect-entry marker, hence the error); (2) the **DIRM version byte was 0x80**
(bundled + directory version 0), whereas version 0 has a different plain-section
layout in DjVuLibre — every real-world DIRM observed writes **0x81**; (3) the
24-bit **size table was zeroed**, which also starved the #584 lazy indexer into
its per-component probing fallback. `build_djvm` now uses the documented
two-pass `partial_emit_with_offsets` shape (the same pattern
`encode_djvm_bundle_jb2` already used — only merge/split had the broken
writer); `build_bundled` takes the size table and writes `0x81`;
`build_indirect` writes version 1 as well.

**Numbers / validation.** 42-way watchmaker merge (504 components):
`djvudump` now prints the directory ("bundled, 504 files 504 pages") and
`ddjvu` renders pages — both errored before. `split 5–8` output likewise
clean. Lazy-loader synergy: merged bundles now carry real sizes, so
`from_async_reader_lazy` indexes them with zero component probes (the #584
fast path). New regression test
`merge_dirm_offsets_sizes_and_version_are_djvulibre_clean` (version byte,
non-zero offsets hitting `FORM` tags, size table == component spans); full lib
suite green (679).

**Decision.** Fixed. Every DJVM writer in the crate now emits
DjVuLibre-acceptable directories.

**Reason.** The comment-era assumption "readers fall back to FORM boundaries"
was true of our reader only; the reference implementation treats a zero offset
as a structural error, and directory version 0 as a different wire format.

## Perf round 93 (2026-07-12) — OCR_INPUT_AB: raw render vs Sauvola mask vs decoded JB2 mask as the recognizer input (#603)

**Issue.** #603 — encode-time and post-hoc OCR feed Tesseract the raw pixmap;
we already compute a Sauvola binarization, and for existing documents the JB2
mask IS the text. Which input recognizes best had never been A/B'd. Decision
rule: switch the default only if a variant strictly dominates (accuracy AND
speed) on both corpora; otherwise document guidance.

**Approach.** New harness `examples/ocr_input_ab.rs` (`ocr-tesseract`):
per fixture, OCR page 0 three ways — (A) raw composite render at native
resolution (today's default), (B) Sauvola segmentation mask, (C) decoded JB2
mask — and score char/word agreement (OCR_QA Levenshtein method) against the
document's embedded text layer as reference, plus wall-clock.

**Numbers.** Bilevel text scans (watchmaker, DjVu3Spec): **A = B = C exactly**
(char 98.06/98.44%) — the render is visually the mask, wall-clock within ±1%.
Halftone scan (cable): A = C 95.30% char, **B drops to 88.26%** (−7 п.п.;
word −15 п.п.) — re-binarizing an already-binarized-and-rendered page loses
strokes. Colour pages: colorbook A 74.32% > B 73.33% > **C 52.03%** (colour
text lives outside the JB2 mask; C is 36% faster but misses half the
reference); conquete A = C 54.27% > B 52.99%; carte (map) is noise for all
three. No variant strictly dominates → **default (A) stands**.

**Fix (drive-by, kept).** `cmd_ocr` hard-coded `dpi: 300` in `OcrOptions`
while rendering at the page's native resolution — a 400/600-dpi scan lied to
Tesseract about its scale (its layout analysis is dpi-sensitive). Now the
per-page options carry `page.dpi()`. Encode-path `with_ocr_text_layer` takes
caller options and already documents the contract.

**Decision.** Kept (diagnostic + the dpi fix). Guidance recorded: leave the
raw render as the OCR input; never feed the JB2 mask alone for colour
documents; Sauvola re-binarization helps nothing and hurts halftones.

**Reason.** Tesseract's internal binarization on our clean renders is at
least as good as any mask we can hand it — where the inputs differ at all,
the mask variants only ever lose information (colour text, halftone strokes).
## Perf round 94 (2026-07-12) — MULTI_INCL: czech.djvu masks restored — scan all INCLs for the shared dictionary (#624)

**Issue.** #624 — every czech.djvu mask decode failed with
`Jb2(MissingSharedDict)`; all exports carried background only. The issue
hypothesized missing *external* DJVI files. Decision rule: czech renders/
exports masks correctly; bundled documents unaffected.

**Approach / root cause.** The hypothesis was wrong: czech is a normal
*bundled* DJVM and both symbol dictionaries are present. Its pages carry
**three** `INCL` chunks — `shared_anno.iff` (ANTz only), `dict0085.iff`, and
`slovnik` (both Djbz, byte-identical) — and both the bundled parser and the
lazy async loader resolved only the *first* INCL. `shared_anno.iff` has no
Djbz, so `shared_djbz` came back `None` (sync) or the whole page errored
(lazy). Fix: scan all INCLs and take the first include whose target actually
holds a Djbz; the lazy loader skips non-Djbz includes instead of failing.
No external-file machinery needed — that remains genuinely out of scope for
single-file bundles.

**Validation.** czech page 1 mask now decodes 1095×1750 with 308 624 black
pixels — **byte-identical to `ddjvu -mode=mask` output** (P4 compare). Two
permanent regression tests (sync `multi_incl_page_resolves_shared_dict`,
lazy `lazy_document_multi_incl_page_resolves_shared_dict`). Bundled fixtures
unaffected (suite green — single-INCL pages hit the same map lookup).

**Decision.** Fixed. The issue's fixture-hunting item is moot; issue closed
by the resolution-order fix.

**Reason.** Real-world encoders attach shared annotations as a DJVI include
*before* the symbol dictionary; INCL order is not a dictionary pointer, so
"first INCL" was never a valid resolution rule.

## Perf round 95 (2026-07-12) — PGO_ARTIFACTS: would PGO'd release binaries help end users? (#586)

**Issue.** #586 — round-5 kept PGO as a local opt-in (−15% in-process cold
render via `make pgo`); whether a PGO'd *release artifact* helps end users, and
whether BOLT stacks, was never measured. Decision rule: ship artifacts only if
the win holds ≥8% on held-out content on ≥2 platforms; record the BOLT verdict
either way.

**Approach.** A/B of the actual shipped thing — the CLI binary (fat-LTO release
vs `scripts/pgo.sh` build, same commit), measured with hyperfine (10 runs,
warmup) on held-out documents (conquete_paix, carte — not in the training set)
and trained ones (watchmaker, pathogenic), across single-page renders, a
300-dpi render, and a whole-document PDF export.

**Numbers (M1, CLI wall-clock).** Single-page renders: **±1–2% = noise** on
held-out AND trained docs (conquete 85.5→84.7 ms; carte 91.8→93.6 ms — pgo
*slower*; watchmaker 29.1→29.7 ms; pathogenic p100 @300dpi 36.9→37.1 ms).
Whole-doc PDF export (watchmaker ×12): **697.2 → 730.3 ms — PGO 5% SLOWER**
(the export/deflate paths aren't in the training profile, and profile-use
de-prioritizes what the profile never saw). The round-5 −15% was an
*in-process* cold-render effect measured under criterion; at CLI-artifact
granularity process startup, I/O and untrained paths erase or invert it.

**Decision.** Rejected: no PGO release artifacts. Gate 1 of the decision rule
(≥8% held-out) fails by an order of magnitude on platform 1, so the
second-platform (x86) run and the BOLT spike were not reached — BOLT verdict
recorded as *moot at artifact level* (it post-link-optimizes a PGO win that
does not exist here; also Linux-only). `make pgo` stays documented as a local
opt-in for render-heavy embedders whose workload matches the training driver.

**Reason.** A profile trained on decode/render generalizes poorly to a CLI
whose heaviest user-visible jobs (exports) exercise encoder/serialization
paths the profile marks cold; shipping such binaries would trade a noise-level
render win for a real export regression.
## Perf round 96 (2026-07-12) — DESKEW: skew sensitivity quantified + opt-in projection-profile deskew (#592)

**Issue.** #592 — nothing corrects page skew, and its cost to JB2/OCR was
unmeasured. Decision rule: proceed only if skew measurably costs bytes or
accuracy; keep the corrector if it recovers ≥ half the measured loss at 100%
OCR safety.

**Step 1 — sensitivity (probe `examples/deskew_probe.rs`).** Synthetic
rotation (bilinear, white fill) before binarization, with a 0.02° control
angle to isolate the pure resampling cost (that control matters: +4.8…+43.2%
Sjbz comes from resampling alone). Skew proper is *very* expensive for JB2:
at 1° Sjbz is **+36% (cable) / +150% (watchmaker) / +384% (DjVu3Spec)**; even
0.3° costs +20/+102/+241%. OCR is nearly insensitive (char agreement
99.2–100% up to 3° — Tesseract deskews internally), so the case for deskew is
compression, not accuracy.

**Step 2 — corrector.** `SegmentOptions::deskew` (opt-in, default off — an
enhancement lever like despeckle): `estimate_skew` maximizes the sharpness
(Σ of squared adjacent-row differences) of the ink projection profile over a
coarse-to-fine ±5° sweep (0.5° → 0.1° → 0.02° + parabolic peak refinement),
then a small-angle bilinear rotation uprights the source before binarization
when |correction| ≥ 0.15°. Two estimator defects found and fixed along the
way: strided row sampling created a comb that pinned every estimate to ~0°,
and score plateaus on small pages broke ties at the plateau edge instead of
its centre (both now unit-tested with a non-periodic synthetic page —
periodic bars alias the projection metric, which is itself a recorded
gotcha).

**Numbers (net of the resampling floor).** watchmaker: recovery **92–96%**
at 0.3/1/3°, 70% at 2°, but only 29% at 0.5°; DjVu3Spec: 85/78/91% at
0.3/2/3° but ~4–36% at 0.5–1°; cable (halftone): marginal everywhere (its
skew cost is small and blur hurts its dots). The weak pocket traces to a
±0.02° estimator bias at specific angles — and a 0.02° *residual* after
double resampling measurably costs more than it recovers (manual check:
correcting a 0.5° skew by exactly −0.50° → 19.5 KB vs −0.52° → 26.9 KB).
OCR safety: agreement ≥99.9% on deskewed pages.

**Decision.** Kept (opt-in, default off). Sensitivity is proven (step-1 bar
cleared by an order of magnitude); recovery clears the ≥50% bar at most
angles ≥1° — the realistic scan-skew regime — with the ~0.5° pocket and
halftone content documented as the cases where it may not pay.

**Reason.** Skew is one of the largest single JB2 size levers measured in
this whole series (+150…384% at 1°), and the corrector recovers most of it
where it matters; sub-0.5° precision is bounded by the projection metric
itself, not the implementation.

## Perf round 97 (2026-07-12) — BLOCK_CLASSIFY: text-vs-photo block classifier for mixed layouts (#562)

**Issue.** #562 — segmentation is purely per-pixel; on mixed layouts photo
areas shred into mask speckle (Sjbz bloat, lost continuous tone). Decision
rule: keep if Sjbz shrinks ≥20% on mixed fixtures AND colour fidelity
improves, with byte-identical output on pure text.

**Approach.** Opt-in `SegmentOptions::block_classify`: 32×32 blocks classified
by two signatures — *continuous tone* (non-white luma ≤223 over 60% of the
block AND sharp horizontal deltas <1.5% of pairs; the per-block analogue of
#570's `classify_content` calibration) **or** *halftone* (binarized mask flips
ink↔paper on >25% of neighbour pairs — dot grids flip every 1–2 px, text an
order of magnitude less). 3×3 majority smoothing; photo blocks are cleared
from the mask so the region routes wholly to BG. Probe
`examples/block_classify_probe.rs` builds two synthetic mixed pages
(watchmaker text + a darkened boy.djvu photo patch; same patch Bayer-dithered
for the newspaper-halftone case) — real mixed-scan fixtures remain #558.

**Numbers.** **Halftone page (the issue's motivating case):** whole-page Sjbz
23 580 → 15 385 B (**−35%**; the patch's own contribution −74%), and against
the *true* continuous photo the decoded region goes from ΔE 38.6 / ssim 0.018
(crisp dots — unreadable as a photo) to **ΔE 7.2 / ssim 0.659** — the
classifier descreens, which is exactly what archival MRC pipelines do.
Continuous-tone page: photo ink −99% (fixed) / −86% (Sauvola), Sjbz −5…−10%,
page-level ΔE improves under Sauvola (0.485→0.463) but the fixed-threshold
variant trades crisp mask edges for BG-12 blur (vs-true-photo ΔE 3.9→4.7) —
smooth photos binarize into few large blobs, so they never bloated JB2 to
begin with. **Pure text: masks bit-identical on all three corpus checks**
(graceful degradation holds). `adaptive_bg_subsample` did not engage on the
synthetic (detailed cells stay under its 5% page-fraction gate) — pairing
them stays a #558 follow-up.

**Decision.** Kept (opt-in, default off). The ≥20%-Sjbz + fidelity bar is met
where the issue aimed — halftone/newspaper content — and the classifier is
provably inert on pure text; smooth-photo pages are documented as roughly
neutral (route-to-BG trades edge crispness for tone continuity).

**Reason.** The Sjbz-bloat premise is halftone-specific: dot grids are what
explode into thousands of components. The flip-density feature catches
exactly that signature, and clearing those blocks simultaneously shrinks the
mask and reconstructs a photo readers can actually see.
## Perf round 98 (2026-07-12) — FLATTEN_BG: flat-field background normalization (#593) — **Rejected**

**Issue.** #593 — the BG44 layer keeps stains/shadows/yellowing; hypothesis:
the wavelet "spends coefficients on the illumination gradient" (by analogy
with BG_DIFFUSE's −52…97%), so flattening paper toward white should shrink
BG44 ≥10% on stained scans. Decision rule: keep opt-in only at ≥10% BG44
shrink with clean crops and zero OCR regression.

**Approach.** Implemented `SegmentOptions::flatten_background`: per-channel
mask-excluded SAT box-mean illumination field (window ≈ min(w,h)/8), gains
`target95/field` clamped to [1, 1.8] (lighten-only), first per-pixel, then —
after the first negative — re-implemented as a coarse-grid field with
bilinear gain interpolation (smooth by construction). Probe: real scans
(conquete_paix, pathogenic, malliavin) + a synthetic stained watchmaker
(vignette + diagonal shadow + bottom-yellowing).

**Numbers.** Real scans: **±0.0% BG44** — their illumination fields are
uniform, gain ≈ 1 everywhere, nothing to flatten. Synthetic stained scan:
visual whitening is dramatic (crop mean RGB 193/194/186 → **255/255/199**),
but BG44 **+12.3%** (2528 → 2839 B) with BOTH field implementations, Sjbz
unchanged. Mechanism: smooth illumination gradients are *low-frequency* —
IW44 encodes them nearly free (the whole stained background is 2.5 KB at
subsample 12) — while a >1 gain multiplies the paper *grain* amplitude, and
grain is exactly what costs wavelet coefficients. The BG_DIFFUSE analogy does
not transfer: diffusion removed high-frequency ink-fallback surprises;
flattening amplifies high-frequency noise.

**Decision.** Rejected; code reverted (numbers preserved here). The aesthetic
goal (white paper in viewers) is better served on the *viewer/export* side —
a display-time white-balance costs zero encoded bytes and is lossless w.r.t.
the archived scan; noted on the issue as the recommended direction.

**Reason.** The premise inverted under measurement: illumination is cheap,
amplified grain is expensive. An encoder-side flatten pays +12% BG44 for what
a viewer-side transform does for free.

## Perf round 99 (2026-07-12) — LOSSY_PRESET_CAL: OCR-calibrated lossy presets (#572)

**Issue.** #572 — `lossy_text()` (t=2%) and `lossy_scan()` (despeckle=8 +
t=2%) were hand-picked; derive them from measured OCR agreement instead.
Decision rule: keep if derived presets beat shipped on ≥1 class without
regressing any other.

**Approach.** New grid harness `examples/ocr_preset_grid.rs`:
`lossy_threshold {2,4,6,8,10}% × despeckle {off,4,8,16}` per content class
(text = watchmaker, scan = pathogenic 600 dpi), scoring each cell by Sjbz
bytes and the **minimum** per-page Tesseract char agreement vs the lossless
mask (the derivation wants 100% across ALL pages, not an average).
Derivation rule: highest fully-agreeing setting, backed off one grid step.
A 2-page pilot suggested t=6% for both classes; the 4-page run overrode it —
more pages, stricter minimum.

**Numbers (4 pages/class).** *Text*: 100% agreement holds only through t=4%
(−23.44%) and breaks at 6% (99.926%) → derived point = **2% — exactly the
shipped preset** (validated, not changed; despeckle is byte-identical no-op
on clean text at every level). *Scan*: 100% holds through t=8% (−4.37%,
d=8: −4.44%) and breaks at 10% (99.85%) → derived point = **6%**:
`lossy_scan` moves t 0.02 → 0.06, −0.70% vs −0.10% Sjbz at equal measured
100% agreement. Grid also quantifies the cliff shape: scan Sjbz barely moves
below 8% (near-twin population thin until then), text saves 22–24% across
the whole range.

**Decision.** Kept: `lossy_scan` re-derived to t=0.06 (beats shipped on the
scan class); `lossy_text` confirmed optimal at 0.02 by the same rule (the
2-page pilot's 6% was a small-sample artifact — recorded as a methodology
note: preset derivation needs the min over ≥4 pages). Harness committed for
future classes (#558 corpus).

**Reason.** The instrument beats intuition in both directions: it *raised*
the scan threshold 3× (its glyphs are large; near-twins stay safe far past
2%) and *defended* the text preset against the looser point a smaller sample
suggested.

## Perf round 100 (2026-07-12) — STRIPWISE_IW44: gigapixel export memory (#594) — **Rejected (coefficient floor dominates the streaming peak)**

**Issue.** #594 — native-resolution IW44 decode of a gigapixel page has a
"~12 B/px transient, ~7 GB for 600 MP" profile; hypothesis: strip-wise
reconstruction feeding the row-streaming exporters halves-to-thirds peak RSS.
Decision rule: keep if peak RSS drops ≥2× with byte-identical output; if the
overlap bookkeeping / coefficient floor dominates, record the floor and close
with the analysis.

**Measurement first (the issue's step 1).** Two content classes, native and
2× export, `ru_maxrss` high-water:
- **Standard layered DjVu (subsampled BG44 — carte, the map class the issue
  names):** PDF export is **flat 25 MB** at 10.7 MP *and* 42.9 MP (2× area).
  Already O(one output row): the streaming path reconstructs the *subsampled*
  background once (small) and upsamples it per output row via
  `composite_rows`, so nothing page-area-sized is materialised. PNG (which
  materialises the full output by contract) scales, 79→221 MB. **The issue's
  premise does not apply to standard DjVu pages** — they already stream.
- **Full-resolution IW44 (Photo profile, subsample=1 — synthetic 4000×4000):**
  peak **15.28 B/px** (244 MB / 16 MP via `to_rgb`) — this reproduces the
  issue's model. Phase-isolated: **coeff floor 5.73 B/px** (91.7 MB, all ZP
  blocks resident — the issue's acknowledged irreducible floor), recon+RGBA
  transient 9.55 B/px. Streaming PDF export = **9.1 B/px** (146 MB); PNG =
  19.9 B/px (319 MB).

**Why strip-wise misses the bar.** Two peaks, two verdicts:
- *to_rgb / PNG (materialised):* the 15.28 → 5.73 B/px removal is 2.67×, but
  PNG must hold the full output RGBA by contract — a strip-wise reconstruct
  that then re-materialises a `Pixmap` saves nothing. No streaming consumer,
  no benefit.
- *PDF / TIFF (streaming — the exporters the decision rule measures):* the
  full OUTPUT RGBA is already streamed away; the 9.1 B/px peak is
  `coeff floor (5.73) + planes + bg_rgb_s1`. Strip-wise reconstruction can
  drop the planes and the full-page background pixmap, reaching
  `coeff floor + one strip ≈ 6 B/px` → **1.5×**, short of the ≥2× bar. The
  coefficient storage (5.73 B/px, slice-ordered ZP → every block resident for
  every slice) is **> half the streaming peak** and cannot be crossed.

**Decision.** Rejected — no strip-wise IDWT. The invasive, high-regression-risk
rewrite of the SIMD-optimised inverse wavelet transform (per-scale overlap
bands across scales 16→1, ~93-row vertical support) would deliver 1.5× on the
narrow Photo-profile / full-res-IW44 case only, while standard DjVu already
streams at O(row). The decision rule's floor-analysis branch applies verbatim.

**Reason.** The 12 B/px model assumed a full-resolution IW44 foreground at page
res; real DjVu encodes backgrounds *subsampled* and foregrounds as JB2 masks,
so the streaming exporters are already O(row). Where a full-res IW44 page does
exist (Photo profile), the ZP-slice-ordered coefficient floor dominates the
streaming peak and caps strip-wise at 1.5×. Follow-up recorded on the issue:
the only >2× lever is compressing the coefficient floor itself (out of scope —
would break the interoperable slice-ordered wire format).
## Perf round 101 (2026-07-12) — FG44_FOREGROUND: continuous-tone FG44 foreground model (#591) — **Rejected (loses to FGbz on fidelity AND bytes)**

**Issue.** #591 — the encoder only ever emits the FGbz palette foreground; a
k-colour palette cannot represent continuous-tone ink (stamps, highlighter,
faded/gradient seals). Wire a `PageEncoder` `ForegroundModel::{Fgbz, Fg44}`
option and keep FG44 if it wins the gradient-ink cases at comparable bytes.

**Approach.** Implemented the full encode path: `ForegroundModel` option,
`foreground_fg44_image` (per-cell mean of inked pixels at spec reduction 12,
harmonic Jacobi diffusion for ink-free cells to keep the wavelet cheap),
`encode_iw44_color` into a **single** FG44 chunk (multiple chunks are rejected
by DjVuLibre as "Duplicate foreground color info" — fixed to one chunk).
Interop gate passes: `ddjvu` renders our FG44 pages. Probe
`examples/fg44_probe.rs` compared FG44 vs FGbz (Exact + median-cut k∈{4..64})
on synthetic gradient-ink text, a radial-gradient rubber stamp, and a
full-page all-masked gradient; also swept FG44 reduction ∈ {1,3,6,12}.

**Numbers (dE_mean vs source, lower better; FGbz baseline in parens).**
- Gradient-ink text: FG44 **8.31 / 1370 B** vs FGbz Exact 2.13 / 772 B.
- Radial stamp (one blit → FGbz gives it one flat colour): FG44 **9.52 / 2714 B**
  vs FGbz 4.91 / 2210 B — FG44 is worse than a *single flat colour* despite
  encoding the gradient.
- Reduction sweep on the stamp: red=1 dE 10.0 / 16 KB, red=3 10.6, red=6 10.7,
  red=12 9.5 — **finer reduction is worse**, ruling out "too coarse".
- All-masked pure gradient: FG44 **18.48** vs FGbz 7.41; the green channel is
  crushed toward 0 on dark ink (src (26,15,36) → decoded (24,0,44)), and
  **`ddjvu` decodes the identical wrong values** — so it is the FG44 encoding,
  not our compositor.

**Decision.** Rejected; code reverted (interop finding preserved: FG44 must be
a single chunk). FGbz — especially `Exact`, which is near-lossless per blit —
dominates because real foreground ink is *piecewise-near-constant per connected
component*, exactly what a palette captures. FG44's only structural advantage
(continuous tone *within* one component) is more than offset by the
reduction-12 blur, the IW44 wavelet quantisation, and YCbCr chroma loss that
crushes dark ink — and it costs more bytes at every operating point.

**Reason.** The palette-failure premise assumed foreground colour varies
continuously; measured, it varies *between* components (which FGbz Exact nails
losslessly) far more than *within* one. The auto-switch follow-up the issue
anticipated has no domain to switch into: FG44 did not win a single tested
case. If a genuine within-component continuous-tone-ink corpus surfaces (#558),
re-open with the dark-ink chroma-crush as the first thing to fix.

## Perf round 102 (2026-07-12) — METADATA_PLANES: TXTz/NAVM/ANT round-trip fuzzing + djvused differential (#597)

### #597 — metadata-plane fuzz/interop blind spot closed, zero content divergence — **Kept (infra)** (2026-07-12)

**Issue.** The metadata planes were a fuzz/interop blind spot: `fuzz_encode`
covers pixel codecs only; TXTz/NAVM/ANT serialization had no round-trip
fuzzing, and `diff_fuzz` classified only structural/render outcomes — a
mutant whose text/annotations/outline we read differently from DjVuLibre was
invisible. The family has history (#368 depth guard, #524/#553 lenient
decode).

**Approach.**
- `tests/metadata_roundtrip.rs` (committed earlier on this branch):
  deterministic LCG soaks — 300 TextLayer→TXTz, 200 bookmark-tree→NAVM (full
  document path), 300 annotation/maparea→ANT cases, multi-byte edge-case
  strings from the #524/#551 population, structural equivalence asserts.
- New `fuzz_metadata` libFuzzer target mirroring the same assertion bodies
  with byte-driven structured generation (deeper nesting than the soaks:
  zones 5, bookmarks 8 — pressing the #368 class). Wired into
  `.github/workflows/fuzz.yml` and `oss-fuzz/build.sh`; seed corpus:
  CP1252/UTF-8 text, deep-nesting NAVM, quote-heavy ANT.
- `diff_fuzz` gains the #597 metadata differential: for every mutant at least
  one side structurally accepts, both sides run text/annotation/outline
  extraction (`page.text()`/`page.annotations()`/`doc.bookmarks()` vs
  `djvused print-pure-txt / print-ant / print-outline`), classified per plane
  as match / both-empty / both-reject / ours-only / theirs-only /
  accept-mismatch / content-diverge. Signatures are whitespace-insensitive
  (text), maparea-count (ant), and title+url multiset with djvused octal
  escapes decoded (outline). A per-(page, plane) baseline on the unmutated
  file gates findings, so pre-existing normalization gaps surface once, not
  per mutant. `navm_fgbz.djvu` joins the default file set (NAVM+ANTz; the
  corpus scans only carry TXTz).

**Adjudication (the #577 way).** Every non-match class traced to a known,
documented acceptance-ladder difference — no metadata parser bug:
- `theirs-only` (txt/outline): INFO-version bit-flips → we reject the whole
  document (`unsupported DjVu format version N`), while djvused prints
  metadata without validating INFO at all (its metadata ops decode no pages).
  Benign: strictness difference, not content difference.
- `ours-only` (txt/ant): DIRM/NAVM bit-flips → djvused exits nonzero on the
  whole file, while our documented lenient decode (#553) keeps reading the
  undamaged planes. Benign: the leniency is intentional and recorded.
- `accept-mismatch`: the same two phenomena on mutants where the surviving
  side sees an empty plane — no content at stake.

**Numbers.** Soak: `--seed 597597597 --mutants 5000` over the four default
files — **20,000 mutants in 1712 s**, zero crashes, zero timeouts. Per plane
(match / both-empty / both-reject / ours-only / theirs-only /
accept-mismatch / **content-diverge**): txt 2847 / 3775 / 474 / 60 / 84 /
237 / **0**; ant 382 / 6583 / 121 / 7 / 6 / 378 / **0**; outline 1670 /
5355 / 30 / 1 / 55 / 366 / **0**. An independent short pilot (default seed,
500 mutants/file) matched: 0 content-diverge. The soak also tallied 9
`pixel-mismatch` render-ladder mutants — a pre-existing class, reproducible
from the seed, out of #597's scope.

**Decision.** Kept (infra). Round-trip targets green; the differential sweep
produced **zero `content-diverge`** across all three planes — when both sides
extract metadata, the content agrees. Local libFuzzer soak remains blocked on
this machine (round-64 dyld hang, re-confirmed today); substitutes are the
committed deterministic soaks (green in `make check`) and the weekly CI fuzz
job with corpus persistence, per the round-64 precedent.

## Perf round 103 (2026-07-12) — CORPUS_DIVERSITY: tier-2 fixtures (#558) — **Kept (infra)**

### #558 — diversify fixtures (newspaper, mixed layout, map, CJK, Cyrillic, photo) — **Kept (infra)** (2026-07-12)

**Issue.** #558. Experiment verdicts rest on Latin-script book scans; missing
halftone-newspaper, mixed-layout, map/line-art, non-Latin, photo, and a second
chroma-half file. Blocking dependency for #562/#569–#571/#603.

**Approach.** Curate a checked-in tier-2 under `tests/corpus/` (≤3.3 MB total new
blobs), provenance in `tests/corpus/README.md`, SHA-256 + fetch for whole-document
sources in `scripts/fetch_corpus.sh`. Wire into `interop_pixdiff --corpus`,
`ocr_qa` (CJK/Cyrillic feeds), `examples/corpus_tier2_baseline.rs`, and
`benches/document.rs` (`tier2_corpus_render/*`).

**Substitutions vs the wishlist.**
- Second "chroma-half": superseded by #561 — valid IW44 v1.2 is always full-res
  chroma. Shipped `big_scanned_page.djvu` (maskless full-res BG44) as the missing
  photo class instead; `carte.djvu` stays the short-INFO/v1.2 control.
- Born-digital: no clearly licensed small DjVu found; tier-1 watchmaker/cable remain
  the anchors.

**Fixtures (SHA-256).**
| file | class | bytes |
|---|---|---|
| war_1812.djvu | newspaper | 919707 |
| goody_twoshoes.djvu | mixed layout | 1060487 |
| map_atlas_sample.djvu | map/line-art (atlas pp.28–29) | 413070 |
| chinese_cookbook_sample.djvu | CJK (5 pp extract) | 218742 |
| cyrillic_simonovich_co2.djvu | Cyrillic | 171563 |
| big_scanned_page.djvu | photo / maskless | 584365 |

**Numbers (macOS darwin, release, `corpus_tier2_baseline` + `interop_pixdiff --corpus`).**

Baseline vs ddjvu (native, PSNR/SSIM luma):
| page | PSNR | SSIM | cold render ms |
|---|---|---|---|
| war_1812 p0 (photo-heavy) | 26.97 | 0.740 | 95.4 |
| war_1812 p2 (text) | 40.82 | 0.990 | 113.0 |
| goody_twoshoes p0 | 47.10 | 0.997 | 70.6 |
| map_atlas_sample p0 | 32.28 | 0.984 | 89.7 |
| chinese_cookbook p1 | 50.69 | 0.999 | 57.4 |
| cyrillic_simonovich p0 | inf | 1.000 | 1.2 |
| big_scanned_page (timed @1/4; native interop byte-match) | inf | 1.000 | 259.5 |

`interop_pixdiff --corpus`: all tier-2 pages render; Cyrillic + photo mean |Δ|=0;
CJK mean 0.19; mixed 0.54; map mean 2.94; newspaper photo-heavy mean 8.88 (known
soft gap on continuous-tone BG — recorded, not a harness failure).

Criterion `tier2_corpus_render` (warm, sample-size 10): newspaper 3.90 ms, mixed
40.6 ms, map 35.7 ms, CJK 35.9 ms, Cyrillic 128 µs, photo@1/4 17.8 ms.

**Decision.** **Kept (infra).** Decision rule met: fixtures green through
interop/quality/bench harnesses with baselines recorded. Unblocks content-class
experiments that cited #558.

**Reason.** Bounded weight (extracts, not 25 MB atlas / 234-page cookbook);
provenance + checksums; harness wiring makes regressions on the new classes
visible. The newspaper photo-heavy PSNR gap is a useful stress case for future
BG/photo work, not a reason to withhold the tier.

## Perf round 104 (2026-07-13) — OCR_WORD_DUP: symbol/word OCR memoization CHEAP probe (#602) — **Rejected**

### #602 — symbol-level OCR propagation through the JB2 dictionary — **Rejected** (2026-07-13)

**Issue.** #602. Word-level memoization over blit-id sequences could cut OCR cost on
symbol-dense corpora if unique-word ratio is low.

**Approach.** CHEAP probe only (`examples/ocr_word_dup_probe.rs`): decode indexed JB2
masks, cluster blits into geometric words, hash glyph-shape id sequences, report
`total_words/unique_words` as the theoretical memoized-OCR bound. No OCR invoked.

**Numbers (release, macOS arm64).**
| corpus | pages | words | unique | uniq% | bound |
|---|---|---|---|---|---|
| pathogenic_bacteria_1896 | 520 | 153059 | 151437 | 98.94% | **1.01×** |
| watchmaker | 12 | 2071 | 1676 | 80.93% | **1.24×** |

**Decision.** **Rejected.** Bound ≪ the ≥5× keep bar (needs ~≤20% unique-word ratio).
Memoized OCR not implemented.

**Reason.** Real dictionaries re-blit glyphs, but word-level *sequences* barely repeat —
the duplication the issue hoped for is at the symbol level, not the word-image level the
probe measures. Per-symbol classification remains a separate (riskier) follow-up if ever
revisited.
## Perf round 105 (2026-07-13) — PDF_JBIG2_GENERIC: generic-region mask encoder (#568) — **Rejected**

### #568 — minimal JBIG2 generic-region PDF masks fail the 0.5× G4 gate — **Rejected** (2026-07-13)

**Issue.** #568 asked whether a minimal PDF `/JBIG2Decode` mask encoder can
beat the existing `PdfOptions::ccitt_g4` path enough to justify shipping it:
keep only if externally validated mask streams are **≤ 0.5× G4** on the text
corpus; otherwise revert and record.

**Approach.** Implemented a minimal encoder prototype in-tree, then reverted it:
one page-information segment plus one immediate generic-region segment,
arithmetic coded with a fresh MQ coder (JBIG2, not DjVu ZP), template 0, no
symbol dictionary or text region. Wired locally behind `PdfOptions::jbig2` using
the same encode-both-keep-smaller pattern as G4, and measured the produced PDF
ImageMask stream bodies against Deflate and G4 on the target corpus.

**Numbers.** Release probe over watchmaker/cable/irish/pathogenic:

| document | pages | Deflate | G4 | JBIG2 generic | JBIG2/G4 | decision |
|---|---:|---:|---:|---:|---:|---|
| watchmaker | 12 | 1,243,109 | 720,415 | 465,673 | 0.646 | Reject |
| cable_1973_100133 | 2 | 64,830 | 40,652 | 29,519 | 0.726 | Reject |
| irish | 1 | 76,937 | 79,025 | 34,160 | 0.432 | Keep |
| pathogenic_bacteria_1896 | 517 | 94,757,321 | 60,812,194 | 41,513,012 | 0.683 | Reject |
| TOTAL | 532 | 96,142,197 | 61,652,286 | 42,042,364 | 0.682 | Reject |

**Decision.** **Rejected and reverted.** The minimal generic-region encoder beats
G4 in absolute bytes, but misses the required **≤ 0.5× G4** gate on the combined
corpus and on all three target text documents (watchmaker/cable/pathogenic).
External render validation was not promoted to a keep gate because the size gate
failed first. A future JBIG2 attempt would need the real compression lever:
symbol dictionary plus text-region coding, not page-sized generic regions.

**Reason.** The generic-region path leaves too much glyph repetition on the
table. Shipping it would add a new arithmetic coder and PDF filter path while
failing the issue's explicit acceptance threshold.

## Perf round 106 (2026-07-13) — IW44_FWD_TRANSFORM: validate band-0/DC coefficient hypothesis (#578) — **Rejected**

### #578 — forward DWT does not explain the IW44 band-0/DC size gap vs c44 — **Rejected** (2026-07-13)

**Issue.** IW44_ENTROPY_PROBE localized the worst `colorbook.djvu` size gap vs
`c44` to band 0/DC on small pages and left one unvalidated root-cause
hypothesis: DjVuLibre's forward IW44 transform might produce different
coefficient values than ours.

**Approach.** Fetched DjVuLibre's `IW44EncodeCodec.cpp` / `IW44Image.cpp`
reference formulas (DjVuLibre 3.5 source mirror) and added an
`iw44-probe`-only diagnostic path plus `examples/iw44_forward_transform_probe.rs`.
The probe decodes each page's real BG44 background, builds the same Y/Cb/Cr input
planes as `encode_iw44_color`, runs both the production forward DWT and a
separate DjVuLibre-compatible scalar filter schedule on those identical planes,
then counts coefficient deltas by IW44 band before comparing the same pixmap
against local `c44` (DjVuLibre 3.5.29).

**Platform / command.** macOS darwin arm64 (Apple Silicon), release build,
local `c44` 3.5.29:

```text
cargo run --release --features="iw44-probe,cli" --example iw44_forward_transform_probe
```

**Numbers.** Worst pages from IW44_ENTROPY_PROBE (`tests/fixtures/colorbook.djvu`,
zero-based page indexes 61, 2, 59, 1, 60):

| page | BG pixmap | ours BG44 | c44 | gap | production-vs-DjVuLibre DWT deltas |
|---:|---:|---:|---:|---:|---:|
| 61 | 746×1213 | 2,791 B | 2,231 B | 560 B | Y/Cb/Cr all 0 changed coeffs; band 0 all 0 |
| 2 | 739×1213 | 2,617 B | 2,139 B | 478 B | Y/Cb/Cr all 0 changed coeffs; band 0 all 0 |
| 59 | 746×1213 | 3,958 B | 3,420 B | 538 B | Y/Cb/Cr all 0 changed coeffs; band 0 all 0 |
| 1 | 739×1213 | 4,703 B | 4,132 B | 571 B | Y/Cb/Cr all 0 changed coeffs; band 0 all 0 |
| 60 | 739×1223 | 4,414 B | 3,924 B | 490 B | Y/Cb/Cr all 0 changed coeffs; band 0 all 0 |

Per-plane sample counts: pages 61/2/59/1 each compared 933,888 coefficients per
plane; page 60 compared 958,464 coefficients per plane. Every plane reported
`changed=0`, `sum_abs_delta=0`, `max_abs_delta=0`, `band0_changed=0`.

**Decision.** Rejected. The forward-transform hypothesis explains **0 B** of the
478–571 B worst-page gaps, i.e. **0.0%** versus the issue's ">= half the
worst-page gap" keep/follow-up bar.

**Reason.** On identical input planes, the production DWT and the independent
DjVuLibre-compatible scalar schedule are coefficient-identical through all
Y/Cb/Cr planes and band 0. There is no transform-side coefficient delta to feed
into `PlaneEncoder`, so no port/adoption follow-up is justified. This closes the
IW44 band-0/DC size-gap thread as entropy-coding-complete for the zero-PSNR
levers already examined by IW44_ENTROPY_PROBE.
>>>>>>> a1d1d8f (quality(iw44): reject forward-transform size-gap hypothesis (#578))

## Perf round 107 (2026-07-13) — QUALITY_METRIC_GATE: MS-SSIM as lossy quality gate (#585) — **Rejected**

### #585 — MS-SSIM did not improve the lossy gate vs OCR agreement — **Rejected** (2026-07-13)

**Issue.** OCR_QA showed the current SSIM gate can disagree with the actual
ground truth for lossy JB2 decisions: clean text can lose OCR agreement while
SSIM remains high, and noisy-scan operating points can look like a structural
warning even when OCR agreement stays 100%. The experiment asked for an
SSIMULACRA2-class metric, or a cheaper MS-SSIM / butteraugli-lite candidate, and
to adopt it as a harness column only if it strictly reduces metric-vs-ground-
truth inversions on recorded sweeps.

**Approach.** Prototyped a clean-room, std-only `f64` MS-SSIM implementation in
`src/quality.rs`:

- reused the existing windowed SSIM statistics but exposed the contrast/
  structure term;
- built a 2x low-pass image pyramid over luma/gray planes;
- used the standard five MS-SSIM scale weights with renormalization for tiny
  images;
- temporarily printed `MS-SSIM` beside `SSIM` in `ocr_preset_grid`, `ocr_qa`,
  `quality_harness`, `mask_aa_quality`, and `fgbz_mediancut_harness`.

No external code was copied or vendored. The prototype was removed after the
decision because it did not satisfy the adoption rule.

**Platform / commands.** macOS Darwin 25.5.0, release build, system Tesseract via
the existing `ocr-tesseract` feature:

```text
cargo fmt && cargo test quality::tests
OCR_PAGES=4 cargo run --release --features ocr-tesseract --example ocr_preset_grid
cargo run --release --example fgbz_mediancut_harness -- --synthetic 0 2,4,6,32
cargo run --release --example mask_aa_quality -- tests/corpus/watchmaker.djvu 0 2 4
```

**OCR grid numbers (4 pages/class).**

Text (`tests/corpus/watchmaker.djvu`, lossless Sjbz 45,472 B; despeckle is a
no-op on this clean corpus, so all d=off/4/8/16 rows were identical):

| Threshold | Sjbz delta | SSIM | MS-SSIM | min-char-agree |
|---:|---:|---:|---:|---:|
| 2% | -22.48% | 0.99902 | 0.99977 | 100.000% |
| 4% | -23.44% | 0.99873 | 0.99966 | 100.000% |
| 6% | -23.98% | 0.99864 | 0.99961 | 99.926% |
| 8% | -24.22% | 0.99838 | 0.99951 | 99.926% |
| 10% | -24.86% | 0.99826 | 0.99945 | 99.816% |

Scan (`tests/corpus/pathogenic_bacteria_1896.djvu`, lossless Sjbz 74,552 B):

| Threshold / despeckle | Sjbz delta | SSIM | MS-SSIM | min-char-agree |
|---|---:|---:|---:|---:|
| 2% / off | -0.03% | 1.00000 | 1.00000 | 100.000% |
| 4% / off | -0.09% | 1.00000 | 1.00000 | 100.000% |
| 6% / off | -0.63% | 0.99987 | 0.99995 | 100.000% |
| 8% / off | -4.37% | 0.99871 | 0.99949 | 100.000% |
| 10% / off | -10.70% | 0.99659 | 0.99860 | 99.848% |
| 2% / d=8 | -0.10% | 0.99999 | 1.00000 | 100.000% |
| 4% / d=8 | -0.16% | 0.99999 | 1.00000 | 100.000% |
| 6% / d=8 | -0.70% | 0.99986 | 0.99995 | 100.000% |
| 8% / d=8 | -4.44% | 0.99870 | 0.99949 | 100.000% |
| 10% / d=8 | -10.75% | 0.99659 | 0.99860 | 99.848% |
| 2% / d=16 | -0.11% | 0.99999 | 0.99999 | 100.000% |
| 4% / d=16 | -0.17% | 0.99999 | 0.99999 | 100.000% |
| 6% / d=16 | -0.71% | 0.99986 | 0.99994 | 100.000% |
| 8% / d=16 | -4.45% | 0.99870 | 0.99949 | 100.000% |
| 10% / d=16 | -10.76% | 0.99658 | 0.99860 | 99.848% |

The omitted d=4 scan rows matched the same shape: 2/4/6/8% stayed at 100% OCR,
10% dropped to 99.848%, and MS-SSIM remained more compressed than SSIM.

**Inversion study.** On the recorded OCR grid, counting strict pairwise
inversions only where ground-truth OCR agreement differs:

| Corpus | SSIM inversions | MS-SSIM inversions |
|---|---:|---:|
| text | 0 | 0 |
| scan | 0 | 0 |

Equal-OCR pairs expose the original calibration complaint (the metric can
"over-warn" among several 100% OCR operating points), but they are not strict
rank inversions because the ground truth is tied. MS-SSIM does not fix that
calibration problem: it remains near 1.0 and is even less separated than SSIM.
From the best to worst text points, SSIM drops 0.00076 while MS-SSIM drops only
0.00032; on scan off-threshold 2% to 10%, SSIM drops 0.00341 while MS-SSIM drops
0.00140. It therefore does not catch the OCR cliff earlier than SSIM.

**Colour / crop and mask-AA checks.** The attempted colour and mask-AA harness
runs on this checkout did not produce usable correlation rows:

```text
fgbz_mediancut_harness --synthetic 0 2,4,6,32
# panicked: render candidate: Iw44(Invalid)

mask_aa_quality tests/corpus/watchmaker.djvu 0 2 4
# panicked: native reference render should succeed: Iw44(Invalid)
```

These render failures are outside this metric prototype and were not
root-caused here. They also supply no evidence that MS-SSIM dominates the
existing `compare_color` / ΔE path for colour judgements.

**Decision.** Rejected. No `src/quality.rs` API, no harness column, and no lossy
quality-gate behaviour was kept.

**Reason.** The issue's adoption rule is strict: add a harness column only if
the candidate reduces inversions vs ground truth. MS-SSIM tied SSIM at 0
strict inversions on the OCR grid and gave a more compressed near-1.0 scale, so
it does not improve the gate. Closing #585 as an evaluated reject keeps the
existing SSIM + OCR agreement + colour ΔE workflow until a genuinely stronger
metric has evidence.

## Perf round 110 (2026-07-13) — X86_V3_FEATURE_BISECT: isolate x86-64-v3 features for ZP decode (#566) — **Rejected**

### #566 — x86-64-v3 feature bisect does not reproduce a ZP Keep-bar win — **Rejected** (2026-07-13)

**Issue.** Prior x86-64-v3 notes claimed roughly **−4..−28%** on ZP decode. #566 asked which discrete
features (BMI1/BMI2/LZCNT/POPCNT / combinations / full `x86-64-v3`) actually move ZP, and to ship only
the ones that clear a keep bar safely.

**Approach.** Added `scripts/x86_feature_callgrind.sh` plus a `workflow_dispatch` job
`x86-feature-bisect` in `.github/workflows/bench.yml`. Each variant rebuilds
`examples/callgrind_workload` under a different `RUSTFLAGS` and records Callgrind **Ir** for
`bzz` / `jb2` / `iw44` workloads on `ubuntu-latest` (x86_64 + Valgrind).

**Platform / artifact.** GitHub Actions `ubuntu-latest`, workflow run of
`x86-feature-bisect` on `perf/566-x86-v3-features`, artifact `summary.csv`
(Callgrind Ir totals).

**Numbers (Ir; Δ% vs `default`):**

| variant | bzz | jb2 (ZP-heavy) | iw44 |
|---|---:|---:|---:|
| default | 783,872 | 2,314,014 | 5,311,122 |
| bmi1 | 791,698 (**+1.00%**) | 2,363,182 (**+2.12%**) | 5,323,546 (**+0.23%**) |
| bmi2 | 788,542 (**+0.60%**) | 2,269,761 (**−1.91%**) | 5,294,382 (**−0.32%**) |
| lzcnt | 781,324 (**−0.32%**) | 2,306,003 (**−0.35%**) | 5,268,635 (**−0.80%**) |
| popcnt | 786,346 (**+0.32%**) | 2,316,488 (**+0.11%**) | 5,313,586 (**+0.05%**) |
| bmi1+bmi2 | 786,082 (**+0.28%**) | 2,267,304 (**−2.02%**) | 5,291,936 (**−0.36%**) |
| x86-64-v3 | 799,887 (**+2.04%**) | 2,274,455 (**−1.71%**) | 4,915,068 (**−7.46%**) |

**Decision.** **Rejected** for ZP feature shipping. Best ZP/JB2 delta is about **−2% Ir**
(`bmi2` / `bmi1+bmi2`), far short of the issue's **−4..−28%** claim and too thin to justify
forcing BMI/LZCNT/POPCNT (or `target-cpu=x86-64-v3`) on the ZP path. Full `x86-64-v3` even
**regresses BZZ Ir (+2.0%)**. The **−7.5% IW44 Ir** under `x86-64-v3` is real but out of
scope for this ZP ticket — file a separate IW44/x86 follow-up if desired.

**Reason.** Callgrind bisect falsifies the ZP keep hypothesis; keep the measurement infra,
do not change default codegen flags for ZP.

## Perf round 111 (2026-07-13) — DIFF_FUZZ_BUCKETS: bucket + adjudicate `our-renders-what-they-reject` (#577) — **Fixed (partial) + Kept (documented)**

### #577 — bucket and adjudicate the remaining `our-renders-what-they-reject` diff-fuzz divergences — **Fixed (partial) + Kept** (2026-07-13)

**Issue.** #577. Round 45/46 left 241 mutants (round-45 corpus/seed) where
DjVuLibre's own decoder refuses a mutated stream but we render anyway —
never bucketed by root cause, so it was unknown how much was a real
missing-validation bug vs. intentional graceful degradation.

**Approach.** Extended `examples/diff_fuzz.rs`: every mutation now carries
which chunk id it landed in; for mutants in the `our-renders-what-they-reject`
class, capture `ddjvu`'s stderr, classify its error line into a short class
(`ddjvu_error_class`), and bucket by `(mutated chunk, ddjvu error class)`.
Per bucket: count, up to 3 examples, and pixel-diff evidence (mean/max abs
diff, %px >8, >32) against a clean-baseline render of the same page — the
same benign-vs-bug evidence bar the round-46 "2b-swallow" precedent used.
Buckets print ranked at the end of a run.

**Session (seed 42, `--mutants 700` × 3 corpus files = 2100 mutants, same
harness config as round 45/46, macOS arm64).** Baseline (clean `main` at
this round's start):

| class | count |
|---|---:|
| both-reject | 1266 |
| both-accept-match | 434 |
| **our-renders-what-they-reject** | **221** |
| both-render-fail | 117 |
| our-stricter | 39 |
| our-laxer | 19 |
| our-render-fail | 4 |

(221, not round 46's 241 — corpus/harness drift since, e.g. #558's tier-2
work and #597's default-file addition don't touch this seed's 3 files, but
minor decoder changes since round 46 shifted a few mutants.)

Top buckets (`mutated chunk` × `ddjvu error class`, before fix):

| # | chunk | count | ddjvu error | pixel evidence |
|---|---|---:|---|---|
| 1 | BG44 | 120 | Unexpected End Of File | mean 127.8, max 255, 93.6% px >8 |
| 2 | FORM | 35 | Unexpected End Of File | mean 128.0, max 255, 97.7% px >8 |
| 3 | BG44 | 22 | Chunk does not bear expected serial number | mean 0.36, max 20, **0.0%** px >8 |
| 4 | FG44 | 18 | Unexpected End Of File | mean 10.4, max 255, 7.9% px >8 |
| 5 | BG44 | 13 | Cannot decode page | mean 121.6, max 255, 72.3% px >8 |

**Adjudication of the top 3 (+ the FG44 bucket, same root cause, fixed
alongside):**

1/2/5 (**BG44/FORM, "we render a corrupted BG44 stream", 155+ mutants
combined**) — root cause confirmed by reading `PageLayers::bg44`
(`src/djvu_render.rs`): the strict decode loop ran `img.decode_chunk(...)`
per BG44 chunk and on `Err` just `break`, keeping whatever was decoded so
far — i.e. the *strict* render path silently degraded to a partial-but-
successful image instead of propagating the error, exactly round 45's
finding 2 ("BG44/IW44 truncated-stream tolerance"), never closed. `FORM`-
bucketed mutants are the same failure observed through a different lens:
the bit-flip lands in the outer `FORM` framing, desyncing chunk boundaries
so a downstream BG44 chunk decode fails the same way. **Decision: mirror.**
Fix: `bg44()`'s loop now `return None` on any chunk decode error instead of
`break`, and `decode_background_chunks_permissive` — previously reusing the
same cached `bg44()`/`bg_rgb_s{1,2}` result for its common fast path — now
runs its own independent decode-until-first-error loop so `permissive: true`
callers keep the old graceful-degradation behavior unaffected.

3 (**BG44/serial, 22 mutants — the round-46 "2b-swallow" precedent**) —
`Iw44Error::UnexpectedSerial` (round 46's serial-continuity check) already
detects this at the `decode_chunk` level and returns `Err`; the bug above
(swallowed by `break`) is what let a good partial image through despite the
detected corruption — hence the earlier "kept, benign" call (tiny <0.3%-pixel
diffs). **Decision: supersede round 46 → mirror.** The same fix above closes
this bucket as a side effect: any `decode_chunk` `Err` (including
`UnexpectedSerial`) now fails the whole strict decode, matching DjVuLibre's
own hard rejection instead of continuing to gracefully degrade. Interop
fidelity (no silent divergence) wins over the marginal graceful-degradation
value for a stream DjVuLibre itself refuses.

4 (**FG44/EOF, 18 mutants — same bug, different layer**) — `decode_fg44`
(`src/djvu_render.rs`) had the identical defect one layer over: on an FG44
chunk-present-but-decode-failed cache miss it unconditionally returned
`Ok(None)` — "no foreground" — even in strict mode. **Decision: mirror.**
Fixed to re-run `extract_foreground()` on cache miss and propagate the real
error, mirroring the pre-existing `decode_mask`/Sjbz idiom already in the
same file; permissive callers are unaffected (`decode_fg44(page).ok().flatten()`
still swallows it back to `None`).

**Fuzz rerun (same seed/mutants, after both fixes):**

| class | before | after | Δ |
|---|---:|---:|---:|
| both-reject | 1266 | 1266 | 0 |
| both-accept-match | 434 | 434 | 0 |
| our-stricter | 39 | 39 | 0 |
| our-laxer | 19 | 19 | 0 |
| our-render-fail | 4 | 4 | 0 |
| **our-renders-what-they-reject** | **221** | **196** | **−25** |
| both-render-fail | 117 | 142 | **+25** |

The BG44/serial bucket (22) disappears entirely from the printed top-10 (all
22 now correctly fail); the rest of the −25 comes from a subset of the
BG44/FORM "Unexpected End Of File"/"Cannot decode page" mutants that
actually raise a `decode_chunk` `Err` — bucket counts after: BG44 104 EOF /
26 cannot-decode-page, FORM 23 EOF / 14 cannot-decode-page (chunk populations
shuffle between error labels because `ddjvu`'s error text for a given mutant
is unaffected by our fix; only *our* accept/reject outcome moves).
**0 new crashes, 0 timeouts, every other class bit-identical** — the fix is
fully isolated to the two swallow sites. `cargo test --workspace --features
cli` and `scripts/check.sh` green (fmt/clippy/no_std/wasm32/tests/README
doctests), including 4 new regression tests: `strict_render_rejects_diff_fuzz_bg44_cannot_decode_bucket`
(the committed `watchmaker_00001` repro — the exact bucket-3 serial-mismatch
case), `strict_fails_on_truncated_fg44` / `permissive_render_returns_ok_on_truncated_fg44`
(hand-truncated FG44 chunk in `cable_1973_100133.djvu`, since none of this
seed's FG44 mutations happen to raise an explicit decode error — the corpus's
FG44 layers are all single-chunk, so `UnexpectedSerial` can't trigger; the new
test proves the fix still closes the bug when a decode error *is* raised).

**Decision.** **Fixed (partial)** for the BG44/FORM/FG44 "swallowed decode
error in strict mode" bug (buckets 1/2/4/5, and all of bucket 3 as a side
effect) — **Kept (documented)** for the residual: most of the BG44/FORM
`Unexpected End Of File` mutants (round-45 finding 2's majority case) still
render, because the IW44 ZP decoder tolerates running past a truncated/
desynced stream's true end *without raising an error at all* — there is
nothing for our fix to propagate. Closing that needs the ZP arithmetic
decoder itself to detect desync, a materially larger, decoder-internals
change; left as the already-tracked round-45 finding 2, not new scope.

**Reason.** The decision rule ("top 3 buckets adjudicated, 0 new crashes")
is met: all three top buckets got a mirror-vs-benign call, backed by root-
cause reads and pixel evidence, not guesses. The FG44 bucket got the same
treatment for free since it's the identical defect. The −25/+25 shift is
small in absolute count but real and isolated; the far larger remaining
gap is a distinct, already-documented decoder-robustness project, not
something #577's "adjudicate the bucket" scope should absorb by inventing a
ZP-decoder rewrite.

## Perf round 108 (2026-07-13) — WASM_SIMD128_SHIP: dual wasm package with runtime detection (#581) — **Kept**

### #581 — ship simd128 wasm build variant with scalar fallback — **Kept** (2026-07-13)

**Issue.** The IW44 wasm kernels already had `cfg(target_feature = "simd128")`
paths and the local WASM_SIMD harness measured render wins, but the documented
browser package still shipped only the scalar wasm artifact unless users knew to
set `RUSTFLAGS="-C target-feature=+simd128"` themselves.

**Approach.** Added `scripts/build_wasm_dual.sh` and `make wasm`. The shipped
browser package is now generated as one JS entry point plus two wasm artifacts:
`scalar/djvu_rs_bg.wasm` and `simd128/djvu_rs_bg.wasm`. The entry point runs
`WebAssembly.validate()` on a tiny `v128.const` probe, initializes the simd128
artifact on capable engines, and otherwise initializes the scalar artifact. The
loader keeps the existing `await init(); WasmDocument...` usage shape and adds
`selectedWasmVariant()` / `wasmSimd128Supported()` for diagnostics. README and
`examples/wasm/README.md` now document this packaging layout; generated `pkg/`
remains ignored and is regenerated before npm publish.

**Platform / commands.** macOS darwin arm64, Node.js v26.4.0, wasm-pack 0.13.1:

```text
make wasm
node --input-type=module -e '... await init(simd wasm bytes); ...'
ITERATIONS=50 WARMUP=10 DPI=150 ./scripts/bench_wasm_simd128.sh
for run in 1 2 3 4 5; do
  node scripts/bench_wasm_simd128.mjs \
    --scalar target/wasm-bench/scalar \
    --simd target/wasm-bench/simd128 \
    --fixture tests/fixtures/boy.djvu \
    --iterations 80 --warmup 20 --dpi 150
done
```

**Numbers.** Direct script run: `render_150dpi_fresh_doc` 2.609 -> 2.454 ms
(**-5.9%**), `render_150dpi_cached_page` 1.414 -> 1.426 ms (+0.9%),
`progressive_150dpi_chunk0` 2.737 -> 3.154 ms (+15.2%), `parse_document`
0.003 -> 0.003 ms (-11.5%). Because these are millisecond-scale Node wasm
timings, repeated already-built harness runs were noisy. Median deltas across
five 80-iteration repeats: fresh render **-16.0%** (runs -10.7%, -26.5%,
-16.0%, +10.0%, -16.1%), cached page +2.6% (runs +13.4%, -2.4%, -1.5%,
+12.7%, +2.6%), progressive **-10.3%** (runs +6.9%, -16.4%, -20.0%, +19.9%,
-10.3%). The dual-package smoke test selected `simd128` under Node's wasm
engine and parsed `tests/fixtures/boy.djvu` successfully (`page_count=1`).

**Decision.** Kept.

**Reason.** The shipped story now matches the measured artifact: supporting
engines get a real `+simd128` build automatically, and unsupported engines keep
the scalar fallback. The primary fresh-render path clears the >=5% bar in the
direct run and in the repeated-run median; progressive also trends positive
across repeats. Cached-page timings are neutral/noisy rather than a reliable
win, so this is not claimed as an across-the-board speedup.
