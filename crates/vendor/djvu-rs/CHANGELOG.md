# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.27.0](https://github.com/matyushkin/djvu-rs/compare/v0.26.2...v0.27.0) (2026-07-12)


### Features

* **cli:** expose TH44 thumbnail embedding via djvu encode --thumbnails ([#590](https://github.com/matyushkin/djvu-rs/issues/590)) ([#653](https://github.com/matyushkin/djvu-rs/issues/653)) ([51ff48d](https://github.com/matyushkin/djvu-rs/commit/51ff48d4f3513e4ab54c7445f9de25d7121ea82c))
* **encode:** automatic content-type detection for the encode profile ([#570](https://github.com/matyushkin/djvu-rs/issues/570)) ([#648](https://github.com/matyushkin/djvu-rs/issues/648)) ([4ac02e0](https://github.com/matyushkin/djvu-rs/commit/4ac02e02ef1690b99577fd94988fbe493bb47776))
* **encode:** DjVuPhoto profile -- mask-less BG44-only encode with grayscale path ([#571](https://github.com/matyushkin/djvu-rs/issues/571)) ([#647](https://github.com/matyushkin/djvu-rs/issues/647)) ([29c747c](https://github.com/matyushkin/djvu-rs/commit/29c747c5de305eeea3e63ddf51d9f2a7ae1eae06))
* **py:** region/tile render and progressive decode in djvu-py ([#583](https://github.com/matyushkin/djvu-rs/issues/583)) ([#650](https://github.com/matyushkin/djvu-rs/issues/650)) ([2c364ab](https://github.com/matyushkin/djvu-rs/commit/2c364ab75df8676a7d2b32d51058488cf22c78a2))
* **segment:** opt-in projection-profile deskew -- skew costs +150-384% Sjbz at 1 degree ([#592](https://github.com/matyushkin/djvu-rs/issues/592)) ([#663](https://github.com/matyushkin/djvu-rs/issues/663)) ([c3995ce](https://github.com/matyushkin/djvu-rs/commit/c3995ce45f6f32c127f62991940f3c9f9117a3d8))
* **wasm:** lazy Range-based document open -- WasmLazyDocument, 11-19x TTFP in Chrome ([#588](https://github.com/matyushkin/djvu-rs/issues/588)) ([#652](https://github.com/matyushkin/djvu-rs/issues/652)) ([6358073](https://github.com/matyushkin/djvu-rs/commit/63580737fed702f3703da469dc9d9f27b3382cfc))


### Bug Fixes

* **bench:** fail closed on missing Criterion results ([#616](https://github.com/matyushkin/djvu-rs/issues/616)) ([7e123bf](https://github.com/matyushkin/djvu-rs/commit/7e123bfb2d90c853fac65b71af81355d444d1d87))
* **bench:** measure both sides of the suspect re-check back-to-back ([#636](https://github.com/matyushkin/djvu-rs/issues/636)) ([3795fc5](https://github.com/matyushkin/djvu-rs/commit/3795fc558cd716246cc929dbd1b7ef056d43b5f0))
* **bench:** re-check flagged regressions against the merge base on the same runner ([#634](https://github.com/matyushkin/djvu-rs/issues/634)) ([63a471d](https://github.com/matyushkin/djvu-rs/commit/63a471da5183315ef880bd6f6386349f9e2b4303))
* **decode:** resolve the shared dictionary across all INCL chunks -- czech.djvu masks restored ([#624](https://github.com/matyushkin/djvu-rs/issues/624)) ([#661](https://github.com/matyushkin/djvu-rs/issues/661)) ([644d9bb](https://github.com/matyushkin/djvu-rs/commit/644d9bbd253e8f47842fc1f23ca7db8ce514acb7))
* **djvm:** DjVuLibre-clean DIRM for merge/split -- fill offsets/sizes, write directory version 1 ([#657](https://github.com/matyushkin/djvu-rs/issues/657)) ([#659](https://github.com/matyushkin/djvu-rs/issues/659)) ([2c822d8](https://github.com/matyushkin/djvu-rs/commit/2c822d8abc6941829240e94aa90f2c968fafa83a))
* **iw44:** decode v1.2 chroma at full resolution ([#614](https://github.com/matyushkin/djvu-rs/issues/614)) ([4ae4435](https://github.com/matyushkin/djvu-rs/commit/4ae44357f42d7d76397b76130921c37fd60d889c))
* **pdf:** paint FGbz-coloured foreground text with per-colour stencils ([#559](https://github.com/matyushkin/djvu-rs/issues/559)) ([#619](https://github.com/matyushkin/djvu-rs/issues/619)) ([d24c9fa](https://github.com/matyushkin/djvu-rs/commit/d24c9fadb29501889e199fdd5c6143bef3499b9b))
* **pdf:** restore lost and invisible foreground masks in PDF export ([#620](https://github.com/matyushkin/djvu-rs/issues/620), [#621](https://github.com/matyushkin/djvu-rs/issues/621)) ([#622](https://github.com/matyushkin/djvu-rs/issues/622)) ([d0598ac](https://github.com/matyushkin/djvu-rs/commit/d0598ac39590404993483a48f6998f34bf0bd385))


### Performance Improvements

* **async:** index bundled DJVM from the DIRM size table -- 103x TTFP over HTTP Range ([#584](https://github.com/matyushkin/djvu-rs/issues/584)) ([#651](https://github.com/matyushkin/djvu-rs/issues/651)) ([ad041b5](https://github.com/matyushkin/djvu-rs/commit/ad041b50e38eafb26c2ccb6554774e482c0b9c6a))
* **cache:** preserve the 1/4-res JB2 mask across render-cache downgrade ([#607](https://github.com/matyushkin/djvu-rs/issues/607)) ([#627](https://github.com/matyushkin/djvu-rs/issues/627)) ([9355aa6](https://github.com/matyushkin/djvu-rs/commit/9355aa69dd12ff3a364f62eb77f3a6593231af47))
* **cli:** parallelize CBZ export via a new cbz library module ([#598](https://github.com/matyushkin/djvu-rs/issues/598)) ([#625](https://github.com/matyushkin/djvu-rs/issues/625)) ([ddfd111](https://github.com/matyushkin/djvu-rs/commit/ddfd11101611e67ca35020e9cc7c690bb8499b9f))
* **encode:** build the FGbz palette from encoder blit metadata ([#612](https://github.com/matyushkin/djvu-rs/issues/612)) ([#626](https://github.com/matyushkin/djvu-rs/issues/626)) ([238ae11](https://github.com/matyushkin/djvu-rs/commit/238ae1169b07ba54fbfbe83fca96edf4199ce8a5))
* **encode:** two-pass layered bundle encode drops backgrounds and mask clones ([#565](https://github.com/matyushkin/djvu-rs/issues/565)) ([#637](https://github.com/matyushkin/djvu-rs/issues/637)) ([344efc6](https://github.com/matyushkin/djvu-rs/commit/344efc673876adf6633e4561f9ee4233d612a700))
* **export:** render whole-document exports on cold page clones -- 504-page PDF peak RSS 2.24 GB -&gt; 45 MB ([#629](https://github.com/matyushkin/djvu-rs/issues/629)) ([#654](https://github.com/matyushkin/djvu-rs/issues/654)) ([037c9fc](https://github.com/matyushkin/djvu-rs/commit/037c9fc457f30e8172a0e32deb107d6b872cb90b))
* **iw44:** resume full decode from the cached first-chunk state ([#608](https://github.com/matyushkin/djvu-rs/issues/608)) ([#643](https://github.com/matyushkin/djvu-rs/issues/643)) ([2c4b269](https://github.com/matyushkin/djvu-rs/commit/2c4b2690d7f25fb429c784d7bbc6051fcea8ddea))
* **metadata:** cache decoded TXTz and ANTz layers per page ([#605](https://github.com/matyushkin/djvu-rs/issues/605)) ([#635](https://github.com/matyushkin/djvu-rs/issues/635)) ([6d4141f](https://github.com/matyushkin/djvu-rs/commit/6d4141f1848c94403af27914c2aceb13ea2fb047))
* **mut:** incremental byte-range save for bundled DJVM -- same-size edits write 1 byte instead of the file ([#595](https://github.com/matyushkin/djvu-rs/issues/595)) ([#658](https://github.com/matyushkin/djvu-rs/issues/658)) ([2990793](https://github.com/matyushkin/djvu-rs/commit/2990793189824703fc9d5a8f93b7c28d9c7af2fd))
* **ocr:** parallelize OCR across pages in the CLI ([#573](https://github.com/matyushkin/djvu-rs/issues/573)) ([#645](https://github.com/matyushkin/djvu-rs/issues/645)) ([9520458](https://github.com/matyushkin/djvu-rs/commit/9520458486160360eb128ae9290bd4e9e5aa79f1))
* **pdf:** stream PDF objects to a writer to bound serialization memory ([#606](https://github.com/matyushkin/djvu-rs/issues/606)) ([#630](https://github.com/matyushkin/djvu-rs/issues/630)) ([a63b680](https://github.com/matyushkin/djvu-rs/commit/a63b680ba15a6f43ec2c0153444c03b2161a477d))
* **render:** LRU tile-cache eviction + hit-rate telemetry and budget sweep ([#576](https://github.com/matyushkin/djvu-rs/issues/576)) ([#644](https://github.com/matyushkin/djvu-rs/issues/644)) ([17f7d95](https://github.com/matyushkin/djvu-rs/commit/17f7d953ef14ba462ba0fb216120ddcd66175d59))
* **segment:** parallelize the Sauvola threshold pass ([#575](https://github.com/matyushkin/djvu-rs/issues/575)) ([#623](https://github.com/matyushkin/djvu-rs/issues/623)) ([5b24f50](https://github.com/matyushkin/djvu-rs/commit/5b24f50939690d2e2ca100919e6407f1e47917e5))
* **wasm:** coarse-grained batch page rendering ([#610](https://github.com/matyushkin/djvu-rs/issues/610)) ([#642](https://github.com/matyushkin/djvu-rs/issues/642)) ([db65a1a](https://github.com/matyushkin/djvu-rs/commit/db65a1a492177657c9acfafb20466cbf94f56fcd))
* **wasm:** open documents through the lazy shared backing ([#609](https://github.com/matyushkin/djvu-rs/issues/609)) ([#632](https://github.com/matyushkin/djvu-rs/issues/632)) ([8b81993](https://github.com/matyushkin/djvu-rs/commit/8b8199314199b212a1a37751b9b0fce26a477d80))
* **wasm:** reusable Rust-owned pixel buffer with zero-copy view ([#611](https://github.com/matyushkin/djvu-rs/issues/611)) ([#631](https://github.com/matyushkin/djvu-rs/issues/631)) ([dea47a5](https://github.com/matyushkin/djvu-rs/commit/dea47a56de03c8246e0e9ab2199948c37d57b332))
* **wasm:** ship simd128 package variant ([1b2e4db](https://github.com/matyushkin/djvu-rs/commit/1b2e4dbe80866b5a489c003c338242868936f8dc))
* **wasm:** ship simd128 package variant ([#581](https://github.com/matyushkin/djvu-rs/issues/581)) ([81acb78](https://github.com/matyushkin/djvu-rs/commit/81acb7824dff6f8eef1d36a56ae86f429d55c8f2))

## [0.26.2](https://github.com/matyushkin/djvu-rs/compare/v0.26.1...v0.26.2) (2026-07-10)


### Performance Improvements

* BG-heavy colour-encode bench + land PAR_PAGE_LAYERS (Sjbz ∥ BG44 join) ([#496](https://github.com/matyushkin/djvu-rs/issues/496)) ([#555](https://github.com/matyushkin/djvu-rs/issues/555)) ([ad29126](https://github.com/matyushkin/djvu-rs/commit/ad29126b411a81cdeda60040e137da3321d368f1))

## [0.26.1](https://github.com/matyushkin/djvu-rs/compare/v0.26.0...v0.26.1) (2026-07-09)


### Bug Fixes

* decode ANTa/ANTz annotation payloads leniently — one bad byte no longer drops all annotations ([#553](https://github.com/matyushkin/djvu-rs/issues/553)) ([#554](https://github.com/matyushkin/djvu-rs/issues/554)) ([9806824](https://github.com/matyushkin/djvu-rs/commit/98068241cec25820fe628efbd63d148c6af7ca03))
* decode non-structural strings leniently — CP1252 fallback for NAVM/TXTz/METa ([#524](https://github.com/matyushkin/djvu-rs/issues/524)) ([#551](https://github.com/matyushkin/djvu-rs/issues/551)) ([a461a8a](https://github.com/matyushkin/djvu-rs/commit/a461a8a411e6124660ae5e8be3a9428b7f45f1a8))

## [0.26.0](https://github.com/matyushkin/djvu-rs/compare/v0.25.0...v0.26.0) (2026-07-09)


### ⚠ BREAKING CHANGES

* `djvu_rs::pdf` now requires the `pdf` feature (`djvu-rs = { version = "…", features = ["pdf"] }`); the `cli` feature enables it automatically.

### Features

* decouple writer-side deps from std — pdf/epub/cli own them ([#509](https://github.com/matyushkin/djvu-rs/issues/509)) ([#547](https://github.com/matyushkin/djvu-rs/issues/547)) ([62f30f6](https://github.com/matyushkin/djvu-rs/commit/62f30f690d6d8035e04398c8c654af0bbe54274e))

## [0.25.0](https://github.com/matyushkin/djvu-rs/compare/v0.24.4...v0.25.0) (2026-07-06)


### Features

* **encode:** TXTZ_OCR — encode-time OCR text layer via PageEncoder (round 51) ([#540](https://github.com/matyushkin/djvu-rs/issues/540)) ([3cad5c3](https://github.com/matyushkin/djvu-rs/commit/3cad5c306218884c8ea5b16981269509cc18250f))
* **fgbz:** median-cut foreground palette quantiser (opt-in) ([#522](https://github.com/matyushkin/djvu-rs/issues/522)) ([440ed07](https://github.com/matyushkin/djvu-rs/commit/440ed0772d4ed571d9f71d5862bd59b2be2c56d9))
* **jb2:** lossy_text() preset — expose the −22% text lossy lever (Branch B complete) ([#514](https://github.com/matyushkin/djvu-rs/issues/514)) ([fad0c28](https://github.com/matyushkin/djvu-rs/commit/fad0c28ff5345847ce0a19e85e245d9061963c55))
* **pdf:** PDF_G4 — CCITT Group 4 encoding for bilevel PDF masks (round 51) ([#543](https://github.com/matyushkin/djvu-rs/issues/543)) ([470b40b](https://github.com/matyushkin/djvu-rs/commit/470b40b31538b0011be52bf69b155ab75f36e935))
* **render:** D_AA_ZOOM — opt-in bilinear mask-coverage AA at upscale ([#517](https://github.com/matyushkin/djvu-rs/issues/517)) ([54a0939](https://github.com/matyushkin/djvu-rs/commit/54a09398e46402ef4b096ef59ce7237906d8ec23))
* **render:** streaming ProgressiveDecoder — the deferred B5 stateful API ([#510](https://github.com/matyushkin/djvu-rs/issues/510)) ([1760bd0](https://github.com/matyushkin/djvu-rs/commit/1760bd0ebd72bc14276c85a862dd027bb9fda5c9))
* **render:** TH44_GRID — fast thumbnail-grid decode path via embedded TH44 (opt-in) ([#533](https://github.com/matyushkin/djvu-rs/issues/533)) ([1c36093](https://github.com/matyushkin/djvu-rs/commit/1c360930df68588707fa44a3e31c3a4d4e520979))
* **wasm:** WASM_THREADS — opt-in rayon thread pool via wasm-bindgen-rayon (feasibility + infra) ([#528](https://github.com/matyushkin/djvu-rs/issues/528)) ([e7d523b](https://github.com/matyushkin/djvu-rs/commit/e7d523b6bd72f84a9edbdc7b32f2c103d73f44b5))


### Bug Fixes

* **iff,interop:** INTEROP_INFO — INFO version ceiling + gamma clamp vs DjVuLibre ([#538](https://github.com/matyushkin/djvu-rs/issues/538)) ([518834b](https://github.com/matyushkin/djvu-rs/commit/518834b4f67ba74c36ee22224dc6856f7718e893))
* **iff,test:** carte.djvu short-INFO parse rejection + mmap/parallel decode-count test ([#531](https://github.com/matyushkin/djvu-rs/issues/531)) ([097f9de](https://github.com/matyushkin/djvu-rs/commit/097f9de953b56958fb9195cc2e6360419560dd19))
* **iw44:** INTEROP_STREAMS — BG44/IW44 serial + dimension checks, JB2 divergence root-caused (round 46) ([#539](https://github.com/matyushkin/djvu-rs/issues/539)) ([c524585](https://github.com/matyushkin/djvu-rs/commit/c5245857bf2e9909ff71b1d7f3aaf62f5d80fbdf))
* **iw44:** tolerate zero-length BG44 refinement chunk payload (BUG-ZPSHORT) ([#518](https://github.com/matyushkin/djvu-rs/issues/518)) ([2387d88](https://github.com/matyushkin/djvu-rs/commit/2387d88e2919866a2e741e56c811c91ae072d421))


### Performance Improvements

* **bench:** large single-page colour-encode bench + PAR_PAGE_LAYERS revisit ([#513](https://github.com/matyushkin/djvu-rs/issues/513)) ([b5363b1](https://github.com/matyushkin/djvu-rs/commit/b5363b150c06c2304f8747ac132ad42130c44367))
* **diag:** BZZ encoder + PDF JPEG-background diagnostics (no shipped-code changes) ([#515](https://github.com/matyushkin/djvu-rs/issues/515)) ([2012345](https://github.com/matyushkin/djvu-rs/commit/2012345f0391ae77a33b6f2728d232acec95e2f8))
* **diag:** JB2_DICT_ORDER — shared-dict index permutation, no size win (Round 24) ([#520](https://github.com/matyushkin/djvu-rs/issues/520)) ([d1463ac](https://github.com/matyushkin/djvu-rs/commit/d1463ac171677a3bf8f625f5d4b573ef6a15c42d))
* **fuzz:** DIFF_FUZZ — corpus-mutation differential fuzzer vs DjVuLibre ([#534](https://github.com/matyushkin/djvu-rs/issues/534)) ([01570eb](https://github.com/matyushkin/djvu-rs/commit/01570ebc5b4f35f38f61d18b7ffce23b11e71f3d))
* gray/Lanczos/memory sweep + D1 quality harness (rounds 7-14) ([cfd018a](https://github.com/matyushkin/djvu-rs/commit/cfd018a23c40e7fdfef7c6d368ec1771321b5e9b))
* **iw44:** AVX2_IDWT — rejected (runtime dispatch regresses IDWT, round 51) ([#541](https://github.com/matyushkin/djvu-rs/issues/541)) ([d749712](https://github.com/matyushkin/djvu-rs/commit/d7497125b2a1b39db06684d8dc241ac246b02af1))
* **iw44:** harmonic BG diffusion for masked cells (−52% to −97% BG44, quality up) ([#505](https://github.com/matyushkin/djvu-rs/issues/505)) ([5d09e0c](https://github.com/matyushkin/djvu-rs/commit/5d09e0cd44a2ac0fba0b1102a188e4f5fa6db42c))
* **iw44:** IW44_ENTROPY_PROBE — localize textured-content size gap vs c44 (round 51, diagnostic) ([#542](https://github.com/matyushkin/djvu-rs/issues/542)) ([a94fc84](https://github.com/matyushkin/djvu-rs/commit/a94fc84d1648d8db9ddda3b641426f5f8d84308b))
* **jb2:** borrow shared dict instead of cloning per page (byte-identical) ([#504](https://github.com/matyushkin/djvu-rs/issues/504)) ([da9bbd5](https://github.com/matyushkin/djvu-rs/commit/da9bbd5e8e76eb5f177139f61ec49cb41dd95495))
* **jb2:** hash-index shared-symbol clustering — −81% (≈5.2×) on 517-page corpus ([#503](https://github.com/matyushkin/djvu-rs/issues/503)) ([f45a607](https://github.com/matyushkin/djvu-rs/commit/f45a607a0e37f31ecbc40567c5f337f5c254fc2e))
* **jb2:** JB2_AUTO_REC6 — density-probe auto-policy for same-size rec-6 ([#516](https://github.com/matyushkin/djvu-rs/issues/516)) ([33606db](https://github.com/matyushkin/djvu-rs/commit/33606dbed54cf92bb82e5a39944f16f148b2ae33))
* **jb2:** JB2_DESPECKLE — speck-removal pre-pass for lossy JB2 on noisy scans ([#521](https://github.com/matyushkin/djvu-rs/issues/521)) ([8aa8b2f](https://github.com/matyushkin/djvu-rs/commit/8aa8b2f0a653d9034ef585b392cfe51f821b5807))
* **jb2:** OCR_QA — OCR-agreement metric for lossy JB2 levers (round 41) ([#532](https://github.com/matyushkin/djvu-rs/issues/532)) ([353bc01](https://github.com/matyushkin/djvu-rs/commit/353bc01a773dd09f9a09df374489206a1c72c2a7))
* **jb2:** same-size rec-6 refinement — validated lossless −11.7% Sjbz on text ([#512](https://github.com/matyushkin/djvu-rs/issues/512)) ([25736a3](https://github.com/matyushkin/djvu-rs/commit/25736a3043e7df14af7f117d509e2d4a2b51512d))
* **mmap:** COLD_OPEN — cold-start harness + madvise (neutral) / prefetch (+24-90%) levers ([#529](https://github.com/matyushkin/djvu-rs/issues/529)) ([e237d50](https://github.com/matyushkin/djvu-rs/commit/e237d501dcd11463c603328651176411194f7df0))
* **pdf:** PDF_ADAPTIVE_RASTER — opt-in per-page Deflate-vs-JPEG choice ([#519](https://github.com/matyushkin/djvu-rs/issues/519)) ([47fec99](https://github.com/matyushkin/djvu-rs/commit/47fec996986c479aacce84e0f1d66603f1fc466a))
* **py:** PY_ZEROCOPY — GIL release + buffer-protocol zero-copy Pixmap in djvu-py ([#530](https://github.com/matyushkin/djvu-rs/issues/530)) ([bde8421](https://github.com/matyushkin/djvu-rs/commit/bde84217af65601c0ff8c095b6017ef619780eff))
* **quality:** QUALITY_COLOR — colour-aware D1 metric (Y/Cb/Cr SSIM + CIE76 ΔE) ([#525](https://github.com/matyushkin/djvu-rs/issues/525)) ([bd90536](https://github.com/matyushkin/djvu-rs/commit/bd90536fb7c01aaa677ce022b9434c8485a53de9))
* **render:** C5_COMPRESS — opt-in downgrade-on-evict render-cache tier (round 47) ([#537](https://github.com/matyushkin/djvu-rs/issues/537)) ([2ae8cf2](https://github.com/matyushkin/djvu-rs/commit/2ae8cf2cacc994e4c7a2a9fbdac9dd1cd09e2f63))
* **render:** D3_BICUBIC — FG44 bicubic upsampling measured, rejected ([#523](https://github.com/matyushkin/djvu-rs/issues/523)) ([37d42e6](https://github.com/matyushkin/djvu-rs/commit/37d42e6a9e1074e34df43c9f6930bb714da22361))
* **render:** Lanczos RGBA-interleaved accumulate — −22 to −27%, bit-identical ([#508](https://github.com/matyushkin/djvu-rs/issues/508)) ([95de0c4](https://github.com/matyushkin/djvu-rs/commit/95de0c4ab555078e2dad344580d3ce7db0dda3ce))
* **render:** VIEWER_BENCH + C4_TILE_CACHE — pan/zoom scenario bench justifies a composited-tile cache ([#527](https://github.com/matyushkin/djvu-rs/issues/527)) ([7304d9a](https://github.com/matyushkin/djvu-rs/commit/7304d9aeb4c63d2459b2cac1e4b6fdc910ad66d0))
* **zp:** ZP_U64 — u64 bit_buf widening investigated, reverted (round 54) ([#544](https://github.com/matyushkin/djvu-rs/issues/544)) ([f6e0f57](https://github.com/matyushkin/djvu-rs/commit/f6e0f571ff07fffbb189972b3c9a67237442362e))

## [0.24.4](https://github.com/matyushkin/djvu-rs/compare/v0.24.3...v0.24.4) (2026-07-03)


### Performance Improvements

* **build:** fat LTO + codegen-units=1 profile (−65% jb2 encode) ([#494](https://github.com/matyushkin/djvu-rs/issues/494)) ([6c6138a](https://github.com/matyushkin/djvu-rs/commit/6c6138a21334b60e29ead46a56cf4c4af40c02b0))
* **encode:** parallel per-page encoding in DJVM bundlers (−35…43% / −39%) ([#493](https://github.com/matyushkin/djvu-rs/issues/493)) ([a6d252a](https://github.com/matyushkin/djvu-rs/commit/a6d252a44f36abe6c4f1049fb4cd72e12f0dc199))
* parallelism sweep round 4 — clustering, EPUB/TIFF export, segmentation ([#497](https://github.com/matyushkin/djvu-rs/issues/497)) ([5deb1c2](https://github.com/matyushkin/djvu-rs/commit/5deb1c2ca3aa82cf6e10b7b721d833440b4822dd))
* rounds 5-6 decode/render/open sweep — lazy open (−48%), shared-dict (−37%), PGO (−15%) ([#498](https://github.com/matyushkin/djvu-rs/issues/498)) ([eb1fed4](https://github.com/matyushkin/djvu-rs/commit/eb1fed4a4e363df72c030df6a911159f4824c7fa))

## [0.24.3](https://github.com/matyushkin/djvu-rs/compare/v0.24.2...v0.24.3) (2026-07-02)


### Performance Improvements

* **encode:** row-slice foreground_fgbz blit-colour scan (~2% color encode) ([#490](https://github.com/matyushkin/djvu-rs/issues/490)) ([33ee923](https://github.com/matyushkin/djvu-rs/commit/33ee9230c317b0c9ff4ac9fd542f949e5d245a7c))

## [0.24.2](https://github.com/matyushkin/djvu-rs/compare/v0.24.1...v0.24.2) (2026-07-02)


### Performance Improvements

* **jb2:** byte-unpack extract_ccs mask expansion (~26% dict encode) ([#484](https://github.com/matyushkin/djvu-rs/issues/484)) ([f8a476f](https://github.com/matyushkin/djvu-rs/commit/f8a476f17a3a5cacae3bddb08e0ed84acd055d44))
* **segment:** row-slice mask fill + block-mean scan (~14% segment_page) ([#486](https://github.com/matyushkin/djvu-rs/issues/486)) ([b567a59](https://github.com/matyushkin/djvu-rs/commit/b567a5957273871377ed0e0f48629d23cd49d28a))

## [0.24.1](https://github.com/matyushkin/djvu-rs/compare/v0.24.0...v0.24.1) (2026-07-01)


### Performance Improvements

* **bzz:** MTF shift via copy_within memmove (~2% bzz_decode) ([#479](https://github.com/matyushkin/djvu-rs/issues/479)) ([b284d6b](https://github.com/matyushkin/djvu-rs/commit/b284d6bf3020d44ee4ad77f0b5121d178f8b16a5))
* **jb2:** byte-unpack bitmap expansion in encode_bitmap_direct (~20-26%) ([#481](https://github.com/matyushkin/djvu-rs/issues/481)) ([d5ab4ce](https://github.com/matyushkin/djvu-rs/commit/d5ab4ce5bd963d504a0cbc11eceae173d233b6b5))

## [0.24.0](https://github.com/matyushkin/djvu-rs/compare/v0.23.0...v0.24.0) (2026-07-01)


### Features

* **encode:** accept JPEG and TIFF inputs in addition to PNG ([#477](https://github.com/matyushkin/djvu-rs/issues/477)) ([9e1af97](https://github.com/matyushkin/djvu-rs/commit/9e1af97913f68fa3abd1a8827481e7c2b6c36d35))
* **encode:** generate TH44 per-page thumbnails during multi-page bundle encoding ([#476](https://github.com/matyushkin/djvu-rs/issues/476)) ([4b3486a](https://github.com/matyushkin/djvu-rs/commit/4b3486aaf4e1696b1cdbf5720e4ec14a5841de88))
* **iw44:** add Iw44Target::Bpp byte-budget encode-stopping criterion ([#475](https://github.com/matyushkin/djvu-rs/issues/475)) ([a1e3d54](https://github.com/matyushkin/djvu-rs/commit/a1e3d54b0462344772a115e5f05f20aca4ef48a5))

## [0.23.0](https://github.com/matyushkin/djvu-rs/compare/v0.22.5...v0.23.0) (2026-06-30)


### ⚠ BREAKING CHANGES

* **encode:** `encode_djvm_bundle_jb2` and `encode_djvm_bundle_jb2_with_shared` gain a trailing `dpi: u16` parameter. Pass `jb2_encode::BUNDLE_DEFAULT_DPI` to preserve prior intent. Bundled-mask DjVu output now carries the supplied dpi and gamma 2.2 (previously a hard-coded dpi 100 / gamma 0.1).

### Bug Fixes

* **encode:** address post-merge review of [#469](https://github.com/matyushkin/djvu-rs/issues/469)-[#472](https://github.com/matyushkin/djvu-rs/issues/472) ([#474](https://github.com/matyushkin/djvu-rs/issues/474)) ([3f510c3](https://github.com/matyushkin/djvu-rs/commit/3f510c3145d9c9845133ec0dd7f3023bab66f588))
* **encode:** single canonical INFO serializer across encode paths ([#470](https://github.com/matyushkin/djvu-rs/issues/470)) ([0b376cb](https://github.com/matyushkin/djvu-rs/commit/0b376cbe79f29a8e6ef878a553f2453e9f6d4e47))


### Performance Improvements

* **jb2:** hash-bucket dedup to drop per-CC bitmap clone (~2-3%, byte-identical) ([#467](https://github.com/matyushkin/djvu-rs/issues/467)) ([95bf134](https://github.com/matyushkin/djvu-rs/commit/95bf1345211c7424797deb3221e475ae8ab7b40e))

## [0.22.5](https://github.com/matyushkin/djvu-rs/compare/v0.22.4...v0.22.5) (2026-06-30)


### Performance Improvements

* **iw44:** default chroma_delay=10 (match c44; recover full-chroma cost) ([#465](https://github.com/matyushkin/djvu-rs/issues/465)) ([566e610](https://github.com/matyushkin/djvu-rs/commit/566e610fe45ce7b98c780aac468cf5f5ec5d5a9c))

## [0.22.4](https://github.com/matyushkin/djvu-rs/compare/v0.22.3...v0.22.4) (2026-06-30)


### Bug Fixes

* **iw44:** emit DjVuLibre-compatible IW44 major version (encoder interop) ([#462](https://github.com/matyushkin/djvu-rs/issues/462)) ([901a106](https://github.com/matyushkin/djvu-rs/commit/901a10681902803bbaa5581430e6f7e78ef97412))
* **iw44:** full-resolution chroma by default for DjVuLibre interop ([#464](https://github.com/matyushkin/djvu-rs/issues/464)) ([ed85953](https://github.com/matyushkin/djvu-rs/commit/ed859530a5839928187e8420dc587a82cfb23872))

## [0.22.3](https://github.com/matyushkin/djvu-rs/compare/v0.22.2...v0.22.3) (2026-06-30)


### Bug Fixes

* **jb2:** consume all comment-record octets to keep ZP coder aligned (correctness) ([#460](https://github.com/matyushkin/djvu-rs/issues/460)) ([c4f69a6](https://github.com/matyushkin/djvu-rs/commit/c4f69a68addb0e696a94b8d89ac38f49ad570e16))

## [0.22.2](https://github.com/matyushkin/djvu-rs/compare/v0.22.1...v0.22.2) (2026-06-30)


### Bug Fixes

* **decoders:** bound IFF/NAVM recursion + IFF length overflow (security review) ([#458](https://github.com/matyushkin/djvu-rs/issues/458)) ([3dc43f3](https://github.com/matyushkin/djvu-rs/commit/3dc43f3e42bf22a003d96ed773eb9eee348d29d9))
* **decoders:** cap allocations + fix overflow on untrusted input (security review) ([#459](https://github.com/matyushkin/djvu-rs/issues/459)) ([a034890](https://github.com/matyushkin/djvu-rs/commit/a03489051c7b296324ce31c5b422b746fd38dddd))
* **jb2:** enforce per-page symbol cap before decode (codex review) ([#456](https://github.com/matyushkin/djvu-rs/issues/456)) ([59896a1](https://github.com/matyushkin/djvu-rs/commit/59896a1b964114af6196114c95d98471cf7999c3))

## [0.22.1](https://github.com/matyushkin/djvu-rs/compare/v0.22.0...v0.22.1) (2026-06-26)


### Bug Fixes

* **jb2:** lower per-page symbol cap 32-&gt;16 MP to stop fuzz_jb2 flake ([c21bf06](https://github.com/matyushkin/djvu-rs/commit/c21bf062ea7ecd88a3a2cdebc10f2f24ae5c2df9))

## [0.22.0](https://github.com/matyushkin/djvu-rs/compare/v0.21.0...v0.22.0) (2026-06-25)


### Features

* **encode:** [#452](https://github.com/matyushkin/djvu-rs/issues/452) shared Djbz dictionary for layered multi-page encode ([3466abd](https://github.com/matyushkin/djvu-rs/commit/3466abd1af9150bb0cfd528ce26c666c5e960f2b))
* **iw44:** [#422](https://github.com/matyushkin/djvu-rs/issues/422) bilinear chroma upsampling for chroma_half pages ([0e96d56](https://github.com/matyushkin/djvu-rs/commit/0e96d56b18cc435fc8f41923d749d934b152ed34))


### Bug Fixes

* **jb2:** bound per-page symbol decode to stop fuzz_jb2 timeout (DoS) ([0710594](https://github.com/matyushkin/djvu-rs/commit/0710594deb7ad8a15493c15adac1b5ba8dd621d1))


### Performance Improvements

* **iw44:** match activation-prediction threshold to real gate (-9.1% BG44) ([20d1372](https://github.com/matyushkin/djvu-rs/commit/20d1372d97a2c41ab32ba18daa070293669ac59f))

## [0.21.0](https://github.com/matyushkin/djvu-rs/compare/v0.20.5...v0.21.0) (2026-06-24)


### Features

* **render:** [#439](https://github.com/matyushkin/djvu-rs/issues/439) anti-aliased color downscale via proportional fg/bg blend ([3e8c508](https://github.com/matyushkin/djvu-rs/commit/3e8c508f64fc895d9dbb76e3f77fd62e631f7c88))


### Bug Fixes

* **pixmap:** import vec! under no_std for scale_lanczos3 ([#448](https://github.com/matyushkin/djvu-rs/issues/448) follow-up) ([b58ae83](https://github.com/matyushkin/djvu-rs/commit/b58ae8341bd5c076f13b153948e728ad058b5ed9))


### Performance Improvements

* **jb2:** [#446](https://github.com/matyushkin/djvu-rs/issues/446) O(1) page-dedup in cluster_shared_symbols_tunable (O(P^2)-&gt;O(P)) ([0618bf7](https://github.com/matyushkin/djvu-rs/commit/0618bf7013d5164b79bf4e90dd1b7ac60f734290))
* **pdf:** [#449](https://github.com/matyushkin/djvu-rs/issues/449) stream page render-&gt;emit-&gt;drop in sequential PDF path ([fdb427b](https://github.com/matyushkin/djvu-rs/commit/fdb427b2f471a5e2145860c6cc6f961370537d4c))
* **render:** [#435](https://github.com/matyushkin/djvu-rs/issues/435) all-bg row fast path in B-series bilinear path (F2 analog) ([71aea00](https://github.com/matyushkin/djvu-rs/commit/71aea00a7be9ca46d1cbd2737d2cb684d127e24a))
* **render:** [#438](https://github.com/matyushkin/djvu-rs/issues/438) all-bg row fast path in area-average compositor (F2/I3 analog) ([93ab095](https://github.com/matyushkin/djvu-rs/commit/93ab095ddc21d683588c99ff9e6196cc1453e955))
* **render:** [#440](https://github.com/matyushkin/djvu-rs/issues/440) parallel BG44/FG44 layer decode via rayon::join (~22% cold) ([9c2d79c](https://github.com/matyushkin/djvu-rs/commit/9c2d79c489d16e9df8546074c72aea04c74ff124))
* **render:** [#443](https://github.com/matyushkin/djvu-rs/issues/443) extend F2 all-bg fast path to non-identity gamma ([7aa47f2](https://github.com/matyushkin/djvu-rs/commit/7aa47f2cc615f0586a4c21d47066a6175c91f5b4))
* **render:** [#447](https://github.com/matyushkin/djvu-rs/issues/447) 32x32 tiled transpose in rotate_pixmap Cw90/Ccw90 ([17727b0](https://github.com/matyushkin/djvu-rs/commit/17727b029953dacb3a7f38efe10a97b60878d704))
* **render:** [#448](https://github.com/matyushkin/djvu-rs/issues/448) hoist Lanczos vertical-pass weights + row-major accumulate (~22%) ([0c2c0c5](https://github.com/matyushkin/djvu-rs/commit/0c2c0c59259a086d28541cc1479b7026bad998f5))

## [0.20.5](https://github.com/matyushkin/djvu-rs/compare/v0.20.4...v0.20.5) (2026-06-24)


### Bug Fixes

* **render:** satisfy lint after downscale optimization ([819f4a3](https://github.com/matyushkin/djvu-rs/commit/819f4a3abeae1550c35317d4450df50d3b31f7c0))


### Performance Improvements

* **jb2:** [#429](https://github.com/matyushkin/djvu-rs/issues/429) byte-aligned crop_bitmap fast path in direct encoder (~14-15% multitile) ([a9946b0](https://github.com/matyushkin/djvu-rs/commit/a9946b0f8a0d313ab95149b955289c4d0df1dc2e))
* record F1 NEON color compositor fast path as Rejected ([79e9b6c](https://github.com/matyushkin/djvu-rs/commit/79e9b6c98a06d2b514227b441ced9f42c7f9ff15))
* **render:** [#426](https://github.com/matyushkin/djvu-rs/issues/426) cache decoded BG44 RGB Pixmap at subsample=2 (~7-9% downscale) ([08fb3e9](https://github.com/matyushkin/djvu-rs/commit/08fb3e99f868d7469b105fa401ab24d7ed3f1ac6))
* **render:** [#427](https://github.com/matyushkin/djvu-rs/issues/427) cache indexed JB2 mask + blit-map for FGbz-palette pages (~5%) ([2758b67](https://github.com/matyushkin/djvu-rs/commit/2758b671fffcd5a4d38afd4280027c0290b8179d))
* **render:** [#428](https://github.com/matyushkin/djvu-rs/issues/428) all-white band fast path in bilevel downscale compositor ([ca8c9dd](https://github.com/matyushkin/djvu-rs/commit/ca8c9dd2241470b7897fc909a43d7443eb17a0ed))
* **render:** [#433](https://github.com/matyushkin/djvu-rs/issues/433) generalize P2 BILEVEL_RGBA fast path to byte-aligned offset_x ([1f6bd49](https://github.com/matyushkin/djvu-rs/commit/1f6bd49e95a480f7cb15f93f0be6d6fdc806dbe2))
* **render:** byte-level POPCNT in mask_box_coverage (~24% at 72 DPI) ([4f3e902](https://github.com/matyushkin/djvu-rs/commit/4f3e9025865aacf9b6464f2e1fd3accb67a45c01))
* **render:** C3 mask-row hoist in general 1:1 bilinear path ([b4f77bb](https://github.com/matyushkin/djvu-rs/commit/b4f77bb831cf5c98bb04199a3e56e272fd9d24fd))
* **render:** cache BG44 decoded RGB Pixmap at sub=1 in PageLayers ([d24d83e](https://github.com/matyushkin/djvu-rs/commit/d24d83eb328df226656de6ec63f3a8eb7b98ad99))
* **render:** eliminate BG44 Pixmap clone via Cow&lt;'a, Pixmap&gt; ([9eb6c7b](https://github.com/matyushkin/djvu-rs/commit/9eb6c7b3e01f3543a1d694b8ed2e8f5b9fe5f9ad))
* **render:** eliminate FG44 + mask clones via Cow&lt;'a, …&gt; ([691b14c](https://github.com/matyushkin/djvu-rs/commit/691b14c109b023cbe552e20d3beb8cd557451a64))
* **render:** F2 all-bg row fast path in general 1:1 bilinear path ([195dfb1](https://github.com/matyushkin/djvu-rs/commit/195dfb103a6ce6ee9b9110780697f5176f04d578))
* **render:** G1 pre-expand mask row to bytes in general 1:1 bilinear path (~25%) ([84d4c18](https://github.com/matyushkin/djvu-rs/commit/84d4c187847717ac23cfc5ab02d582d2833f68a7))
* **render:** I3 all-white row fast path in bilevel 1:1 compositor ([53d6189](https://github.com/matyushkin/djvu-rs/commit/53d618980a67cb2e25fe59cf8bb7302c6f2782a2))
* **render:** inline alpha in bilinear compositor, remove fill_alpha_255 ([24461b3](https://github.com/matyushkin/djvu-rs/commit/24461b3d5e582f60de3b4d0fd09d791bb198e7fa))
* **render:** P2 BILEVEL_RGBA table for bilevel 1:1 compositor (~24%) ([57c8069](https://github.com/matyushkin/djvu-rs/commit/57c80693ce97b71d13829c3491d3cbded32a4614))
* **tiff:** [#430](https://github.com/matyushkin/djvu-rs/issues/430) Deflate-compressed bilevel TIFF export (~149x smaller) ([2d05853](https://github.com/matyushkin/djvu-rs/commit/2d058536e2c788c373db4039a1f8cf4aec1334a9))
* **tiff:** [#431](https://github.com/matyushkin/djvu-rs/issues/431) LUT byte-expansion in extract_bilevel_pixels (~18% bilevel export) ([908c2b5](https://github.com/matyushkin/djvu-rs/commit/908c2b5714edaccaa294b392d7ac3f02c2e14e6c))

## [0.20.4](https://github.com/matyushkin/djvu-rs/compare/v0.20.3...v0.20.4) (2026-06-21)


### Performance Improvements

* **render:** precompute area-average bounds and fuse downscale alpha writes (issue [#416](https://github.com/matyushkin/djvu-rs/issues/416))

## [0.20.3](https://github.com/matyushkin/djvu-rs/compare/v0.20.2...v0.20.3) (2026-06-18)


### Bug Fixes

* **render:** replace to_ne_bytes() with per-byte writes in bilevel compositor ([2ee62d4](https://github.com/matyushkin/djvu-rs/commit/2ee62d4f7d909e45821336ef04ba5c179d0a758e))
* **render:** use saturating_sub to avoid usize underflow in bilinear 1:1 path ([14de98c](https://github.com/matyushkin/djvu-rs/commit/14de98c0ce29a263658535f25e43433d87cebb82))
* resolve clippy warnings in WIP test additions ([a173164](https://github.com/matyushkin/djvu-rs/commit/a1731646671243b925455d6ef5398bd2e0ae5226))
* route raw IFF framing through iff::partial_emit in tests ([e718bd8](https://github.com/matyushkin/djvu-rs/commit/e718bd823c14f6f447e596a66738f89503239629))
* **text:** restore alloc String/ToString imports in no-std path ([1384ce4](https://github.com/matyushkin/djvu-rs/commit/1384ce4e9b04a5b587041a0d1ac4f07601b47dd5))


### Performance Improvements

* **render:** close 1.2–2.1× DjVuLibre gap (issue [#408](https://github.com/matyushkin/djvu-rs/issues/408)) ([ef7a676](https://github.com/matyushkin/djvu-rs/commit/ef7a6766ed5986bc8f19aa95c4635ba44b39396d))
* **render:** close 1.2–2.1× DjVuLibre gap (issue [#408](https://github.com/matyushkin/djvu-rs/issues/408)) ([5daf560](https://github.com/matyushkin/djvu-rs/commit/5daf56037126a00fe0e52aba43b11c4614534539))
* **render:** fix area-avg box bounds — −52% colorbook, −10% corpus color ([db84134](https://github.com/matyushkin/djvu-rs/commit/db841345542e736bf076a490cbe099e2085949e1))
* **render:** fix area-avg box bounds (inclusive→exclusive) + pow2 shift ([db84134](https://github.com/matyushkin/djvu-rs/commit/db841345542e736bf076a490cbe099e2085949e1))
* **render:** fix area-avg box bounds (inclusive→exclusive) + pow2 shift ([2e6e24d](https://github.com/matyushkin/djvu-rs/commit/2e6e24d3f181bb8cde29339eeeb70d74f89f17de))

## [0.20.2](https://github.com/matyushkin/djvu-rs/compare/v0.20.1...v0.20.2) (2026-06-17)


### Bug Fixes

* derive IW44 decode scale from native width, not display width ([5b111dc](https://github.com/matyushkin/djvu-rs/commit/5b111dc22c957e9eec5ef0ee06ac8341065829a6))

## [0.20.1](https://github.com/matyushkin/djvu-rs/compare/v0.20.0...v0.20.1) (2026-06-16)


### Bug Fixes

* **jb2:** accept valid pages whose large final symbol decodes at EOF ([2d55a7b](https://github.com/matyushkin/djvu-rs/commit/2d55a7b695f828db043f7f54787675e1b4afe2ef))
* **jb2:** accept valid pages whose large final symbol decodes at EOF ([c984927](https://github.com/matyushkin/djvu-rs/commit/c984927be605f5685e4a9b7f59835d128937e07e))

## [0.20.0](https://github.com/matyushkin/djvu-rs/compare/v0.19.1...v0.20.0) (2026-06-16)


### ⚠ BREAKING CHANGES

* **render:** `Page::render_aa` drops its ignored `_boldness` parameter, and six redundant `Page` render methods are deprecated in favor of `render_with` and `RenderOptions::fit_to_*`.

### Features

* **mut:** indirect DJVM rebundling + explicit external-file rewrite plan ([6df6e79](https://github.com/matyushkin/djvu-rs/commit/6df6e79a0252701e57c73f947008e83ef2e99b87))


### Bug Fixes

* **examples:** construct Jb2EncodeOptions with ..Default::default() ([cb864f7](https://github.com/matyushkin/djvu-rs/commit/cb864f786eb0d89d8733bcec50fc96e90caf81b4))
* **examples:** restore encode_quality_jb2 build after Jb2EncodeOptions gained a field ([7b4f8f2](https://github.com/matyushkin/djvu-rs/commit/7b4f8f2bfc62cc8adbec2957946a38cd3c430186))
* repair main CI — pyo3 advisory bump + broken jb2 example ([c1fc322](https://github.com/matyushkin/djvu-rs/commit/c1fc3229cb9d87b47c53e1c84895fcb385e19951))


### Code Refactoring

* **render:** shrink the Page render interface and seal internal duplication ([#346](https://github.com/matyushkin/djvu-rs/issues/346)) ([21fabd2](https://github.com/matyushkin/djvu-rs/commit/21fabd29ade6ff83830ed45ae226778902305507))

## [0.19.1](https://github.com/matyushkin/djvu-rs/compare/v0.19.0...v0.19.1) (2026-06-09)


### Bug Fixes

* **iw44:** decode all slices — drop ZP-exhaustion early-exit ([bd9b736](https://github.com/matyushkin/djvu-rs/commit/bd9b736dce1bdf0ef42030fefe62e2e79e8899d1))
* **iw44:** decode all slices — drop ZP-exhaustion early-exit in decode_chunk ([7365f26](https://github.com/matyushkin/djvu-rs/commit/7365f26227d2ecc7029b34f7168aaa30d834ad05))

## [0.19.0](https://github.com/matyushkin/djvu-rs/compare/v0.18.0...v0.19.0) (2026-05-17)


### Features

* **cli:** expose adaptive segmentation flags ([#317](https://github.com/matyushkin/djvu-rs/issues/317)) ([aee5450](https://github.com/matyushkin/djvu-rs/commit/aee5450690c224375f27d6376a5790e55c432890)), closes [#297](https://github.com/matyushkin/djvu-rs/issues/297)
* embed OCR text layers in DjVu output ([#315](https://github.com/matyushkin/djvu-rs/issues/315)) ([bfb9257](https://github.com/matyushkin/djvu-rs/commit/bfb9257b31d4eb4c70ebef0976d30333ecbeb73a))


### Performance Improvements

* **iw44:** record ARM64 NEON validation ([418d4fb](https://github.com/matyushkin/djvu-rs/commit/418d4fb06ac79518dc6a933c6386a9f02b3803fa))
* **iw44:** record rejected AVX2 row-pass spike ([5b6cdf3](https://github.com/matyushkin/djvu-rs/commit/5b6cdf38e6609949a74255604bb1e3a1dd08e289))
* **pdf:** record export memory baseline ([#318](https://github.com/matyushkin/djvu-rs/issues/318)) ([3b29f6b](https://github.com/matyushkin/djvu-rs/commit/3b29f6b119e363845d0d79ce2a8754f05b469277)), closes [#298](https://github.com/matyushkin/djvu-rs/issues/298)
* **pdf:** stream RGB staging for color pages ([#319](https://github.com/matyushkin/djvu-rs/issues/319)) ([445ed11](https://github.com/matyushkin/djvu-rs/commit/445ed11c2457bd563c10fd579052c93729b2d273)), closes [#299](https://github.com/matyushkin/djvu-rs/issues/299)

## [0.18.0](https://github.com/matyushkin/djvu-rs/compare/v0.17.0...v0.18.0) (2026-05-17)


### Features

* add archival FGbz scan profile ([#287](https://github.com/matyushkin/djvu-rs/issues/287)) ([6e0b850](https://github.com/matyushkin/djvu-rs/commit/6e0b8508689893e01124372ec6fac1dcfed423cd))
* emit per-blit FGbz color indices ([#291](https://github.com/matyushkin/djvu-rs/issues/291)) ([f9120c8](https://github.com/matyushkin/djvu-rs/commit/f9120c8cb132675b5dc0a45e1f378de8a718f076))
* **encode:** add adaptive segmentation options ([7b71793](https://github.com/matyushkin/djvu-rs/commit/7b71793bfa7c4378d22587da9fade166790979d4))
* **encode:** support layered directory bundles ([404054c](https://github.com/matyushkin/djvu-rs/commit/404054c4a848db1452f49d050492858da7b4be69))
* **ocr:** narrow experimental backends ([913e100](https://github.com/matyushkin/djvu-rs/commit/913e1001ac75078a49ee41e06e3b73cc5217d9f6))


### Bug Fixes

* **render:** reduce native colorbook diff ([63fe3dc](https://github.com/matyushkin/djvu-rs/commit/63fe3dc01064917d364647942d244d6773591c86))


### Performance Improvements

* add JB2 cross-size refinement probe ([#285](https://github.com/matyushkin/djvu-rs/issues/285)) ([a1805a9](https://github.com/matyushkin/djvu-rs/commit/a1805a990e71aa35a26f53c9ee87f1c932a9475d))
* **export:** stream TIFF color rendering ([4e3f955](https://github.com/matyushkin/djvu-rs/commit/4e3f9552010a13dd3aa4304a02367c10dda960cf))
* **render:** composite pixmap output directly ([39fbe27](https://github.com/matyushkin/djvu-rs/commit/39fbe27cd46792b7b69d4bfa808406c37bed3619))

## [0.17.0](https://github.com/matyushkin/djvu-rs/compare/v0.16.1...v0.17.0) (2026-05-04)


### Features

* add BZZ encoder with ZP arithmetic coding ([582432f](https://github.com/matyushkin/djvu-rs/commit/582432f601cf546c9404228af1541ec2067b503e))
* add fit_to_width/height/box to RenderOptions ([#33](https://github.com/matyushkin/djvu-rs/issues/33)) ([b371a93](https://github.com/matyushkin/djvu-rs/commit/b371a93099276cab573244cc262dc6ba093276cf))
* add NAVM bookmark encoder and ANTa/ANTz annotation encoder ([#133](https://github.com/matyushkin/djvu-rs/issues/133)) ([#136](https://github.com/matyushkin/djvu-rs/issues/136)) ([9e6bf66](https://github.com/matyushkin/djvu-rs/commit/9e6bf662125194fe57be02bed2ce42d87b8c16b7))
* **api:** bundled DJVM mutation + set_bookmarks (PR3 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#268](https://github.com/matyushkin/djvu-rs/issues/268)) ([6672a93](https://github.com/matyushkin/djvu-rs/commit/6672a936202f66b32e0e77dafdf467503547de6e))
* **api:** DjVuDocumentMut::from_bytes — chunk-replacement primitive (PR1 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#263](https://github.com/matyushkin/djvu-rs/issues/263)) ([b6279ae](https://github.com/matyushkin/djvu-rs/commit/b6279aed03de0eae42b1df7ee22da0bb56efaf08))
* **api:** high-level setters for DjVuDocumentMut (PR2 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#267](https://github.com/matyushkin/djvu-rs/issues/267)) ([eec0815](https://github.com/matyushkin/djvu-rs/commit/eec08153575052d81f71eb5382176816f1592aff))
* **api:** raw_chunk / all_chunks / chunk_ids on DjVuPage and DjVuDocument (Issue [#43](https://github.com/matyushkin/djvu-rs/issues/43)) ([#54](https://github.com/matyushkin/djvu-rs/issues/54)) ([3135627](https://github.com/matyushkin/djvu-rs/commit/31356279ad2739ae13ab36a144fbaabe5b5f63ab))
* **async:** add native lazy page loader ([735a226](https://github.com/matyushkin/djvu-rs/commit/735a22615ad91c89b2a38c865407a46319c46815))
* **async:** add wasm lazy reader entrypoint ([77fc6ff](https://github.com/matyushkin/djvu-rs/commit/77fc6ffecd57cb5760f9a7e75333987b3530c872))
* **async:** async render API via tokio::task::spawn_blocking (Issue [#51](https://github.com/matyushkin/djvu-rs/issues/51)) ([#61](https://github.com/matyushkin/djvu-rs/issues/61)) ([452636f](https://github.com/matyushkin/djvu-rs/commit/452636f143c56f4fc674968be5d5a4d8bd15d14a))
* **async:** load_document_async — buffered AsyncRead constructor ([#196](https://github.com/matyushkin/djvu-rs/issues/196) Phase 1) ([#231](https://github.com/matyushkin/djvu-rs/issues/231)) ([2d85e65](https://github.com/matyushkin/djvu-rs/commit/2d85e65e1963efb88bb86fe4b1b7e706228e910d))
* **async:** page_byte_range API + streaming async loader ([#196](https://github.com/matyushkin/djvu-rs/issues/196) Phase 2) ([#237](https://github.com/matyushkin/djvu-rs/issues/237)) ([a365abb](https://github.com/matyushkin/djvu-rs/commit/a365abb64b06a1c86de87e230623e05f465909e1))
* **async:** progressive stream render API (Issue [#81](https://github.com/matyushkin/djvu-rs/issues/81)) ([#112](https://github.com/matyushkin/djvu-rs/issues/112)) ([eaff91d](https://github.com/matyushkin/djvu-rs/commit/eaff91d5f5b8ccfa3cf86429339e7c21bcde5f72))
* **async:** resolve lazy shared dictionaries ([30d8ac9](https://github.com/matyushkin/djvu-rs/commit/30d8ac9f2a4be61c5e82b4bea5f1d8083d4bcd07))
* **bench:** add render_scaled and pdf_export benchmarks + BENCHMARKS.md (Issue [#52](https://github.com/matyushkin/djvu-rs/issues/52)) ([#62](https://github.com/matyushkin/djvu-rs/issues/62)) ([a8523c7](https://github.com/matyushkin/djvu-rs/commit/a8523c711c2fc3fc12d1089c5becf1b68af25595))
* **ci:** add ocr-tesseract integration test + CI job ([#178](https://github.com/matyushkin/djvu-rs/issues/178)) ([220671d](https://github.com/matyushkin/djvu-rs/commit/220671d0ae6d21f05a2a847945e062ae27eb931c))
* **ci:** continuous benchmark tracking — PR regression detection (Issue [#88](https://github.com/matyushkin/djvu-rs/issues/88)) ([#109](https://github.com/matyushkin/djvu-rs/issues/109)) ([0dfade5](https://github.com/matyushkin/djvu-rs/commit/0dfade5a8c4ec125bac50ff9deb5df37a159c56f))
* **cli:** implement djvu info/render/text — 24/24 tests green ([eb2e9d6](https://github.com/matyushkin/djvu-rs/commit/eb2e9d61f0a12285f9c6d7de4a4665b31f19a32a))
* **cli:** multi-page djvu encode from a directory of PNGs ([#223](https://github.com/matyushkin/djvu-rs/issues/223) follow-up) ([#245](https://github.com/matyushkin/djvu-rs/issues/245)) ([b8858bb](https://github.com/matyushkin/djvu-rs/commit/b8858bb36366be29441c17a06f0ea75daa4a210a))
* **cos-djvu:** benchmark suite, corpus infrastructure, BENCHMARKS.md (closes [#282](https://github.com/matyushkin/djvu-rs/issues/282)) ([#332](https://github.com/matyushkin/djvu-rs/issues/332)) ([50b6933](https://github.com/matyushkin/djvu-rs/commit/50b69330c73e5883a2954fe646f2cc7b7ec4e654))
* **cos-djvu:** phase 1 — IFF parser, typed errors, MIT skeleton (closes [#267](https://github.com/matyushkin/djvu-rs/issues/267)) ([#277](https://github.com/matyushkin/djvu-rs/issues/277)) ([1943f3f](https://github.com/matyushkin/djvu-rs/commit/1943f3f1cbef880038065ea377ccdfdff4ee33d0))
* **cos-djvu:** phase 2a — ZP arithmetic coder + BZZ decompressor (closes [#268](https://github.com/matyushkin/djvu-rs/issues/268)) ([#279](https://github.com/matyushkin/djvu-rs/issues/279)) ([4983056](https://github.com/matyushkin/djvu-rs/commit/498305678a0d13947e6fe5473d2bc6250383595d))
* **cos-djvu:** phase-5 rendering pipeline — compositing, gamma, scaling, AA (closes [#273](https://github.com/matyushkin/djvu-rs/issues/273)) ([7f5e161](https://github.com/matyushkin/djvu-rs/commit/7f5e161dd7f13c94ed1fb680966416e339eb80d5))
* **cos-djvu:** phase-6 quality — fuzz targets, benchmarks, no_std, full docs (closes [#274](https://github.com/matyushkin/djvu-rs/issues/274)) ([#324](https://github.com/matyushkin/djvu-rs/issues/324)) ([67fab7f](https://github.com/matyushkin/djvu-rs/commit/67fab7fdce945d972c574f61f9e460c32998a189))
* **cos-djvu:** text layer + annotations extraction (closes [#272](https://github.com/matyushkin/djvu-rs/issues/272)) ([#316](https://github.com/matyushkin/djvu-rs/issues/316)) ([5beeba9](https://github.com/matyushkin/djvu-rs/commit/5beeba96034a55396769d044ff1c20f20dc379a6))
* **djvm:** add merge and split commands for DjVu documents ([#126](https://github.com/matyushkin/djvu-rs/issues/126)) ([37eefd8](https://github.com/matyushkin/djvu-rs/commit/37eefd89f969ade36e3547fce0222a9d23d9c215)), closes [#76](https://github.com/matyushkin/djvu-rs/issues/76)
* djvu render --format pdf|cbz, roadmap v0.1 finalised ([7d823f6](https://github.com/matyushkin/djvu-rs/commit/7d823f634a1efbc75f743d5d5a8a4f6056e0b0a0))
* DjVu to PDF converter with text, bookmarks, and hyperlinks ([#2](https://github.com/matyushkin/djvu-rs/issues/2)-[#6](https://github.com/matyushkin/djvu-rs/issues/6)) ([#29](https://github.com/matyushkin/djvu-rs/issues/29)) ([a6f0a74](https://github.com/matyushkin/djvu-rs/commit/a6f0a7486d85146e677ae094396e96b44675e894))
* **djvu-enc:** high-level PageEncoder for bilevel Lossless ([#218](https://github.com/matyushkin/djvu-rs/issues/218)) ([#243](https://github.com/matyushkin/djvu-rs/issues/243)) ([afefcef](https://github.com/matyushkin/djvu-rs/commit/afefcef00cb2026772b4b22eb896c5533f87b844))
* **djvu-enc:** minimal layered Quality (segment → Sjbz + BG44) ([#246](https://github.com/matyushkin/djvu-rs/issues/246)) ([2febd04](https://github.com/matyushkin/djvu-rs/commit/2febd0486b839d800390e0abc1729965f02ad66c))
* document model — DjVuDocument, Page, DIRM, NAVM (closes [#271](https://github.com/matyushkin/djvu-rs/issues/271)) ([#283](https://github.com/matyushkin/djvu-rs/issues/283)) ([e36fd41](https://github.com/matyushkin/djvu-rs/commit/e36fd4169b591cb5f4146fc58bd3aaacf464d82b))
* **epub:** DPI-aware rendering, language tag, hyperlinks, cover image ([191e386](https://github.com/matyushkin/djvu-rs/commit/191e3869623a091998d75761f9be30d75c9576a8))
* **epub:** EPUB 3 export — page images, text overlay, navigation (Issue [#74](https://github.com/matyushkin/djvu-rs/issues/74)) ([9a6d155](https://github.com/matyushkin/djvu-rs/commit/9a6d1555c43041b86a783c5967a55607e0090768))
* **epub:** opt-in reflowable text section per page ([#228](https://github.com/matyushkin/djvu-rs/issues/228)) ([#240](https://github.com/matyushkin/djvu-rs/issues/240)) ([8207fb9](https://github.com/matyushkin/djvu-rs/commit/8207fb9e3046f594e5fb13150fa2d1220c5b35cf))
* **ffi:** add C FFI bindings via extern "C" functions ([#127](https://github.com/matyushkin/djvu-rs/issues/127)) ([2226bdf](https://github.com/matyushkin/djvu-rs/commit/2226bdfad34ef39a08bb9a3b1b220cac2942b30a)), closes [#72](https://github.com/matyushkin/djvu-rs/issues/72)
* **fgbz-enc:** FGbz foreground palette encoder ([#217](https://github.com/matyushkin/djvu-rs/issues/217)) ([#241](https://github.com/matyushkin/djvu-rs/issues/241)) ([e43bc9f](https://github.com/matyushkin/djvu-rs/commit/e43bc9f81ed3bc5e3cfaf5fa249c651749583a36))
* **fuzz:** add render to fuzz_full, add CI fuzz workflow (60 s/target) ([7e3e4eb](https://github.com/matyushkin/djvu-rs/commit/7e3e4eb0d51404d0c460b71ac5a359d4eac6da8b))
* hOCR and ALTO XML export for text layer (Issue [#75](https://github.com/matyushkin/djvu-rs/issues/75)) ([#98](https://github.com/matyushkin/djvu-rs/issues/98)) ([263cf14](https://github.com/matyushkin/djvu-rs/commit/263cf1492b57caa2b1d986a7eec8fd4a6cc8305b))
* implement ImageDecoder trait for image-rs integration (Issue [#80](https://github.com/matyushkin/djvu-rs/issues/80)) ([#97](https://github.com/matyushkin/djvu-rs/issues/97)) ([d7e4a64](https://github.com/matyushkin/djvu-rs/commit/d7e4a64e427454f4353f5124d1d03ba4ae31fe8c))
* import cos-djvu history, remove GPL legacy code ([0f33110](https://github.com/matyushkin/djvu-rs/commit/0f33110d1846e7114c6a46726c9e088cbef25bea))
* indirect DJVM — create_indirect() and parse_from_dir() ([#135](https://github.com/matyushkin/djvu-rs/issues/135)) ([#137](https://github.com/matyushkin/djvu-rs/issues/137)) ([d7fbf74](https://github.com/matyushkin/djvu-rs/commit/d7fbf74e45b9f5a85d0f162796709126131ab8a0))
* IW44 wavelet decoder with planar YCbCr (closes [#270](https://github.com/matyushkin/djvu-rs/issues/270)) ([#281](https://github.com/matyushkin/djvu-rs/issues/281)) ([f799e70](https://github.com/matyushkin/djvu-rs/commit/f799e7098dfbffdabada8e0c19fea2e31cdac351))
* IW44 wavelet encoder — BG44/FG44 chunk encoding (issue [#131](https://github.com/matyushkin/djvu-rs/issues/131)) ([#139](https://github.com/matyushkin/djvu-rs/issues/139)) ([10adc4f](https://github.com/matyushkin/djvu-rs/commit/10adc4f61bf8a79f9c4ba74559fb004b31d8968c))
* JB2 bilevel decoder (closes [#269](https://github.com/matyushkin/djvu-rs/issues/269)) ([#280](https://github.com/matyushkin/djvu-rs/issues/280)) ([e2a6898](https://github.com/matyushkin/djvu-rs/commit/e2a6898618765fe96a07ddaac51a40b665b64efb))
* JB2 bilevel image encoder — Sjbz chunk encoding (issue [#132](https://github.com/matyushkin/djvu-rs/issues/132)) ([#140](https://github.com/matyushkin/djvu-rs/issues/140)) ([cf280ba](https://github.com/matyushkin/djvu-rs/commit/cf280ba6e1356f1d8f7498c84c57e2759f603b3c))
* **jb2-enc:** expose tunable shared-Djbz clustering + corpus harness ([#194](https://github.com/matyushkin/djvu-rs/issues/194) Phase 2) ([#219](https://github.com/matyushkin/djvu-rs/issues/219)) ([9aafe42](https://github.com/matyushkin/djvu-rs/commit/9aafe42f04b6d474fefa0bbc73f06b4c666c9d1f))
* **jb2-enc:** multi-page shared Djbz dictionary in DJVM bundle, Phase 1 ([#194](https://github.com/matyushkin/djvu-rs/issues/194)) ([#216](https://github.com/matyushkin/djvu-rs/issues/216)) ([507b061](https://github.com/matyushkin/djvu-rs/commit/507b0618b27dc6619ce33736d01a9e920f2fdb32))
* **jb2-enc:** symbol-dictionary encoder Phases 1-3 ([#188](https://github.com/matyushkin/djvu-rs/issues/188)) ([#213](https://github.com/matyushkin/djvu-rs/issues/213)) ([8a2ed15](https://github.com/matyushkin/djvu-rs/commit/8a2ed150ad86bde2d0305c0b18e1297523590ce2))
* **jb2:** DJVI shared dictionary support via INCL chunks (Issue [#45](https://github.com/matyushkin/djvu-rs/issues/45)) ([#56](https://github.com/matyushkin/djvu-rs/issues/56)) ([86a63cb](https://github.com/matyushkin/djvu-rs/commit/86a63cb975303c14159340da4718ed3e23182e3e))
* mask and foreground/background layer extraction API ([#36](https://github.com/matyushkin/djvu-rs/issues/36)) ([d4c6527](https://github.com/matyushkin/djvu-rs/commit/d4c6527eb03aa9397a681ccdf03a27cf9ed77b0b))
* **mask:** wire Smmr (G4/MMR) decoder into both render pipelines ([07272b3](https://github.com/matyushkin/djvu-rs/commit/07272b32f44da9b4382bd9f1d4ff7828ce6223d3))
* **metadata:** METa/METz document metadata parsing (Issue [#44](https://github.com/matyushkin/djvu-rs/issues/44)) ([#55](https://github.com/matyushkin/djvu-rs/issues/55)) ([eb4515b](https://github.com/matyushkin/djvu-rs/commit/eb4515b49819c3b75824738989c07153a7c2c0d6))
* **mmap:** add memory-mapped I/O via MmapDocument ([387a2ea](https://github.com/matyushkin/djvu-rs/commit/387a2ea40696d88906dc5ead848154cf42189c6a)), closes [#70](https://github.com/matyushkin/djvu-rs/issues/70)
* **ocr:** pluggable OCR backend trait with Tesseract, ONNX, and Candle backends ([#125](https://github.com/matyushkin/djvu-rs/issues/125)) ([bf26603](https://github.com/matyushkin/djvu-rs/commit/bf26603b714d1721f63beedcf2ad54a5f95e21a0)), closes [#77](https://github.com/matyushkin/djvu-rs/issues/77)
* **pdf:** DCTDecode background encoding — smaller PDF output (Issue [#49](https://github.com/matyushkin/djvu-rs/issues/49)) ([#59](https://github.com/matyushkin/djvu-rs/issues/59)) ([de90a9f](https://github.com/matyushkin/djvu-rs/commit/de90a9fd94ef98fdeb8822aef174e945ebe5a3ea))
* **pdf:** parallel page rendering with rayon ([#148](https://github.com/matyushkin/djvu-rs/issues/148)) ([ae79a7c](https://github.com/matyushkin/djvu-rs/commit/ae79a7c6539fd2173885b18aa4e3beb7146b70b4))
* progressive DjVu rendering, multi-book cache, cos-diagnostics crate ([32432d8](https://github.com/matyushkin/djvu-rs/commit/32432d8c240f4a4673310a70158cf12cb9643635))
* **python:** add Python bindings via PyO3 ([#128](https://github.com/matyushkin/djvu-rs/issues/128)) ([e250fff](https://github.com/matyushkin/djvu-rs/commit/e250fffcef36531d8641e1ab8031198c30c7d372)), closes [#71](https://github.com/matyushkin/djvu-rs/issues/71)
* **render:** add rayon-based parallel page rendering ([3dc06f9](https://github.com/matyushkin/djvu-rs/commit/3dc06f991d7e26099ea4623580ebded7022f8775)), closes [#69](https://github.com/matyushkin/djvu-rs/issues/69)
* **render:** BGjp/FGjp JPEG background/foreground decoder (Issue [#47](https://github.com/matyushkin/djvu-rs/issues/47)) ([#57](https://github.com/matyushkin/djvu-rs/issues/57)) ([b65bd81](https://github.com/matyushkin/djvu-rs/commit/b65bd817270c21248aa3ec46b140b6fb97a9b683))
* **render:** grayscale output mode — GrayPixmap + render_gray8 ([c13ebb7](https://github.com/matyushkin/djvu-rs/commit/c13ebb75ffa0673e670530bdd7ebe53f311a5044))
* **render:** grayscale output mode — GrayPixmap + render_gray8 (Issue [#15](https://github.com/matyushkin/djvu-rs/issues/15)) ([75d7b37](https://github.com/matyushkin/djvu-rs/commit/75d7b37aed6ce0a6d4d118abc8050b731089a8bf))
* **render:** Lanczos-3 separable resampling (Issue [#50](https://github.com/matyushkin/djvu-rs/issues/50)) ([#60](https://github.com/matyushkin/djvu-rs/issues/60)) ([56817d1](https://github.com/matyushkin/djvu-rs/commit/56817d162a335fe9597d2e65f84e0778ee147c65))
* **render:** permissive render mode — skip corrupted chunks ([dc5734a](https://github.com/matyushkin/djvu-rs/commit/dc5734a088f82b6c5bf7b6cfc06a599a2f342a2b))
* **render:** permissive render mode — skip corrupted chunks (Issue [#19](https://github.com/matyushkin/djvu-rs/issues/19)) ([df5a8d7](https://github.com/matyushkin/djvu-rs/commit/df5a8d715ba8534856d50f570752b8722118c2a0))
* **render:** public render_streaming API (Phase 2 of [#225](https://github.com/matyushkin/djvu-rs/issues/225)) ([#260](https://github.com/matyushkin/djvu-rs/issues/260)) ([b92fac7](https://github.com/matyushkin/djvu-rs/commit/b92fac7c962c57115673eb31b6f84a5e7b36c085))
* **render:** zero-copy region render — render_region API (Issue [#86](https://github.com/matyushkin/djvu-rs/issues/86)) ([#111](https://github.com/matyushkin/djvu-rs/issues/111)) ([b2aa2a8](https://github.com/matyushkin/djvu-rs/commit/b2aa2a860dc28a63409785af8ef2be55cca40a11))
* **segment + cli:** FG/BG segmentation v1 + djvu encode subcommand ([#220](https://github.com/matyushkin/djvu-rs/issues/220), [#223](https://github.com/matyushkin/djvu-rs/issues/223)) ([#244](https://github.com/matyushkin/djvu-rs/issues/244)) ([4945b06](https://github.com/matyushkin/djvu-rs/commit/4945b0670938f4623e5368c14aa5ba41a0b107a5))
* serde support for metadata, annotations, bookmarks, and text zones (Issue [#82](https://github.com/matyushkin/djvu-rs/issues/82)) ([#96](https://github.com/matyushkin/djvu-rs/issues/96)) ([e872ecd](https://github.com/matyushkin/djvu-rs/commit/e872ecdefbee38e0862c899da3955c2ee98ca233))
* **smmr-enc:** public Smmr (G4/MMR) encoder API ([#221](https://github.com/matyushkin/djvu-rs/issues/221)) ([#242](https://github.com/matyushkin/djvu-rs/issues/242)) ([009c706](https://github.com/matyushkin/djvu-rs/commit/009c70681eb9c34992ed8a438343638a694cd98b))
* **smmr:** add G4/MMR bilevel image decoder (issue [#134](https://github.com/matyushkin/djvu-rs/issues/134)) ([#138](https://github.com/matyushkin/djvu-rs/issues/138)) ([8bd6e41](https://github.com/matyushkin/djvu-rs/commit/8bd6e4157c885e7a03c0443657d294099fbe7619))
* **text:** add reflowable_text() for paragraph reading-order extraction ([#228](https://github.com/matyushkin/djvu-rs/issues/228)) ([#239](https://github.com/matyushkin/djvu-rs/issues/239)) ([221a49a](https://github.com/matyushkin/djvu-rs/commit/221a49a1c066bdef59d5dcabc26961e8d6283752))
* **text:** TextLayer::transform — rotate + scale zone rects for rendered pages (Issue [#46](https://github.com/matyushkin/djvu-rs/issues/46)) ([#53](https://github.com/matyushkin/djvu-rs/issues/53)) ([c4a514e](https://github.com/matyushkin/djvu-rs/commit/c4a514e8bbdd4bccdb0032130b949580ef5306cc))
* **tiff:** embed DPI resolution tags in exported TIFF files ([87efb14](https://github.com/matyushkin/djvu-rs/commit/87efb140cc603a3ad4bd0f13b5e7a12c995ca516))
* **tiff:** TIFF export — multi-page color and bilevel modes (Issue [#48](https://github.com/matyushkin/djvu-rs/issues/48)) ([#58](https://github.com/matyushkin/djvu-rs/issues/58)) ([dc90cc0](https://github.com/matyushkin/djvu-rs/commit/dc90cc049ee7bd299ce6156c9c390c8616bdabca))
* transfer from cos-djvu, remove legacy GPL code, add PD corpus, benchmarks ([33fd496](https://github.com/matyushkin/djvu-rs/commit/33fd4969a691b5445c708b6c5f1ae2f877304f73))
* **ui:** table of contents navigation panel (closes [#60](https://github.com/matyushkin/djvu-rs/issues/60)) ([#298](https://github.com/matyushkin/djvu-rs/issues/298)) ([3fb0b2a](https://github.com/matyushkin/djvu-rs/commit/3fb0b2a94ff19891d6469f95aa381dcf66b7b712))
* user-controllable rotation in RenderOptions ([#35](https://github.com/matyushkin/djvu-rs/issues/35)) ([e0f79a8](https://github.com/matyushkin/djvu-rs/commit/e0f79a80bd3d3b2cc43933debb9a0b290aaea285))
* **wasm:** add WasmPage::text_zones_json() — text selection overlay API ([e513bbf](https://github.com/matyushkin/djvu-rs/commit/e513bbfc4052161f0b61f0c2cc429c58869d7170)), closes [#119](https://github.com/matyushkin/djvu-rs/issues/119)
* **wasm:** add WasmPage::text() — expose page text layer to JS ([35a776d](https://github.com/matyushkin/djvu-rs/commit/35a776d16804b39e1ae8cfddbf18073e4711204c))
* **wasm:** progressive IW44 render API ([#150](https://github.com/matyushkin/djvu-rs/issues/150)) ([35d3a30](https://github.com/matyushkin/djvu-rs/commit/35d3a30d285fd699781d9c4e5373b584b29f544d))
* **wasm:** WebAssembly bindings via wasm-bindgen (Issue [#73](https://github.com/matyushkin/djvu-rs/issues/73)) ([#118](https://github.com/matyushkin/djvu-rs/issues/118)) ([4300939](https://github.com/matyushkin/djvu-rs/commit/4300939b9085725005f7ce0a62d4e34f836367ff))


### Bug Fixes

* add missing chunk_data binding in iw44_new doctest ([d1a210b](https://github.com/matyushkin/djvu-rs/commit/d1a210b8b61fabc5c7634282f0bfb8062503dfa3))
* **annotation:** add MAX_SEXPR_DEPTH=64 guard to prevent stack overflow on deeply nested S-expressions ([#200](https://github.com/matyushkin/djvu-rs/issues/200)) ([2f03789](https://github.com/matyushkin/djvu-rs/commit/2f03789638a65f9ac2e6c7cf5763f63ec9a3a0f6))
* apply gamma correction in all legacy render paths ([#9](https://github.com/matyushkin/djvu-rs/issues/9)) ([#22](https://github.com/matyushkin/djvu-rs/issues/22)) ([dfba614](https://github.com/matyushkin/djvu-rs/commit/dfba614c510d8da150ee76cf62f629f89cff48bc))
* apply page rotation from INFO chunk in render_pixmap and render_coarse ([#10](https://github.com/matyushkin/djvu-rs/issues/10)) ([#24](https://github.com/matyushkin/djvu-rs/issues/24)) ([adec5ee](https://github.com/matyushkin/djvu-rs/commit/adec5eed27bf5f096bad4187a2d88a5349bd07a4))
* **ci:** enable cli feature for nextest to build djvu binary ([576f12f](https://github.com/matyushkin/djvu-rs/commit/576f12faf154662a3992ac3a20c3c126e32fc798))
* **ci:** exclude djvu-py from nextest, fix audit advisories ([788b0d3](https://github.com/matyushkin/djvu-rs/commit/788b0d39fc04d1ed28dc5052f2bc88004f106117))
* **ci:** exclude fuzz/ from workspace to fix cargo-fuzz builds ([330ff0a](https://github.com/matyushkin/djvu-rs/commit/330ff0a28a601629718cf0c12d4e90ab93524621))
* **ci:** IJG license allowlist, no_std BTreeMap, clippy errors ([6a2a391](https://github.com/matyushkin/djvu-rs/commit/6a2a39199dffd1aa60cbe916bd2b28a516185dcd))
* **ci:** install libleptonica-dev + libtesseract-dev for ocr-tesseract job ([0410f70](https://github.com/matyushkin/djvu-rs/commit/0410f70f0ed73be0425906685c3af1e0b6397374))
* **ci:** split test into test-stable + test-beta jobs (matrix.rust not allowed in job if) ([0d60c76](https://github.com/matyushkin/djvu-rs/commit/0d60c7662a78bff90dad81d9847e07d93dfb3b15))
* **ci:** use core::mem::take in no_std context; fix clippy redundant-Some in ocr_export test ([#101](https://github.com/matyushkin/djvu-rs/issues/101)) ([cc1cdf1](https://github.com/matyushkin/djvu-rs/commit/cc1cdf136e4eaaf9311170a20cd1f3b5ffd4ce54))
* clippy errors and fmt — let-chain, ref on let, line wrapping ([b4ba2f8](https://github.com/matyushkin/djvu-rs/commit/b4ba2f87acc234e7b7467f394169775b846c03b5))
* **clippy:** use `contains()` instead of `iter().any()` in tiff_export ([f9b4c2b](https://github.com/matyushkin/djvu-rs/commit/f9b4c2b7b9ad3aadbc9539aec09ef651f5545312))
* **docs:** resolve all remaining broken intra-doc links (0 warnings) ([4ac8bb1](https://github.com/matyushkin/djvu-rs/commit/4ac8bb162d922529ac4e58757155d556fbe3e7fc))
* **docs:** resolve broken intra-doc links in djvu_document ([8dbfda0](https://github.com/matyushkin/djvu-rs/commit/8dbfda050a72df34dbe3f149427aec4a6e4252a9))
* eliminate memory leaks and add OOM protection ([25f041d](https://github.com/matyushkin/djvu-rs/commit/25f041d78c513f1e3dfbd87aa69dc4fc261488da))
* exclude .cargo/config.toml from published package (fixes docs.rs build) ([b9dd0da](https://github.com/matyushkin/djvu-rs/commit/b9dd0dafac62f0d6e12064f431d6b962b6a93e08))
* FGbz multi-color foreground palette — use per-glyph blit index ([#12](https://github.com/matyushkin/djvu-rs/issues/12)) ([#26](https://github.com/matyushkin/djvu-rs/issues/26)) ([7897164](https://github.com/matyushkin/djvu-rs/commit/789716408799e39c0e78e3caeedb051f913d26ac))
* **fuzz:** add [workspace] to fuzz/Cargo.toml to fix cargo-fuzz build ([be7a5a8](https://github.com/matyushkin/djvu-rs/commit/be7a5a80d23ca77c6c326c1c9065e1498ab41b5d))
* **fuzz:** revert to cargo install cargo-fuzz, add explicit binary cache ([84e6eef](https://github.com/matyushkin/djvu-rs/commit/84e6eef4b18c6f957f327d34fc2f87ab560a5f3c))
* **fuzz:** use correct public module djvu_rs::jb2 in fuzz_jb2 target ([9133de6](https://github.com/matyushkin/djvu-rs/commit/9133de69f3af8ee05f2c8f9239efcf87e2ed0a76))
* **hard-rule:** eliminate last 5 .expect()/.unwrap() in production code (Issue [#443](https://github.com/matyushkin/djvu-rs/issues/443)) ([#444](https://github.com/matyushkin/djvu-rs/issues/444)) ([e4247ea](https://github.com/matyushkin/djvu-rs/commit/e4247eafc1eabadfac42fb04c318252ac0ebcc71))
* **iff:** preserve FORM length parity for byte-identical mutation (PR4 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#269](https://github.com/matyushkin/djvu-rs/issues/269)) ([df1e95e](https://github.com/matyushkin/djvu-rs/commit/df1e95e51c08ebe4e85cb1899352aae0cf5a5692))
* **iw44:** correct vext lane in prelim_flags_band0_neon horizontal-OR ([#266](https://github.com/matyushkin/djvu-rs/issues/266)) ([b390681](https://github.com/matyushkin/djvu-rs/commit/b3906813dbe4d4c9946c93b9b9e6884c1da62efc))
* **jb2_encode:** return empty Vec for zero-dimension bitmaps; unreachable dead branch ([#142](https://github.com/matyushkin/djvu-rs/issues/142)) ([#143](https://github.com/matyushkin/djvu-rs/issues/143)) ([091b657](https://github.com/matyushkin/djvu-rs/commit/091b6573467c67c3c8d487ee82dba24b7c59f136))
* **jb2-enc:** cap cluster_shared_symbols pixel budget at decoder limit ([#270](https://github.com/matyushkin/djvu-rs/issues/270)) ([#271](https://github.com/matyushkin/djvu-rs/issues/271)) ([78e5c79](https://github.com/matyushkin/djvu-rs/commit/78e5c793d3d7a86512e42820d8e3cf0c9870c48e))
* **jb2-enc:** tile direct encoder to ≤1MP records ([#198](https://github.com/matyushkin/djvu-rs/issues/198)) ([#214](https://github.com/matyushkin/djvu-rs/issues/214)) ([24ac60f](https://github.com/matyushkin/djvu-rs/commit/24ac60fce1c139a8b55a3d30e71a7c36cac5d6d8))
* **jb2,iw44:** cap comment bytes and IW44 pixel limit to prevent fuzz timeouts ([49c7b1b](https://github.com/matyushkin/djvu-rs/commit/49c7b1bd29505528d53b70e7ab820e5a9c0eae2e))
* **jb2,iw44:** prevent DoS via refinement bitmaps and uncapped total pixel budget ([3e72cf6](https://github.com/matyushkin/djvu-rs/commit/3e72cf6c33d21661db1198013d789217d8978580))
* **jb2:** add blit-pixel budget to prevent type-7 dict-copy DoS ([1c03505](https://github.com/matyushkin/djvu-rs/commit/1c0350543b870c3b29fe87c1b59370410ba5e464))
* **jb2:** cap decode loop at 1 M records to prevent infinite spin on exhausted ZP input ([0b84f2d](https://github.com/matyushkin/djvu-rs/commit/0b84f2dcadaf4cbbae75bd2e53cb82f7f99d4f2d))
* **jb2:** correct regression test comment for fuzz2 fix ([ee380ae](https://github.com/matyushkin/djvu-rs/commit/ee380ae87d3f589627182b5a1350ae76072eb901))
* **jb2:** guard blit against negative symbol dimensions ([49a3792](https://github.com/matyushkin/djvu-rs/commit/49a3792c764e25594165a10b127612a446d7a732))
* **jb2:** guard blit fast path against i32 overflow and data buffer overread ([be72d29](https://github.com/matyushkin/djvu-rs/commit/be72d29584170afb0f12a951c10d3f24859ae02f))
* **jb2:** limit symbol bitmap size to 4 MP to prevent DoS via crafted input ([943f25e](https://github.com/matyushkin/djvu-rs/commit/943f25eaacc7c91234caea2e99ff87950f7d632f))
* **jb2:** prevent infinite loop in decode_num on corrupt streams ([d7bee1e](https://github.com/matyushkin/djvu-rs/commit/d7bee1e8c8c612583a055d40c4aa7a8c81666e47)), closes [#122](https://github.com/matyushkin/djvu-rs/issues/122)
* **jb2:** reduce MAX_RECORDS and MAX_SYMBOL_PIXELS to prevent fuzz timeouts ([3292193](https://github.com/matyushkin/djvu-rs/commit/3292193237d733976ea24c879ade958b1021caaa))
* **ocr:** resolve all clippy errors in ocr_neural, ocr_onnx, ocr_tesseract ([65a0cee](https://github.com/matyushkin/djvu-rs/commit/65a0cee338411a608c0c8a92d02c3638cb18c153))
* **pdf:** Unicode-aware glyph width for invisible text layer ([2398c38](https://github.com/matyushkin/djvu-rs/commit/2398c3851d8cef9c5a2b41dbcdaf5352baf9575a))
* remove deprecated [[licenses.deny]] syntax from deny.toml (cargo-deny v2) ([7971e44](https://github.com/matyushkin/djvu-rs/commit/7971e448cdeb82d8be78910e9f275ecc1b975d65))
* **render:** add #[allow(unsafe_code)] + unsafe blocks for Rust 2024 SIMD ([#169](https://github.com/matyushkin/djvu-rs/issues/169)) ([179d171](https://github.com/matyushkin/djvu-rs/commit/179d17132c31b5ec31eaa99156acc4ac4cdda1f6))
* **render:** correct gamma LUT formula to match DjVuLibre ([#161](https://github.com/matyushkin/djvu-rs/issues/161)) ([cd3228d](https://github.com/matyushkin/djvu-rs/commit/cd3228d812e77f6d9bdcc48e1d9a92f8ff34077e))
* **render:** resolve unsafe_code / unsafe_op_in_unsafe_fn CI conflicts ([889f28f](https://github.com/matyushkin/djvu-rs/commit/889f28fb67c5b13c8f5b4a66ae32b1546a498988))
* **render:** restore bilevel composite fast path, recover 2× regression from [#165](https://github.com/matyushkin/djvu-rs/issues/165) ([46b6931](https://github.com/matyushkin/djvu-rs/commit/46b69318d0e1181d4eec5d9fc428cc5eee9723f6))
* **render:** scale page-space coords into FG44 + BG plane space ([#199](https://github.com/matyushkin/djvu-rs/issues/199)) ([#248](https://github.com/matyushkin/djvu-rs/issues/248)) ([e2c07df](https://github.com/matyushkin/djvu-rs/commit/e2c07df45d5cfbc1edbef07553549e4fbf0fd2fc))
* **render:** use core::arch instead of std::arch for no_std compatibility ([122b989](https://github.com/matyushkin/djvu-rs/commit/122b989708e475658a764b2b145a887605590e37))
* replace all internal cos-djvu/cos_djvu references with djvu-rs/djvu_rs ([03fb17a](https://github.com/matyushkin/djvu-rs/commit/03fb17ae4460ca6baff50235993c6f005ddc08e4))
* resolve issues [#164](https://github.com/matyushkin/djvu-rs/issues/164) [#169](https://github.com/matyushkin/djvu-rs/issues/169) [#170](https://github.com/matyushkin/djvu-rs/issues/170) [#174](https://github.com/matyushkin/djvu-rs/issues/174) [#176](https://github.com/matyushkin/djvu-rs/issues/176) [#177](https://github.com/matyushkin/djvu-rs/issues/177) ([774cbdb](https://github.com/matyushkin/djvu-rs/commit/774cbdb834c703c27ca82ed9f8f3bb4a17504aeb))
* **smmr:** replace manual div_ceil with .div_ceil() per clippy ([a8d24ca](https://github.com/matyushkin/djvu-rs/commit/a8d24ca5fa470e9f9c59bee7ef7a605786a8fe79))
* update MSRV to 1.88 (let-chains stabilized in 1.88) ([8d5b94f](https://github.com/matyushkin/djvu-rs/commit/8d5b94f9d84c9065662e2464003aa75a95613421))
* update tesseract API and optimize JB2 inner loop ([f64d884](https://github.com/matyushkin/djvu-rs/commit/f64d88423aff0d4861a6dd50c1ec7a4c45e1d10b))
* vendor djvu-rs into crates/cos-djvu and fix production panics ([5f6d7fe](https://github.com/matyushkin/djvu-rs/commit/5f6d7fecadb7ea3bdcfa5cd530215ff28ea5e133)), closes [#4](https://github.com/matyushkin/djvu-rs/issues/4)
* wasm CI job, open_dir API, DPI scaling in OCR export, jb2_new cleanup ([6cbe038](https://github.com/matyushkin/djvu-rs/commit/6cbe038909858f4c523f13d7204998a7109cd84d))
* **wasm:** correct render() pixel layout and Uint8ClampedArray allocation ([1a967d2](https://github.com/matyushkin/djvu-rs/commit/1a967d2003bb74f0e42bccc8f108b2f1303617c1))
* **zp:** widen a/c/fence fields from u16 to u32 to match jb2 inline decoder ([fb0db12](https://github.com/matyushkin/djvu-rs/commit/fb0db122daa4dccd97f49a971103de0add5a2366))


### Performance Improvements

* allow 1.5× upscale in IW44 subsample selection for faster downscaled renders ([8f0baa2](https://github.com/matyushkin/djvu-rs/commit/8f0baa2a06eadce23974d973b7ee635b5939a5f0))
* area-averaging downscale for better quality when rendering at reduced size ([#13](https://github.com/matyushkin/djvu-rs/issues/13)) ([#28](https://github.com/matyushkin/djvu-rs/issues/28)) ([b822ded](https://github.com/matyushkin/djvu-rs/commit/b822ded71372b0f70071f8245768d09b89e62a17))
* **bitmap:** packed bitwise dilation with ping-pong buffers ([2887689](https://github.com/matyushkin/djvu-rs/commit/288768968e910eccfe313785aa316ef8eb0fbdac))
* **bitmap:** packed bitwise dilation with ping-pong buffers (Issue [#17](https://github.com/matyushkin/djvu-rs/issues/17)) ([0814f55](https://github.com/matyushkin/djvu-rs/commit/0814f5519475073c4b426ca91360491c8ad69830))
* **bzz:** inline ZP state locals in MTF decode hot loop ([bad5b21](https://github.com/matyushkin/djvu-rs/commit/bad5b215bfa5384589f2252b3da2ee0ecb9a3ac2))
* **bzz:** parallel inverse-BWT via rayon (Issue [#89](https://github.com/matyushkin/djvu-rs/issues/89)) ([#110](https://github.com/matyushkin/djvu-rs/issues/110)) ([eb5bab0](https://github.com/matyushkin/djvu-rs/commit/eb5bab0024029e494a7ab069db46425e8e75b2f9))
* **ci:** single nextest pass on main avoids sequential overhead ([f17d901](https://github.com/matyushkin/djvu-rs/commit/f17d901b1055c3cc2fb8e6dbfa3b6d445b6aa357))
* **deps:** split ocr-neural into lightweight stub + ocr-neural-candle ([#175](https://github.com/matyushkin/djvu-rs/issues/175)) ([3a930b2](https://github.com/matyushkin/djvu-rs/commit/3a930b2e38ca939eb15af2eefbb55156a158224b))
* downsampled mask pyramid for composite — 8 ms vs 23 ms for 150 dpi renders ([1374f27](https://github.com/matyushkin/djvu-rs/commit/1374f2707ead48b1db2a0eea48c12c8872bf9527))
* eliminate bounds checks in JB2 hot loops and ZP renormalize ([7e94000](https://github.com/matyushkin/djvu-rs/commit/7e94000ace8ed67b5410586563e7168031fa595e))
* eliminate redundant mask sampling in 3-layer composite ([#14](https://github.com/matyushkin/djvu-rs/issues/14)) ([#27](https://github.com/matyushkin/djvu-rs/issues/27)) ([f601036](https://github.com/matyushkin/djvu-rs/commit/f601036e8988c2ea0c4f2a5742cd4b1321bc8065))
* **iw44-enc:** NEON forward wavelet + parallel Y/Cb/Cr + skip empty passes ([#206](https://github.com/matyushkin/djvu-rs/issues/206)) ([d1f2765](https://github.com/matyushkin/djvu-rs/commit/d1f27658d9eecfcd741a63788a56be7a891e8ed0))
* **iw44:** allocate chroma planes at half resolution when chroma_half=true (Issue [#85](https://github.com/matyushkin/djvu-rs/issues/85)) ([#99](https://github.com/matyushkin/djvu-rs/issues/99)) ([927e7c0](https://github.com/matyushkin/djvu-rs/commit/927e7c01580412e45a1487ddb98d62b524eed059))
* **iw44:** compact-plane wavelet for sub≥2 + correct start_scale (Issue [#115](https://github.com/matyushkin/djvu-rs/issues/115)) ([#116](https://github.com/matyushkin/djvu-rs/issues/116)) ([4fc8921](https://github.com/matyushkin/djvu-rs/commit/4fc8921a7892834b3be97ed952d30000ea467b21))
* **iw44:** NEON decoder — column pass s=2/4, scatter, YCbCr, lifting (~25% to_rgb) ([#207](https://github.com/matyushkin/djvu-rs/issues/207)) ([f570297](https://github.com/matyushkin/djvu-rs/commit/f5702973fd0833ed195554335853325ece4e5f12))
* **iw44:** SIMD row pass — 8 rows at a time with i32x8 ([#107](https://github.com/matyushkin/djvu-rs/issues/107)) ([1418ff4](https://github.com/matyushkin/djvu-rs/commit/1418ff4f3c2cdfd1e0ee7a1210bc3852d0240239))
* **iw44:** SIMD YCbCr→RGB using wide::i32x8 (Issue [#1](https://github.com/matyushkin/djvu-rs/issues/1)) ([#64](https://github.com/matyushkin/djvu-rs/issues/64)) ([abceef4](https://github.com/matyushkin/djvu-rs/commit/abceef47d2bca524a20d13f4926f4dbf84e3c79b))
* **iw44:** SIMD-accelerate inverse wavelet transform column pass ([2ac4318](https://github.com/matyushkin/djvu-rs/commit/2ac4318c2c6e56af93ad5fe52670e032a70d378f)), closes [#68](https://github.com/matyushkin/djvu-rs/issues/68)
* **iw44:** WASM simd128 ycbcr_raw kernels (Phase 1 of [#190](https://github.com/matyushkin/djvu-rs/issues/190)) ([#253](https://github.com/matyushkin/djvu-rs/issues/253)) ([d59fcee](https://github.com/matyushkin/djvu-rs/commit/d59fcee1c770011a726ee41d77cf8964629dbc94))
* **iw44:** x86_64 AVX2 ports of prelim_flags kernels (Phase 3 of [#189](https://github.com/matyushkin/djvu-rs/issues/189)) ([#261](https://github.com/matyushkin/djvu-rs/issues/261)) ([0ed7a36](https://github.com/matyushkin/djvu-rs/commit/0ed7a36e3c117d83c9bf9dde6df5bd347a2652cf))
* **iw44:** x86_64 AVX2 ycbcr_raw kernels (Phase 1 of [#189](https://github.com/matyushkin/djvu-rs/issues/189)) ([#251](https://github.com/matyushkin/djvu-rs/issues/251)) ([7f3d867](https://github.com/matyushkin/djvu-rs/commit/7f3d8679e68f7c1daa8d7973e82d475ab3974c47))
* **jb2-enc:** eliminate bounds checks from JB2 encode hot loop ([#205](https://github.com/matyushkin/djvu-rs/issues/205)) ([5cbeaf0](https://github.com/matyushkin/djvu-rs/commit/5cbeaf0f64afbb2d39b86f568c73d04df9c9d35a))
* **jb2-enc:** opt-in lossy rec-7 near-duplicate substitution (Phase 4 of [#224](https://github.com/matyushkin/djvu-rs/issues/224)) ([#256](https://github.com/matyushkin/djvu-rs/issues/256)) ([98fb8c7](https://github.com/matyushkin/djvu-rs/commit/98fb8c76c22a3eb4a7306585f030605a0941673b))
* **jb2-enc:** per-CC accounting harness for shared-Djbz ([#194](https://github.com/matyushkin/djvu-rs/issues/194) Phase 2.5) ([#255](https://github.com/matyushkin/djvu-rs/issues/255)) ([f09bfdf](https://github.com/matyushkin/djvu-rs/commit/f09bfdf71ff0ce9959baa67aedb0c2fe73535f99))
* **jb2:** bit-pack Jbm to 1 bit/pixel — 8x memory, corpus −3.9% ([#187](https://github.com/matyushkin/djvu-rs/issues/187)) ([17f331f](https://github.com/matyushkin/djvu-rs/commit/17f331f3d44e9e8b6f2d179b77e7212ba5b47433))
* **jb2:** close performance gap vs DjVuLibre + CLI improvements ([#159](https://github.com/matyushkin/djvu-rs/issues/159)) ([3efc430](https://github.com/matyushkin/djvu-rs/commit/3efc430234f8848ecc597302c7f6ece8ae9ac887))
* **jb2:** local-copy ZP state for register-allocation + hardware CLZ ([0590d3c](https://github.com/matyushkin/djvu-rs/commit/0590d3cd00dce042132d8759204421a7b1750f33))
* **jb2:** reuse scratch buffer across symbol decodes to eliminate per-symbol heap allocations (Issue [#90](https://github.com/matyushkin/djvu-rs/issues/90)) ([#100](https://github.com/matyushkin/djvu-rs/issues/100)) ([12575d0](https://github.com/matyushkin/djvu-rs/commit/12575d095e73e650a5b04f4a5a56624b15c541f9))
* **jb2:** shared dict cache + split_at_mut inner loop (Issue [#87](https://github.com/matyushkin/djvu-rs/issues/87)) ([#106](https://github.com/matyushkin/djvu-rs/issues/106)) ([08ca0f4](https://github.com/matyushkin/djvu-rs/commit/08ca0f43f557024411e765afd9f419c2681f6275))
* partial BG44 chunk decode for sub=4 renders — skip high-frequency refinement ([b371e4e](https://github.com/matyushkin/djvu-rs/commit/b371e4eebe5cd766114d81dac3480430bc8712ed))
* **pdf:** output_dpi option + bilevel fast path — 2× faster export ([cfccbc6](https://github.com/matyushkin/djvu-rs/commit/cfccbc6deefbf288bcd2055ee27c3fed5d7c9b54)), closes [#147](https://github.com/matyushkin/djvu-rs/issues/147)
* **render:** 66% speedup on 600 dpi bilevel pages (Issue [#104](https://github.com/matyushkin/djvu-rs/issues/104)) ([#105](https://github.com/matyushkin/djvu-rs/issues/105)) ([8e5a2f4](https://github.com/matyushkin/djvu-rs/commit/8e5a2f42fdb0872b5a13d2560330346b0dc09989))
* **render:** eliminate redundant mask sampling in 3-layer composite (Issue [#14](https://github.com/matyushkin/djvu-rs/issues/14)) ([#37](https://github.com/matyushkin/djvu-rs/issues/37)) ([585991d](https://github.com/matyushkin/djvu-rs/commit/585991d961978277b8fdaf2b87236f6d1b825ac0))
* **render:** internal row-streaming refactor (Phase 1 of [#225](https://github.com/matyushkin/djvu-rs/issues/225)) ([#259](https://github.com/matyushkin/djvu-rs/issues/259)) ([3d22a59](https://github.com/matyushkin/djvu-rs/commit/3d22a59ae90ccff0cfcc0b4eb3fb21c7bb693dc6))
* **render:** NEON bilinear vertical pass + 4-byte RGBX stride ([#93](https://github.com/matyushkin/djvu-rs/issues/93)) ([b0dfdb8](https://github.com/matyushkin/djvu-rs/commit/b0dfdb8de78e34f5f7478bc06bf3c3dfad21d1df))
* **render:** precomputed coord tables, zero-copy BG path, remove PageMapper ([cf1a8e9](https://github.com/matyushkin/djvu-rs/commit/cf1a8e99eebf86d47c35fa403603354cbd23a5d7))
* **render:** x86_64 SSE2/SSSE3 fast paths for alpha fill and RGB→RGBA ([#169](https://github.com/matyushkin/djvu-rs/issues/169)) ([0ea3f3e](https://github.com/matyushkin/djvu-rs/commit/0ea3f3e674ae1eb112fb73368038e3fd0b2e0dde))
* replace bg_subsample division with shift in composite hot path ([0e0f2a3](https://github.com/matyushkin/djvu-rs/commit/0e0f2a3aa52da045126588ec4094fe436ce739c3))
* replace mask division with bit-shift in composite hot path ([e4b7982](https://github.com/matyushkin/djvu-rs/commit/e4b7982903dbaa040e3285ecb5ace4f24ca06b73))
* use chunks_exact_mut in composite loops — eliminate per-pixel bounds checks ([0176860](https://github.com/matyushkin/djvu-rs/commit/01768603ded218496c95678c40b5cc892a9dae9e))

## [0.16.1](https://github.com/matyushkin/djvu-rs/compare/v0.16.0...v0.16.1) (2026-05-04)

### Fixed

* Gate the async lazy example behind the `async` feature so default CI and
  release validation do not build it without `tokio`.
* Fix no-default-features wasm builds after the workspace crate split.
* Publish all extracted workspace crates in dependency order before publishing
  the umbrella `djvu-rs` crate.

## [0.16.0](https://github.com/matyushkin/djvu-rs/compare/v0.15.0...v0.16.0) (2026-05-04)


### Features

* **async:** add native lazy page loader ([735a226](https://github.com/matyushkin/djvu-rs/commit/735a22615ad91c89b2a38c865407a46319c46815))
* **async:** add wasm lazy reader entrypoint ([77fc6ff](https://github.com/matyushkin/djvu-rs/commit/77fc6ffecd57cb5760f9a7e75333987b3530c872))
* **async:** resolve lazy shared dictionaries ([30d8ac9](https://github.com/matyushkin/djvu-rs/commit/30d8ac9f2a4be61c5e82b4bea5f1d8083d4bcd07))

## [Unreleased]

### Changed

* **crates:** split codec primitives into publishable workspace crates: `djvu-zp`,
  `djvu-bzz`, `djvu-iff`, `djvu-bitmap`, `djvu-jb2`, `djvu-pixmap`, and
  `djvu-iw44` ([#229](https://github.com/matyushkin/djvu-rs/issues/229)).
  The umbrella `djvu-rs` crate keeps the historical module paths as re-export
  shims, while consumers that only need `djvu_iff::parse_form` can depend on
  `djvu-iff` directly; a path-dependency cold `cargo check` for an iff-only
  consumer measured 3.67 s.

## [0.15.0](https://github.com/matyushkin/djvu-rs/compare/v0.14.0...v0.15.0) (2026-05-03)


### Features

* **api:** bundled DJVM mutation + set_bookmarks (PR3 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#268](https://github.com/matyushkin/djvu-rs/issues/268)) ([6672a93](https://github.com/matyushkin/djvu-rs/commit/6672a936202f66b32e0e77dafdf467503547de6e))
* **api:** DjVuDocumentMut::from_bytes — chunk-replacement primitive (PR1 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#263](https://github.com/matyushkin/djvu-rs/issues/263)) ([b6279ae](https://github.com/matyushkin/djvu-rs/commit/b6279aed03de0eae42b1df7ee22da0bb56efaf08))
* **api:** high-level setters for DjVuDocumentMut (PR2 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#267](https://github.com/matyushkin/djvu-rs/issues/267)) ([eec0815](https://github.com/matyushkin/djvu-rs/commit/eec08153575052d81f71eb5382176816f1592aff))
* **async:** load_document_async — buffered AsyncRead constructor ([#196](https://github.com/matyushkin/djvu-rs/issues/196) Phase 1) ([#231](https://github.com/matyushkin/djvu-rs/issues/231)) ([2d85e65](https://github.com/matyushkin/djvu-rs/commit/2d85e65e1963efb88bb86fe4b1b7e706228e910d))
* **async:** page_byte_range API + streaming async loader ([#196](https://github.com/matyushkin/djvu-rs/issues/196) Phase 2) ([#237](https://github.com/matyushkin/djvu-rs/issues/237)) ([a365abb](https://github.com/matyushkin/djvu-rs/commit/a365abb64b06a1c86de87e230623e05f465909e1))
* **cli:** multi-page djvu encode from a directory of PNGs ([#223](https://github.com/matyushkin/djvu-rs/issues/223) follow-up) ([#245](https://github.com/matyushkin/djvu-rs/issues/245)) ([b8858bb](https://github.com/matyushkin/djvu-rs/commit/b8858bb36366be29441c17a06f0ea75daa4a210a))
* **djvu-enc:** high-level PageEncoder for bilevel Lossless ([#218](https://github.com/matyushkin/djvu-rs/issues/218)) ([#243](https://github.com/matyushkin/djvu-rs/issues/243)) ([afefcef](https://github.com/matyushkin/djvu-rs/commit/afefcef00cb2026772b4b22eb896c5533f87b844))
* **djvu-enc:** minimal layered Quality (segment → Sjbz + BG44) ([#246](https://github.com/matyushkin/djvu-rs/issues/246)) ([2febd04](https://github.com/matyushkin/djvu-rs/commit/2febd0486b839d800390e0abc1729965f02ad66c))
* **epub:** opt-in reflowable text section per page ([#228](https://github.com/matyushkin/djvu-rs/issues/228)) ([#240](https://github.com/matyushkin/djvu-rs/issues/240)) ([8207fb9](https://github.com/matyushkin/djvu-rs/commit/8207fb9e3046f594e5fb13150fa2d1220c5b35cf))
* **fgbz-enc:** FGbz foreground palette encoder ([#217](https://github.com/matyushkin/djvu-rs/issues/217)) ([#241](https://github.com/matyushkin/djvu-rs/issues/241)) ([e43bc9f](https://github.com/matyushkin/djvu-rs/commit/e43bc9f81ed3bc5e3cfaf5fa249c651749583a36))
* **jb2-enc:** expose tunable shared-Djbz clustering + corpus harness ([#194](https://github.com/matyushkin/djvu-rs/issues/194) Phase 2) ([#219](https://github.com/matyushkin/djvu-rs/issues/219)) ([9aafe42](https://github.com/matyushkin/djvu-rs/commit/9aafe42f04b6d474fefa0bbc73f06b4c666c9d1f))
* **render:** public render_streaming API (Phase 2 of [#225](https://github.com/matyushkin/djvu-rs/issues/225)) ([#260](https://github.com/matyushkin/djvu-rs/issues/260)) ([b92fac7](https://github.com/matyushkin/djvu-rs/commit/b92fac7c962c57115673eb31b6f84a5e7b36c085))
* **segment + cli:** FG/BG segmentation v1 + djvu encode subcommand ([#220](https://github.com/matyushkin/djvu-rs/issues/220), [#223](https://github.com/matyushkin/djvu-rs/issues/223)) ([#244](https://github.com/matyushkin/djvu-rs/issues/244)) ([4945b06](https://github.com/matyushkin/djvu-rs/commit/4945b0670938f4623e5368c14aa5ba41a0b107a5))
* **smmr-enc:** public Smmr (G4/MMR) encoder API ([#221](https://github.com/matyushkin/djvu-rs/issues/221)) ([#242](https://github.com/matyushkin/djvu-rs/issues/242)) ([009c706](https://github.com/matyushkin/djvu-rs/commit/009c70681eb9c34992ed8a438343638a694cd98b))
* **text:** add reflowable_text() for paragraph reading-order extraction ([#228](https://github.com/matyushkin/djvu-rs/issues/228)) ([#239](https://github.com/matyushkin/djvu-rs/issues/239)) ([221a49a](https://github.com/matyushkin/djvu-rs/commit/221a49a1c066bdef59d5dcabc26961e8d6283752))


### Bug Fixes

* **iff:** preserve FORM length parity for byte-identical mutation (PR4 of [#222](https://github.com/matyushkin/djvu-rs/issues/222)) ([#269](https://github.com/matyushkin/djvu-rs/issues/269)) ([df1e95e](https://github.com/matyushkin/djvu-rs/commit/df1e95e51c08ebe4e85cb1899352aae0cf5a5692))
* **iw44:** correct vext lane in prelim_flags_band0_neon horizontal-OR ([#266](https://github.com/matyushkin/djvu-rs/issues/266)) ([b390681](https://github.com/matyushkin/djvu-rs/commit/b3906813dbe4d4c9946c93b9b9e6884c1da62efc))
* **jb2-enc:** cap cluster_shared_symbols pixel budget at decoder limit ([#270](https://github.com/matyushkin/djvu-rs/issues/270)) ([#271](https://github.com/matyushkin/djvu-rs/issues/271)) ([78e5c79](https://github.com/matyushkin/djvu-rs/commit/78e5c793d3d7a86512e42820d8e3cf0c9870c48e))
* **render:** scale page-space coords into FG44 + BG plane space ([#199](https://github.com/matyushkin/djvu-rs/issues/199)) ([#248](https://github.com/matyushkin/djvu-rs/issues/248)) ([e2c07df](https://github.com/matyushkin/djvu-rs/commit/e2c07df45d5cfbc1edbef07553549e4fbf0fd2fc))


### Performance Improvements

* **iw44:** WASM simd128 inverse wavelet load/store (Phase 2 of [#190](https://github.com/matyushkin/djvu-rs/issues/190)) ([#257](https://github.com/matyushkin/djvu-rs/issues/257)) ([353813e](https://github.com/matyushkin/djvu-rs/commit/353813eac5460f1e1f4976fc639881ca8a98a306))
* **iw44:** WASM simd128 ycbcr_raw kernels (Phase 1 of [#190](https://github.com/matyushkin/djvu-rs/issues/190)) ([#253](https://github.com/matyushkin/djvu-rs/issues/253)) ([d59fcee](https://github.com/matyushkin/djvu-rs/commit/d59fcee1c770011a726ee41d77cf8964629dbc94))
* **iw44:** x86_64 AVX2 ports of prelim_flags kernels (Phase 3 of [#189](https://github.com/matyushkin/djvu-rs/issues/189)) ([#261](https://github.com/matyushkin/djvu-rs/issues/261)) ([0ed7a36](https://github.com/matyushkin/djvu-rs/commit/0ed7a36e3c117d83c9bf9dde6df5bd347a2652cf))
* **iw44:** x86_64 AVX2 stride-1 load/store (Phase 2 of [#189](https://github.com/matyushkin/djvu-rs/issues/189)) ([#252](https://github.com/matyushkin/djvu-rs/issues/252)) ([3edf027](https://github.com/matyushkin/djvu-rs/commit/3edf02784f68710f9eb1eeb7762a986a322b0ab3))
* **iw44:** x86_64 AVX2 ycbcr_raw kernels (Phase 1 of [#189](https://github.com/matyushkin/djvu-rs/issues/189)) ([#251](https://github.com/matyushkin/djvu-rs/issues/251)) ([7f3d867](https://github.com/matyushkin/djvu-rs/commit/7f3d8679e68f7c1daa8d7973e82d475ab3974c47))
* **jb2-enc:** opt-in lossy rec-7 near-duplicate substitution (Phase 4 of [#224](https://github.com/matyushkin/djvu-rs/issues/224)) ([#256](https://github.com/matyushkin/djvu-rs/issues/256)) ([98fb8c7](https://github.com/matyushkin/djvu-rs/commit/98fb8c76c22a3eb4a7306585f030605a0941673b))
* **jb2-enc:** per-CC accounting harness for shared-Djbz ([#194](https://github.com/matyushkin/djvu-rs/issues/194) Phase 2.5) ([#255](https://github.com/matyushkin/djvu-rs/issues/255)) ([f09bfdf](https://github.com/matyushkin/djvu-rs/commit/f09bfdf71ff0ce9959baa67aedb0c2fe73535f99))
* **render:** internal row-streaming refactor (Phase 1 of [#225](https://github.com/matyushkin/djvu-rs/issues/225)) ([#259](https://github.com/matyushkin/djvu-rs/issues/259)) ([3d22a59](https://github.com/matyushkin/djvu-rs/commit/3d22a59ae90ccff0cfcc0b4eb3fb21c7bb693dc6))

## [0.14.0](https://github.com/matyushkin/djvu-rs/compare/v0.13.0...v0.14.0) (2026-04-28)


### Features

* **jb2-enc:** multi-page shared Djbz dictionary in DJVM bundle, Phase 1 ([#194](https://github.com/matyushkin/djvu-rs/issues/194)) ([#216](https://github.com/matyushkin/djvu-rs/issues/216)) ([507b061](https://github.com/matyushkin/djvu-rs/commit/507b0618b27dc6619ce33736d01a9e920f2fdb32))
* **jb2-enc:** symbol-dictionary encoder Phases 1-3 ([#188](https://github.com/matyushkin/djvu-rs/issues/188)) ([#213](https://github.com/matyushkin/djvu-rs/issues/213)) ([8a2ed15](https://github.com/matyushkin/djvu-rs/commit/8a2ed150ad86bde2d0305c0b18e1297523590ce2))


### Bug Fixes

* **annotation:** add MAX_SEXPR_DEPTH=64 guard to prevent stack overflow on deeply nested S-expressions ([#200](https://github.com/matyushkin/djvu-rs/issues/200)) ([2f03789](https://github.com/matyushkin/djvu-rs/commit/2f03789638a65f9ac2e6c7cf5763f63ec9a3a0f6))
* **jb2-enc:** tile direct encoder to ≤1MP records ([#198](https://github.com/matyushkin/djvu-rs/issues/198)) ([#214](https://github.com/matyushkin/djvu-rs/issues/214)) ([24ac60f](https://github.com/matyushkin/djvu-rs/commit/24ac60fce1c139a8b55a3d30e71a7c36cac5d6d8))


### Performance Improvements

* **iw44-enc:** NEON forward wavelet + parallel Y/Cb/Cr + skip empty passes ([#206](https://github.com/matyushkin/djvu-rs/issues/206)) ([d1f2765](https://github.com/matyushkin/djvu-rs/commit/d1f27658d9eecfcd741a63788a56be7a891e8ed0))
* **iw44:** NEON decoder — column pass s=2/4, scatter, YCbCr, lifting (~25% to_rgb) ([#207](https://github.com/matyushkin/djvu-rs/issues/207)) ([f570297](https://github.com/matyushkin/djvu-rs/commit/f5702973fd0833ed195554335853325ece4e5f12))
* **jb2-enc:** eliminate bounds checks from JB2 encode hot loop ([#205](https://github.com/matyushkin/djvu-rs/issues/205)) ([5cbeaf0](https://github.com/matyushkin/djvu-rs/commit/5cbeaf0f64afbb2d39b86f568c73d04df9c9d35a))

## [0.13.0](https://github.com/matyushkin/djvu-rs/compare/v0.12.0...v0.13.0) (2026-04-18)


### Features

* **ci:** add ocr-tesseract integration test + CI job ([#178](https://github.com/matyushkin/djvu-rs/issues/178)) ([220671d](https://github.com/matyushkin/djvu-rs/commit/220671d0ae6d21f05a2a847945e062ae27eb931c))
* **epub:** DPI-aware rendering, language tag, hyperlinks, cover image ([191e386](https://github.com/matyushkin/djvu-rs/commit/191e3869623a091998d75761f9be30d75c9576a8))
* **mask:** wire Smmr (G4/MMR) decoder into both render pipelines ([07272b3](https://github.com/matyushkin/djvu-rs/commit/07272b32f44da9b4382bd9f1d4ff7828ce6223d3))
* **tiff:** embed DPI resolution tags in exported TIFF files ([87efb14](https://github.com/matyushkin/djvu-rs/commit/87efb140cc603a3ad4bd0f13b5e7a12c995ca516))


### Bug Fixes

* **ci:** install libleptonica-dev + libtesseract-dev for ocr-tesseract job ([0410f70](https://github.com/matyushkin/djvu-rs/commit/0410f70f0ed73be0425906685c3af1e0b6397374))
* **ci:** split test into test-stable + test-beta jobs (matrix.rust not allowed in job if) ([0d60c76](https://github.com/matyushkin/djvu-rs/commit/0d60c7662a78bff90dad81d9847e07d93dfb3b15))
* **docs:** resolve all remaining broken intra-doc links (0 warnings) ([4ac8bb1](https://github.com/matyushkin/djvu-rs/commit/4ac8bb162d922529ac4e58757155d556fbe3e7fc))
* **docs:** resolve broken intra-doc links in djvu_document ([8dbfda0](https://github.com/matyushkin/djvu-rs/commit/8dbfda050a72df34dbe3f149427aec4a6e4252a9))
* **fuzz:** revert to cargo install cargo-fuzz, add explicit binary cache ([84e6eef](https://github.com/matyushkin/djvu-rs/commit/84e6eef4b18c6f957f327d34fc2f87ab560a5f3c))
* **fuzz:** use correct public module djvu_rs::jb2 in fuzz_jb2 target ([9133de6](https://github.com/matyushkin/djvu-rs/commit/9133de69f3af8ee05f2c8f9239efcf87e2ed0a76))
* **pdf:** Unicode-aware glyph width for invisible text layer ([2398c38](https://github.com/matyushkin/djvu-rs/commit/2398c3851d8cef9c5a2b41dbcdaf5352baf9575a))
* **render:** add #[allow(unsafe_code)] + unsafe blocks for Rust 2024 SIMD ([#169](https://github.com/matyushkin/djvu-rs/issues/169)) ([179d171](https://github.com/matyushkin/djvu-rs/commit/179d17132c31b5ec31eaa99156acc4ac4cdda1f6))
* **render:** correct gamma LUT formula to match DjVuLibre ([#161](https://github.com/matyushkin/djvu-rs/issues/161)) ([cd3228d](https://github.com/matyushkin/djvu-rs/commit/cd3228d812e77f6d9bdcc48e1d9a92f8ff34077e))
* **render:** resolve unsafe_code / unsafe_op_in_unsafe_fn CI conflicts ([889f28f](https://github.com/matyushkin/djvu-rs/commit/889f28fb67c5b13c8f5b4a66ae32b1546a498988))
* **render:** restore bilevel composite fast path, recover 2× regression from [#165](https://github.com/matyushkin/djvu-rs/issues/165) ([46b6931](https://github.com/matyushkin/djvu-rs/commit/46b69318d0e1181d4eec5d9fc428cc5eee9723f6))
* **render:** use core::arch instead of std::arch for no_std compatibility ([122b989](https://github.com/matyushkin/djvu-rs/commit/122b989708e475658a764b2b145a887605590e37))
* resolve issues [#164](https://github.com/matyushkin/djvu-rs/issues/164) [#169](https://github.com/matyushkin/djvu-rs/issues/169) [#170](https://github.com/matyushkin/djvu-rs/issues/170) [#174](https://github.com/matyushkin/djvu-rs/issues/174) [#176](https://github.com/matyushkin/djvu-rs/issues/176) [#177](https://github.com/matyushkin/djvu-rs/issues/177) ([774cbdb](https://github.com/matyushkin/djvu-rs/commit/774cbdb834c703c27ca82ed9f8f3bb4a17504aeb))
* wasm CI job, open_dir API, DPI scaling in OCR export, jb2_new cleanup ([6cbe038](https://github.com/matyushkin/djvu-rs/commit/6cbe038909858f4c523f13d7204998a7109cd84d))
* **zp:** widen a/c/fence fields from u16 to u32 to match jb2 inline decoder ([fb0db12](https://github.com/matyushkin/djvu-rs/commit/fb0db122daa4dccd97f49a971103de0add5a2366))


### Performance Improvements

* **bzz:** inline ZP state locals in MTF decode hot loop ([bad5b21](https://github.com/matyushkin/djvu-rs/commit/bad5b215bfa5384589f2252b3da2ee0ecb9a3ac2))
* **ci:** single nextest pass on main avoids sequential overhead ([f17d901](https://github.com/matyushkin/djvu-rs/commit/f17d901b1055c3cc2fb8e6dbfa3b6d445b6aa357))
* **deps:** split ocr-neural into lightweight stub + ocr-neural-candle ([#175](https://github.com/matyushkin/djvu-rs/issues/175)) ([3a930b2](https://github.com/matyushkin/djvu-rs/commit/3a930b2e38ca939eb15af2eefbb55156a158224b))
* **jb2:** bit-pack Jbm to 1 bit/pixel — 8x memory, corpus −3.9% ([#187](https://github.com/matyushkin/djvu-rs/issues/187)) ([17f331f](https://github.com/matyushkin/djvu-rs/commit/17f331f3d44e9e8b6f2d179b77e7212ba5b47433))
* **render:** x86_64 SSE2/SSSE3 fast paths for alpha fill and RGB→RGBA ([#169](https://github.com/matyushkin/djvu-rs/issues/169)) ([0ea3f3e](https://github.com/matyushkin/djvu-rs/commit/0ea3f3e674ae1eb112fb73368038e3fd0b2e0dde))

## [0.12.0](https://github.com/matyushkin/djvu-rs/compare/v0.11.1...v0.12.0) (2026-04-14)


### Features

* **pdf:** parallel page rendering with rayon ([#148](https://github.com/matyushkin/djvu-rs/issues/148)) ([ae79a7c](https://github.com/matyushkin/djvu-rs/commit/ae79a7c6539fd2173885b18aa4e3beb7146b70b4))
* **wasm:** progressive IW44 render API ([#150](https://github.com/matyushkin/djvu-rs/issues/150)) ([35d3a30](https://github.com/matyushkin/djvu-rs/commit/35d3a30d285fd699781d9c4e5373b584b29f544d))


### Bug Fixes

* **ocr:** resolve all clippy errors in ocr_neural, ocr_onnx, ocr_tesseract ([65a0cee](https://github.com/matyushkin/djvu-rs/commit/65a0cee338411a608c0c8a92d02c3638cb18c153))
* update tesseract API and optimize JB2 inner loop ([f64d884](https://github.com/matyushkin/djvu-rs/commit/f64d88423aff0d4861a6dd50c1ec7a4c45e1d10b))


### Performance Improvements

* allow 1.5× upscale in IW44 subsample selection for faster downscaled renders ([8f0baa2](https://github.com/matyushkin/djvu-rs/commit/8f0baa2a06eadce23974d973b7ee635b5939a5f0))
* downsampled mask pyramid for composite — 8 ms vs 23 ms for 150 dpi renders ([1374f27](https://github.com/matyushkin/djvu-rs/commit/1374f2707ead48b1db2a0eea48c12c8872bf9527))
* eliminate bounds checks in JB2 hot loops and ZP renormalize ([7e94000](https://github.com/matyushkin/djvu-rs/commit/7e94000ace8ed67b5410586563e7168031fa595e))
* **jb2:** close performance gap vs DjVuLibre + CLI improvements ([#159](https://github.com/matyushkin/djvu-rs/issues/159)) ([3efc430](https://github.com/matyushkin/djvu-rs/commit/3efc430234f8848ecc597302c7f6ece8ae9ac887))
* **jb2:** local-copy ZP state for register-allocation + hardware CLZ ([0590d3c](https://github.com/matyushkin/djvu-rs/commit/0590d3cd00dce042132d8759204421a7b1750f33))
* partial BG44 chunk decode for sub=4 renders — skip high-frequency refinement ([b371e4e](https://github.com/matyushkin/djvu-rs/commit/b371e4eebe5cd766114d81dac3480430bc8712ed))
* **pdf:** output_dpi option + bilevel fast path — 2× faster export ([cfccbc6](https://github.com/matyushkin/djvu-rs/commit/cfccbc6deefbf288bcd2055ee27c3fed5d7c9b54)), closes [#147](https://github.com/matyushkin/djvu-rs/issues/147)
* replace bg_subsample division with shift in composite hot path ([0e0f2a3](https://github.com/matyushkin/djvu-rs/commit/0e0f2a3aa52da045126588ec4094fe436ce739c3))
* replace mask division with bit-shift in composite hot path ([e4b7982](https://github.com/matyushkin/djvu-rs/commit/e4b7982903dbaa040e3285ecb5ace4f24ca06b73))
* use chunks_exact_mut in composite loops — eliminate per-pixel bounds checks ([0176860](https://github.com/matyushkin/djvu-rs/commit/01768603ded218496c95678c40b5cc892a9dae9e))

## [0.11.1](https://github.com/matyushkin/djvu-rs/compare/v0.11.0...v0.11.1) (2026-04-13)


### Bug Fixes

* **fuzz:** add [workspace] to fuzz/Cargo.toml to fix cargo-fuzz build ([be7a5a8](https://github.com/matyushkin/djvu-rs/commit/be7a5a80d23ca77c6c326c1c9065e1498ab41b5d))
* **smmr:** replace manual div_ceil with .div_ceil() per clippy ([a8d24ca](https://github.com/matyushkin/djvu-rs/commit/a8d24ca5fa470e9f9c59bee7ef7a605786a8fe79))

## [0.11.0](https://github.com/matyushkin/djvu-rs/compare/v0.10.0...v0.11.0) (2026-04-13)


### Features

* add BZZ encoder with ZP arithmetic coding ([582432f](https://github.com/matyushkin/djvu-rs/commit/582432f601cf546c9404228af1541ec2067b503e))
* add NAVM bookmark encoder and ANTa/ANTz annotation encoder ([#133](https://github.com/matyushkin/djvu-rs/issues/133)) ([#136](https://github.com/matyushkin/djvu-rs/issues/136)) ([9e6bf66](https://github.com/matyushkin/djvu-rs/commit/9e6bf662125194fe57be02bed2ce42d87b8c16b7))
* indirect DJVM — create_indirect() and parse_from_dir() ([#135](https://github.com/matyushkin/djvu-rs/issues/135)) ([#137](https://github.com/matyushkin/djvu-rs/issues/137)) ([d7fbf74](https://github.com/matyushkin/djvu-rs/commit/d7fbf74e45b9f5a85d0f162796709126131ab8a0))
* IW44 wavelet encoder — BG44/FG44 chunk encoding (issue [#131](https://github.com/matyushkin/djvu-rs/issues/131)) ([#139](https://github.com/matyushkin/djvu-rs/issues/139)) ([10adc4f](https://github.com/matyushkin/djvu-rs/commit/10adc4f61bf8a79f9c4ba74559fb004b31d8968c))
* JB2 bilevel image encoder — Sjbz chunk encoding (issue [#132](https://github.com/matyushkin/djvu-rs/issues/132)) ([#140](https://github.com/matyushkin/djvu-rs/issues/140)) ([cf280ba](https://github.com/matyushkin/djvu-rs/commit/cf280ba6e1356f1d8f7498c84c57e2759f603b3c))
* **smmr:** add G4/MMR bilevel image decoder (issue [#134](https://github.com/matyushkin/djvu-rs/issues/134)) ([#138](https://github.com/matyushkin/djvu-rs/issues/138)) ([8bd6e41](https://github.com/matyushkin/djvu-rs/commit/8bd6e4157c885e7a03c0443657d294099fbe7619))


### Bug Fixes

* **ci:** enable cli feature for nextest to build djvu binary ([576f12f](https://github.com/matyushkin/djvu-rs/commit/576f12faf154662a3992ac3a20c3c126e32fc798))
* **ci:** exclude djvu-py from nextest, fix audit advisories ([788b0d3](https://github.com/matyushkin/djvu-rs/commit/788b0d39fc04d1ed28dc5052f2bc88004f106117))
* **ci:** exclude fuzz/ from workspace to fix cargo-fuzz builds ([330ff0a](https://github.com/matyushkin/djvu-rs/commit/330ff0a28a601629718cf0c12d4e90ab93524621))
* **jb2_encode:** return empty Vec for zero-dimension bitmaps; unreachable dead branch ([#142](https://github.com/matyushkin/djvu-rs/issues/142)) ([#143](https://github.com/matyushkin/djvu-rs/issues/143)) ([091b657](https://github.com/matyushkin/djvu-rs/commit/091b6573467c67c3c8d487ee82dba24b7c59f136))

## [0.10.0](https://github.com/matyushkin/djvu-rs/compare/v0.9.0...v0.10.0) (2026-04-10)


### Features

* **djvm:** add merge and split commands for DjVu documents ([#126](https://github.com/matyushkin/djvu-rs/issues/126)) ([37eefd8](https://github.com/matyushkin/djvu-rs/commit/37eefd89f969ade36e3547fce0222a9d23d9c215)), closes [#76](https://github.com/matyushkin/djvu-rs/issues/76)
* **ffi:** add C FFI bindings via extern "C" functions ([#127](https://github.com/matyushkin/djvu-rs/issues/127)) ([2226bdf](https://github.com/matyushkin/djvu-rs/commit/2226bdfad34ef39a08bb9a3b1b220cac2942b30a)), closes [#72](https://github.com/matyushkin/djvu-rs/issues/72)
* **ocr:** pluggable OCR backend trait with Tesseract, ONNX, and Candle backends ([#125](https://github.com/matyushkin/djvu-rs/issues/125)) ([bf26603](https://github.com/matyushkin/djvu-rs/commit/bf26603b714d1721f63beedcf2ad54a5f95e21a0)), closes [#77](https://github.com/matyushkin/djvu-rs/issues/77)
* **python:** add Python bindings via PyO3 ([#128](https://github.com/matyushkin/djvu-rs/issues/128)) ([e250fff](https://github.com/matyushkin/djvu-rs/commit/e250fffcef36531d8641e1ab8031198c30c7d372)), closes [#71](https://github.com/matyushkin/djvu-rs/issues/71)


### Bug Fixes

* eliminate memory leaks and add OOM protection ([25f041d](https://github.com/matyushkin/djvu-rs/commit/25f041d78c513f1e3dfbd87aa69dc4fc261488da))
* **jb2:** prevent infinite loop in decode_num on corrupt streams ([d7bee1e](https://github.com/matyushkin/djvu-rs/commit/d7bee1e8c8c612583a055d40c4aa7a8c81666e47)), closes [#122](https://github.com/matyushkin/djvu-rs/issues/122)

## [0.9.0](https://github.com/matyushkin/djvu-rs/compare/v0.8.0...v0.9.0) (2026-04-09)


### Features

* **epub:** EPUB 3 export — page images, text overlay, navigation (Issue [#74](https://github.com/matyushkin/djvu-rs/issues/74)) ([9a6d155](https://github.com/matyushkin/djvu-rs/commit/9a6d1555c43041b86a783c5967a55607e0090768))
* **wasm:** add WasmPage::text_zones_json() — text selection overlay API ([e513bbf](https://github.com/matyushkin/djvu-rs/commit/e513bbfc4052161f0b61f0c2cc429c58869d7170)), closes [#119](https://github.com/matyushkin/djvu-rs/issues/119)
* **wasm:** add WasmPage::text() — expose page text layer to JS ([35a776d](https://github.com/matyushkin/djvu-rs/commit/35a776d16804b39e1ae8cfddbf18073e4711204c))


### Bug Fixes

* **wasm:** correct render() pixel layout and Uint8ClampedArray allocation ([1a967d2](https://github.com/matyushkin/djvu-rs/commit/1a967d2003bb74f0e42bccc8f108b2f1303617c1))

## [0.8.0](https://github.com/matyushkin/djvu-rs/compare/v0.7.1...v0.8.0) (2026-04-09)


### Features

* **epub:** EPUB 3 export — page images, text overlay, navigation (Issue [#74](https://github.com/matyushkin/djvu-rs/issues/74)) ([9a6d155](https://github.com/matyushkin/djvu-rs/commit/9a6d1555c43041b86a783c5967a55607e0090768))
* **wasm:** add WasmPage::text_zones_json() — text selection overlay API ([e513bbf](https://github.com/matyushkin/djvu-rs/commit/e513bbfc4052161f0b61f0c2cc429c58869d7170)), closes [#119](https://github.com/matyushkin/djvu-rs/issues/119)
* **wasm:** add WasmPage::text() — expose page text layer to JS ([35a776d](https://github.com/matyushkin/djvu-rs/commit/35a776d16804b39e1ae8cfddbf18073e4711204c))
* **wasm:** WebAssembly bindings via wasm-bindgen (Issue [#73](https://github.com/matyushkin/djvu-rs/issues/73)) ([#118](https://github.com/matyushkin/djvu-rs/issues/118)) ([4300939](https://github.com/matyushkin/djvu-rs/commit/4300939b9085725005f7ce0a62d4e34f836367ff))


### Bug Fixes

* **wasm:** correct render() pixel layout and Uint8ClampedArray allocation ([1a967d2](https://github.com/matyushkin/djvu-rs/commit/1a967d2003bb74f0e42bccc8f108b2f1303617c1))

## [0.7.1](https://github.com/matyushkin/djvu-rs/compare/v0.7.0...v0.7.1) (2026-04-08)


### Performance Improvements

* **iw44:** compact-plane wavelet for sub≥2 + correct start_scale (Issue [#115](https://github.com/matyushkin/djvu-rs/issues/115)) ([#116](https://github.com/matyushkin/djvu-rs/issues/116)) ([4fc8921](https://github.com/matyushkin/djvu-rs/commit/4fc8921a7892834b3be97ed952d30000ea467b21))

## [0.7.0](https://github.com/matyushkin/djvu-rs/compare/v0.6.0...v0.7.0) (2026-04-08)


### Features

* **async:** progressive stream render API (Issue [#81](https://github.com/matyushkin/djvu-rs/issues/81)) ([#112](https://github.com/matyushkin/djvu-rs/issues/112)) ([eaff91d](https://github.com/matyushkin/djvu-rs/commit/eaff91d5f5b8ccfa3cf86429339e7c21bcde5f72))
* **ci:** continuous benchmark tracking — PR regression detection (Issue [#88](https://github.com/matyushkin/djvu-rs/issues/88)) ([#109](https://github.com/matyushkin/djvu-rs/issues/109)) ([0dfade5](https://github.com/matyushkin/djvu-rs/commit/0dfade5a8c4ec125bac50ff9deb5df37a159c56f))
* **render:** zero-copy region render — render_region API (Issue [#86](https://github.com/matyushkin/djvu-rs/issues/86)) ([#111](https://github.com/matyushkin/djvu-rs/issues/111)) ([b2aa2a8](https://github.com/matyushkin/djvu-rs/commit/b2aa2a860dc28a63409785af8ef2be55cca40a11))


### Performance Improvements

* **bzz:** parallel inverse-BWT via rayon (Issue [#89](https://github.com/matyushkin/djvu-rs/issues/89)) ([#110](https://github.com/matyushkin/djvu-rs/issues/110)) ([eb5bab0](https://github.com/matyushkin/djvu-rs/commit/eb5bab0024029e494a7ab069db46425e8e75b2f9))
* **iw44:** SIMD row pass — 8 rows at a time with i32x8 ([#107](https://github.com/matyushkin/djvu-rs/issues/107)) ([1418ff4](https://github.com/matyushkin/djvu-rs/commit/1418ff4f3c2cdfd1e0ee7a1210bc3852d0240239))

## [0.6.0](https://github.com/matyushkin/djvu-rs/compare/v0.5.0...v0.6.0) (2026-04-06)


### Features

* hOCR and ALTO XML export for text layer (Issue [#75](https://github.com/matyushkin/djvu-rs/issues/75)) ([#98](https://github.com/matyushkin/djvu-rs/issues/98)) ([263cf14](https://github.com/matyushkin/djvu-rs/commit/263cf1492b57caa2b1d986a7eec8fd4a6cc8305b))
* implement ImageDecoder trait for image-rs integration (Issue [#80](https://github.com/matyushkin/djvu-rs/issues/80)) ([#97](https://github.com/matyushkin/djvu-rs/issues/97)) ([d7e4a64](https://github.com/matyushkin/djvu-rs/commit/d7e4a64e427454f4353f5124d1d03ba4ae31fe8c))
* serde support for metadata, annotations, bookmarks, and text zones (Issue [#82](https://github.com/matyushkin/djvu-rs/issues/82)) ([#96](https://github.com/matyushkin/djvu-rs/issues/96)) ([e872ecd](https://github.com/matyushkin/djvu-rs/commit/e872ecdefbee38e0862c899da3955c2ee98ca233))


### Bug Fixes

* **ci:** use core::mem::take in no_std context; fix clippy redundant-Some in ocr_export test ([#101](https://github.com/matyushkin/djvu-rs/issues/101)) ([cc1cdf1](https://github.com/matyushkin/djvu-rs/commit/cc1cdf136e4eaaf9311170a20cd1f3b5ffd4ce54))
* **jb2:** correct regression test comment for fuzz2 fix ([ee380ae](https://github.com/matyushkin/djvu-rs/commit/ee380ae87d3f589627182b5a1350ae76072eb901))
* **jb2:** guard blit against negative symbol dimensions ([49a3792](https://github.com/matyushkin/djvu-rs/commit/49a3792c764e25594165a10b127612a446d7a732))
* **jb2:** reduce MAX_RECORDS and MAX_SYMBOL_PIXELS to prevent fuzz timeouts ([3292193](https://github.com/matyushkin/djvu-rs/commit/3292193237d733976ea24c879ade958b1021caaa))


### Performance Improvements

* **iw44:** allocate chroma planes at half resolution when chroma_half=true (Issue [#85](https://github.com/matyushkin/djvu-rs/issues/85)) ([#99](https://github.com/matyushkin/djvu-rs/issues/99)) ([927e7c0](https://github.com/matyushkin/djvu-rs/commit/927e7c01580412e45a1487ddb98d62b524eed059))
* **jb2:** reuse scratch buffer across symbol decodes to eliminate per-symbol heap allocations (Issue [#90](https://github.com/matyushkin/djvu-rs/issues/90)) ([#100](https://github.com/matyushkin/djvu-rs/issues/100)) ([12575d0](https://github.com/matyushkin/djvu-rs/commit/12575d095e73e650a5b04f4a5a56624b15c541f9))
* **jb2:** shared dict cache + split_at_mut inner loop (Issue [#87](https://github.com/matyushkin/djvu-rs/issues/87)) ([#106](https://github.com/matyushkin/djvu-rs/issues/106)) ([08ca0f4](https://github.com/matyushkin/djvu-rs/commit/08ca0f43f557024411e765afd9f419c2681f6275))
* **render:** 66% speedup on 600 dpi bilevel pages (Issue [#104](https://github.com/matyushkin/djvu-rs/issues/104)) ([#105](https://github.com/matyushkin/djvu-rs/issues/105)) ([8e5a2f4](https://github.com/matyushkin/djvu-rs/commit/8e5a2f42fdb0872b5a13d2560330346b0dc09989))
* **render:** NEON bilinear vertical pass + 4-byte RGBX stride ([#93](https://github.com/matyushkin/djvu-rs/issues/93)) ([b0dfdb8](https://github.com/matyushkin/djvu-rs/commit/b0dfdb8de78e34f5f7478bc06bf3c3dfad21d1df))
* **render:** precomputed coord tables, zero-copy BG path, remove PageMapper ([cf1a8e9](https://github.com/matyushkin/djvu-rs/commit/cf1a8e99eebf86d47c35fa403603354cbd23a5d7))

## [0.5.3] (unreleased)


### Performance Improvements

* **jb2:** cache shared symbol dictionary to avoid re-decoding Djbz on every `decode_mask()` call — `render_large_doc_first_page` 14.5 ms → 10.5 ms (−28%), `render_large_doc_mid_page` 43.9 ms → 36.2 ms (−18%) (closes [#87](https://github.com/matyushkin/djvu-rs/issues/87))
  - `Document::get_or_decode_dict`: `RwLock<HashMap<usize, Arc<JB2Dict>>>` keyed by Djbz data pointer — multi-page documents decode the shared dictionary once across all pages
  - `decode_bitmap_direct`: `split_at_mut` look-ahead row access eliminates per-pixel `row * width` multiply and 4-comparison bounds checks; `jb2_decode` small-page benchmark: 245 µs → 189 µs (−23%)


## [0.5.2](https://github.com/matyushkin/djvu-rs/compare/v0.5.1...v0.5.2) (2026-04-06)


### Performance Improvements

* **render:** 66% speedup on 600 dpi bilevel pages — `render_large_doc_first_page` 42.7 ms → 14.5 ms (closes [#104](https://github.com/matyushkin/djvu-rs/issues/104))
  - `Pixmap::new`: replaced per-pixel push loop with bulk fill — 18 ms → 0.8 ms (−95%) for a 2649×4530 buffer
  - `composite_bilevel`: row-slice writes + rayon `par_chunks_mut` under `--features parallel`
  - Skip `apply_gamma` for pure bilevel pages (0/255 values, gamma is a mathematical no-op)
  - Parallel Y/Cb/Cr wavelet reconstruction via `rayon::join` under `--features parallel`
  - Parallel bilinear scaler passes under `--features parallel`


## [0.5.0](https://github.com/matyushkin/djvu-rs/compare/v0.4.2...v0.5.0) (2026-04-05)


### Features

* **fuzz:** add render to fuzz_full, add CI fuzz workflow (60 s/target) ([7e3e4eb](https://github.com/matyushkin/djvu-rs/commit/7e3e4eb0d51404d0c460b71ac5a359d4eac6da8b))
* **mmap:** add memory-mapped I/O via MmapDocument ([387a2ea](https://github.com/matyushkin/djvu-rs/commit/387a2ea40696d88906dc5ead848154cf42189c6a)), closes [#70](https://github.com/matyushkin/djvu-rs/issues/70)
* **render:** add rayon-based parallel page rendering ([3dc06f9](https://github.com/matyushkin/djvu-rs/commit/3dc06f991d7e26099ea4623580ebded7022f8775)), closes [#69](https://github.com/matyushkin/djvu-rs/issues/69)


### Bug Fixes

* **clippy:** use `contains()` instead of `iter().any()` in tiff_export ([f9b4c2b](https://github.com/matyushkin/djvu-rs/commit/f9b4c2b7b9ad3aadbc9539aec09ef651f5545312))
* **jb2,iw44:** cap comment bytes and IW44 pixel limit to prevent fuzz timeouts ([49c7b1b](https://github.com/matyushkin/djvu-rs/commit/49c7b1bd29505528d53b70e7ab820e5a9c0eae2e))
* **jb2,iw44:** prevent DoS via refinement bitmaps and uncapped total pixel budget ([3e72cf6](https://github.com/matyushkin/djvu-rs/commit/3e72cf6c33d21661db1198013d789217d8978580))
* **jb2:** add blit-pixel budget to prevent type-7 dict-copy DoS ([1c03505](https://github.com/matyushkin/djvu-rs/commit/1c0350543b870c3b29fe87c1b59370410ba5e464))
* **jb2:** cap decode loop at 1 M records to prevent infinite spin on exhausted ZP input ([0b84f2d](https://github.com/matyushkin/djvu-rs/commit/0b84f2dcadaf4cbbae75bd2e53cb82f7f99d4f2d))
* **jb2:** guard blit fast path against i32 overflow and data buffer overread ([be72d29](https://github.com/matyushkin/djvu-rs/commit/be72d29584170afb0f12a951c10d3f24859ae02f))
* **jb2:** limit symbol bitmap size to 4 MP to prevent DoS via crafted input ([943f25e](https://github.com/matyushkin/djvu-rs/commit/943f25eaacc7c91234caea2e99ff87950f7d632f))


### Performance Improvements

* **iw44:** SIMD-accelerate inverse wavelet transform column pass ([2ac4318](https://github.com/matyushkin/djvu-rs/commit/2ac4318c2c6e56af93ad5fe52670e032a70d378f)), closes [#68](https://github.com/matyushkin/djvu-rs/issues/68)

## [0.4.2](https://github.com/matyushkin/djvu-rs/compare/v0.4.1...v0.4.2) (2026-04-05)

### Documentation

* Rewrite README — DjVuDocument API, CLI examples, PDF/TIFF/async sections

## [0.4.1](https://github.com/matyushkin/djvu-rs/compare/v0.4.0...v0.4.1) (2026-04-05)

### Documentation

* remove stale next-up block from roadmap section

## [0.4.0](https://github.com/matyushkin/djvu-rs/compare/djvu-rs-v0.3.0...djvu-rs-v0.4.0) (2026-04-05)


### Features

* add fit_to_width/height/box to RenderOptions ([#33](https://github.com/matyushkin/djvu-rs/issues/33)) ([b371a93](https://github.com/matyushkin/djvu-rs/commit/b371a93099276cab573244cc262dc6ba093276cf))
* **api:** raw_chunk / all_chunks / chunk_ids on DjVuPage and DjVuDocument (Issue [#43](https://github.com/matyushkin/djvu-rs/issues/43)) ([#54](https://github.com/matyushkin/djvu-rs/issues/54)) ([3135627](https://github.com/matyushkin/djvu-rs/commit/31356279ad2739ae13ab36a144fbaabe5b5f63ab))
* **async:** async render API via tokio::task::spawn_blocking (Issue [#51](https://github.com/matyushkin/djvu-rs/issues/51)) ([#61](https://github.com/matyushkin/djvu-rs/issues/61)) ([452636f](https://github.com/matyushkin/djvu-rs/commit/452636f143c56f4fc674968be5d5a4d8bd15d14a))
* **bench:** add render_scaled and pdf_export benchmarks + BENCHMARKS.md (Issue [#52](https://github.com/matyushkin/djvu-rs/issues/52)) ([#62](https://github.com/matyushkin/djvu-rs/issues/62)) ([a8523c7](https://github.com/matyushkin/djvu-rs/commit/a8523c711c2fc3fc12d1089c5becf1b68af25595))
* **cli:** implement djvu info/render/text — 24/24 tests green ([eb2e9d6](https://github.com/matyushkin/djvu-rs/commit/eb2e9d61f0a12285f9c6d7de4a4665b31f19a32a))
* **cos-djvu:** benchmark suite, corpus infrastructure, BENCHMARKS.md (closes [#282](https://github.com/matyushkin/djvu-rs/issues/282)) ([#332](https://github.com/matyushkin/djvu-rs/issues/332)) ([50b6933](https://github.com/matyushkin/djvu-rs/commit/50b69330c73e5883a2954fe646f2cc7b7ec4e654))
* **cos-djvu:** phase 1 — IFF parser, typed errors, MIT skeleton (closes [#267](https://github.com/matyushkin/djvu-rs/issues/267)) ([#277](https://github.com/matyushkin/djvu-rs/issues/277)) ([1943f3f](https://github.com/matyushkin/djvu-rs/commit/1943f3f1cbef880038065ea377ccdfdff4ee33d0))
* **cos-djvu:** phase 2a — ZP arithmetic coder + BZZ decompressor (closes [#268](https://github.com/matyushkin/djvu-rs/issues/268)) ([#279](https://github.com/matyushkin/djvu-rs/issues/279)) ([4983056](https://github.com/matyushkin/djvu-rs/commit/498305678a0d13947e6fe5473d2bc6250383595d))
* **cos-djvu:** phase-5 rendering pipeline — compositing, gamma, scaling, AA (closes [#273](https://github.com/matyushkin/djvu-rs/issues/273)) ([7f5e161](https://github.com/matyushkin/djvu-rs/commit/7f5e161dd7f13c94ed1fb680966416e339eb80d5))
* **cos-djvu:** phase-6 quality — fuzz targets, benchmarks, no_std, full docs (closes [#274](https://github.com/matyushkin/djvu-rs/issues/274)) ([#324](https://github.com/matyushkin/djvu-rs/issues/324)) ([67fab7f](https://github.com/matyushkin/djvu-rs/commit/67fab7fdce945d972c574f61f9e460c32998a189))
* **cos-djvu:** text layer + annotations extraction (closes [#272](https://github.com/matyushkin/djvu-rs/issues/272)) ([#316](https://github.com/matyushkin/djvu-rs/issues/316)) ([5beeba9](https://github.com/matyushkin/djvu-rs/commit/5beeba96034a55396769d044ff1c20f20dc379a6))
* djvu render --format pdf|cbz, roadmap v0.1 finalised ([7d823f6](https://github.com/matyushkin/djvu-rs/commit/7d823f634a1efbc75f743d5d5a8a4f6056e0b0a0))
* DjVu to PDF converter with text, bookmarks, and hyperlinks ([#2](https://github.com/matyushkin/djvu-rs/issues/2)-[#6](https://github.com/matyushkin/djvu-rs/issues/6)) ([#29](https://github.com/matyushkin/djvu-rs/issues/29)) ([a6f0a74](https://github.com/matyushkin/djvu-rs/commit/a6f0a7486d85146e677ae094396e96b44675e894))
* document model — DjVuDocument, Page, DIRM, NAVM (closes [#271](https://github.com/matyushkin/djvu-rs/issues/271)) ([#283](https://github.com/matyushkin/djvu-rs/issues/283)) ([e36fd41](https://github.com/matyushkin/djvu-rs/commit/e36fd4169b591cb5f4146fc58bd3aaacf464d82b))
* import cos-djvu history, remove GPL legacy code ([0f33110](https://github.com/matyushkin/djvu-rs/commit/0f33110d1846e7114c6a46726c9e088cbef25bea))
* IW44 wavelet decoder with planar YCbCr (closes [#270](https://github.com/matyushkin/djvu-rs/issues/270)) ([#281](https://github.com/matyushkin/djvu-rs/issues/281)) ([f799e70](https://github.com/matyushkin/djvu-rs/commit/f799e7098dfbffdabada8e0c19fea2e31cdac351))
* JB2 bilevel decoder (closes [#269](https://github.com/matyushkin/djvu-rs/issues/269)) ([#280](https://github.com/matyushkin/djvu-rs/issues/280)) ([e2a6898](https://github.com/matyushkin/djvu-rs/commit/e2a6898618765fe96a07ddaac51a40b665b64efb))
* **jb2:** DJVI shared dictionary support via INCL chunks (Issue [#45](https://github.com/matyushkin/djvu-rs/issues/45)) ([#56](https://github.com/matyushkin/djvu-rs/issues/56)) ([86a63cb](https://github.com/matyushkin/djvu-rs/commit/86a63cb975303c14159340da4718ed3e23182e3e))
* mask and foreground/background layer extraction API ([#36](https://github.com/matyushkin/djvu-rs/issues/36)) ([d4c6527](https://github.com/matyushkin/djvu-rs/commit/d4c6527eb03aa9397a681ccdf03a27cf9ed77b0b))
* **metadata:** METa/METz document metadata parsing (Issue [#44](https://github.com/matyushkin/djvu-rs/issues/44)) ([#55](https://github.com/matyushkin/djvu-rs/issues/55)) ([eb4515b](https://github.com/matyushkin/djvu-rs/commit/eb4515b49819c3b75824738989c07153a7c2c0d6))
* **pdf:** DCTDecode background encoding — smaller PDF output (Issue [#49](https://github.com/matyushkin/djvu-rs/issues/49)) ([#59](https://github.com/matyushkin/djvu-rs/issues/59)) ([de90a9f](https://github.com/matyushkin/djvu-rs/commit/de90a9fd94ef98fdeb8822aef174e945ebe5a3ea))
* progressive DjVu rendering, multi-book cache, cos-diagnostics crate ([32432d8](https://github.com/matyushkin/djvu-rs/commit/32432d8c240f4a4673310a70158cf12cb9643635))
* **render:** BGjp/FGjp JPEG background/foreground decoder (Issue [#47](https://github.com/matyushkin/djvu-rs/issues/47)) ([#57](https://github.com/matyushkin/djvu-rs/issues/57)) ([b65bd81](https://github.com/matyushkin/djvu-rs/commit/b65bd817270c21248aa3ec46b140b6fb97a9b683))
* **render:** grayscale output mode — GrayPixmap + render_gray8 ([c13ebb7](https://github.com/matyushkin/djvu-rs/commit/c13ebb75ffa0673e670530bdd7ebe53f311a5044))
* **render:** grayscale output mode — GrayPixmap + render_gray8 (Issue [#15](https://github.com/matyushkin/djvu-rs/issues/15)) ([75d7b37](https://github.com/matyushkin/djvu-rs/commit/75d7b37aed6ce0a6d4d118abc8050b731089a8bf))
* **render:** Lanczos-3 separable resampling (Issue [#50](https://github.com/matyushkin/djvu-rs/issues/50)) ([#60](https://github.com/matyushkin/djvu-rs/issues/60)) ([56817d1](https://github.com/matyushkin/djvu-rs/commit/56817d162a335fe9597d2e65f84e0778ee147c65))
* **render:** permissive render mode — skip corrupted chunks ([dc5734a](https://github.com/matyushkin/djvu-rs/commit/dc5734a088f82b6c5bf7b6cfc06a599a2f342a2b))
* **render:** permissive render mode — skip corrupted chunks (Issue [#19](https://github.com/matyushkin/djvu-rs/issues/19)) ([df5a8d7](https://github.com/matyushkin/djvu-rs/commit/df5a8d715ba8534856d50f570752b8722118c2a0))
* **text:** TextLayer::transform — rotate + scale zone rects for rendered pages (Issue [#46](https://github.com/matyushkin/djvu-rs/issues/46)) ([#53](https://github.com/matyushkin/djvu-rs/issues/53)) ([c4a514e](https://github.com/matyushkin/djvu-rs/commit/c4a514e8bbdd4bccdb0032130b949580ef5306cc))
* **tiff:** TIFF export — multi-page color and bilevel modes (Issue [#48](https://github.com/matyushkin/djvu-rs/issues/48)) ([#58](https://github.com/matyushkin/djvu-rs/issues/58)) ([dc90cc0](https://github.com/matyushkin/djvu-rs/commit/dc90cc049ee7bd299ce6156c9c390c8616bdabca))
* transfer from cos-djvu, remove legacy GPL code, add PD corpus, benchmarks ([33fd496](https://github.com/matyushkin/djvu-rs/commit/33fd4969a691b5445c708b6c5f1ae2f877304f73))
* **ui:** table of contents navigation panel (closes [#60](https://github.com/matyushkin/djvu-rs/issues/60)) ([#298](https://github.com/matyushkin/djvu-rs/issues/298)) ([3fb0b2a](https://github.com/matyushkin/djvu-rs/commit/3fb0b2a94ff19891d6469f95aa381dcf66b7b712))
* user-controllable rotation in RenderOptions ([#35](https://github.com/matyushkin/djvu-rs/issues/35)) ([e0f79a8](https://github.com/matyushkin/djvu-rs/commit/e0f79a80bd3d3b2cc43933debb9a0b290aaea285))


### Bug Fixes

* add missing chunk_data binding in iw44_new doctest ([d1a210b](https://github.com/matyushkin/djvu-rs/commit/d1a210b8b61fabc5c7634282f0bfb8062503dfa3))
* apply gamma correction in all legacy render paths ([#9](https://github.com/matyushkin/djvu-rs/issues/9)) ([#22](https://github.com/matyushkin/djvu-rs/issues/22)) ([dfba614](https://github.com/matyushkin/djvu-rs/commit/dfba614c510d8da150ee76cf62f629f89cff48bc))
* apply page rotation from INFO chunk in render_pixmap and render_coarse ([#10](https://github.com/matyushkin/djvu-rs/issues/10)) ([#24](https://github.com/matyushkin/djvu-rs/issues/24)) ([adec5ee](https://github.com/matyushkin/djvu-rs/commit/adec5eed27bf5f096bad4187a2d88a5349bd07a4))
* **ci:** IJG license allowlist, no_std BTreeMap, clippy errors ([6a2a391](https://github.com/matyushkin/djvu-rs/commit/6a2a39199dffd1aa60cbe916bd2b28a516185dcd))
* clippy errors and fmt — let-chain, ref on let, line wrapping ([b4ba2f8](https://github.com/matyushkin/djvu-rs/commit/b4ba2f87acc234e7b7467f394169775b846c03b5))
* exclude .cargo/config.toml from published package (fixes docs.rs build) ([b9dd0da](https://github.com/matyushkin/djvu-rs/commit/b9dd0dafac62f0d6e12064f431d6b962b6a93e08))
* FGbz multi-color foreground palette — use per-glyph blit index ([#12](https://github.com/matyushkin/djvu-rs/issues/12)) ([#26](https://github.com/matyushkin/djvu-rs/issues/26)) ([7897164](https://github.com/matyushkin/djvu-rs/commit/789716408799e39c0e78e3caeedb051f913d26ac))
* **hard-rule:** eliminate last 5 .expect()/.unwrap() in production code (Issue [#443](https://github.com/matyushkin/djvu-rs/issues/443)) ([#444](https://github.com/matyushkin/djvu-rs/issues/444)) ([e4247ea](https://github.com/matyushkin/djvu-rs/commit/e4247eafc1eabadfac42fb04c318252ac0ebcc71))
* remove deprecated [[licenses.deny]] syntax from deny.toml (cargo-deny v2) ([7971e44](https://github.com/matyushkin/djvu-rs/commit/7971e448cdeb82d8be78910e9f275ecc1b975d65))
* replace all internal cos-djvu/cos_djvu references with djvu-rs/djvu_rs ([03fb17a](https://github.com/matyushkin/djvu-rs/commit/03fb17ae4460ca6baff50235993c6f005ddc08e4))
* update MSRV to 1.88 (let-chains stabilized in 1.88) ([8d5b94f](https://github.com/matyushkin/djvu-rs/commit/8d5b94f9d84c9065662e2464003aa75a95613421))
* vendor djvu-rs into crates/cos-djvu and fix production panics ([5f6d7fe](https://github.com/matyushkin/djvu-rs/commit/5f6d7fecadb7ea3bdcfa5cd530215ff28ea5e133)), closes [#4](https://github.com/matyushkin/djvu-rs/issues/4)


### Performance Improvements

* area-averaging downscale for better quality when rendering at reduced size ([#13](https://github.com/matyushkin/djvu-rs/issues/13)) ([#28](https://github.com/matyushkin/djvu-rs/issues/28)) ([b822ded](https://github.com/matyushkin/djvu-rs/commit/b822ded71372b0f70071f8245768d09b89e62a17))
* **bitmap:** packed bitwise dilation with ping-pong buffers ([2887689](https://github.com/matyushkin/djvu-rs/commit/288768968e910eccfe313785aa316ef8eb0fbdac))
* **bitmap:** packed bitwise dilation with ping-pong buffers (Issue [#17](https://github.com/matyushkin/djvu-rs/issues/17)) ([0814f55](https://github.com/matyushkin/djvu-rs/commit/0814f5519475073c4b426ca91360491c8ad69830))
* eliminate redundant mask sampling in 3-layer composite ([#14](https://github.com/matyushkin/djvu-rs/issues/14)) ([#27](https://github.com/matyushkin/djvu-rs/issues/27)) ([f601036](https://github.com/matyushkin/djvu-rs/commit/f601036e8988c2ea0c4f2a5742cd4b1321bc8065))
* **iw44:** SIMD YCbCr→RGB using wide::i32x8 (Issue [#1](https://github.com/matyushkin/djvu-rs/issues/1)) ([#64](https://github.com/matyushkin/djvu-rs/issues/64)) ([abceef4](https://github.com/matyushkin/djvu-rs/commit/abceef47d2bca524a20d13f4926f4dbf84e3c79b))
* **render:** eliminate redundant mask sampling in 3-layer composite (Issue [#14](https://github.com/matyushkin/djvu-rs/issues/14)) ([#37](https://github.com/matyushkin/djvu-rs/issues/37)) ([585991d](https://github.com/matyushkin/djvu-rs/commit/585991d961978277b8fdaf2b87236f6d1b825ac0))

## [Unreleased]

## [0.3.0] — 2026-04-05

### Added

- **TIFF export** — `djvu_to_tiff` converts DjVu to multi-page TIFF in color (RGB8) or bilevel
  (Gray8) modes; CLI: `djvu render --format tiff`; feature-gated: `--features tiff`
- **BGjp/FGjp JPEG decoder** — DjVu pages with JPEG-encoded background or foreground now render
  correctly; uses `zune-jpeg` (pure Rust, no libjpeg)
- **Async render API** — `djvu_async::render_pixmap_async` / `render_gray8_async` delegate
  CPU-bound IW44/JB2 work to `tokio::task::spawn_blocking`; feature-gated: `--features async`
- **Document metadata** — `metadata::parse_metadata` / `parse_metadata_bzz` extract METa/METz
  chunks; `DjVuMetadata` struct with title, author, date, and arbitrary key-value fields
- **Chunk introspection API** — `DjVuPage::raw_chunk`, `all_chunks`, `chunk_ids`;
  `DjVuDocument::raw_chunk`, `all_chunks` for direct access to IFF chunk data
- **DJVI shared dictionary** — `Sjbz` pages that reference a shared JB2 dictionary via `INCL`
  chunks now decode correctly; fixes rendering of multi-page documents with shared symbol sets
- **TextLayer coordinate transform** — `TextLayer::transform(scale, rotation)` maps zone rectangles
  to the rendered page coordinate system; simplifies hit-testing in viewer applications
- **DCTDecode PDF export** — `pdf::djvu_to_pdf_with_options` encodes page images as JPEG
  (DCTDecode) instead of raw RGB (FlateDecode); typically 5–10× smaller output; `PdfOptions`
  controls JPEG quality (default 80)
- **Lanczos-3 resampling** — `RenderOptions { resampling: Resampling::Lanczos3, .. }` applies a
  two-pass separable 6-tap Lanczos kernel after rendering; sharper thumbnails at the cost of ~5×
  render time vs `Bilinear`
- **Grayscale output** — `djvu_render::render_gray8` returns a `GrayPixmap` (1 byte/pixel);
  CLI: `djvu render --gray`
- **Permissive render mode** — `RenderOptions::permissive = true` skips corrupted or unsupported
  chunks instead of returning an error; useful for broken files in the wild
- **Benchmark suite** — `benches/render.rs` gains `render_scaled` (Bilinear vs Lanczos-3) and
  `pdf_export` benchmarks; `BENCHMARKS.md` documents results on Apple M1 Max and comparison vs
  DjVuLibre 3.5.29
- **Benchmark CI** — `.github/workflows/bench.yml` runs `cargo bench` on `ubuntu-latest` and
  `macos-latest` on every release tag; Criterion HTML reports uploaded as artifacts

### Performance

- **SIMD YCbCr→RGB** — `Iw44Image::to_rgb` now processes 8 pixels per iteration using
  `wide::i32x8` (maps to AVX2 on x86_64, NEON on ARM64, scalar fallback elsewhere); eliminates
  per-pixel overhead in the full-resolution color conversion hot path
- **Packed bitwise mask dilation** — `Bitmap::dilate` uses bitwise OR on packed `u64` words
  instead of per-pixel loops; 2–4× faster for bold-text rendering
- **Composite optimisation** — eliminated redundant mask sampling in the 3-layer composite loop

### Fixed

- **Permissive mode robustness** — decode pipeline no longer panics on documents with missing or
  truncated BG44/FG44 chunks when `permissive = true`

## [0.2.1] — 2026-04-04

### Fixed

- Exclude `.cargo/config.toml` from published package — it contained `-D warnings` which caused
  docs.rs builds to fail silently

## [0.2.0] — 2026-04-04

### Added

- **Structural PDF export** — `djvu render --format pdf` now produces searchable PDFs with selectable
  text (from TXTz/TXTa), bookmarks (NAVM → PDF outline), and hyperlinks (ANTz → PDF link annotations)
- **Mask / layer extraction API** — `DjVuPage::extract_mask()`, `extract_foreground()`,
  `extract_background()`; CLI: `djvu render --layer mask|fg|bg`
- **`RenderOptions::fit_to_width` / `fit_to_height` / `fit_to_box`** — aspect-preserving smart scaling
  helpers that respect page rotation
- **User-controllable rotation** — `RenderOptions::rotation` overrides the INFO chunk value

### Fixed

- **Gamma correction** — gamma LUT now applied in all render paths (`render_pixmap`, `render_coarse`,
  `render_progressive`, rotation branches)
- **Page rotation** — `render_pixmap` and `render_coarse` now apply the rotation from the INFO chunk;
  output dimensions swap correctly for 90°/270° pages
- **FGbz multi-color foreground** — per-glyph blit index is now used when compositing; documents with
  multi-color foreground (stamps, colored annotations) render correctly

### Performance

- **Area-averaging downscale** — render at scale < 1.0 now uses box-filter averaging instead of
  bilinear; better anti-aliasing and ~2× faster for thumbnail/overview sizes
- **Composite optimisation** — eliminated redundant mask sampling in 3-layer composite loop

### Refactored

- **Removed `ouroboros` dependency** — `Document` is now a fully owned struct; no self-referential
  proc-macro required; `lib.rs` is now truly `unsafe`-free

## [0.1.0] — 2026-04-04

### Added

- **IFF container parser** — zero-copy, borrowing slices from input (`iff::parse_form`)
- **JB2 bilevel image decoder** — ZP adaptive arithmetic coding with symbol dictionary (`jb2_new`)
- **IW44 wavelet image decoder** — planar YCbCr storage, progressive multi-chunk refinement (`iw44_new`)
- **BZZ decompressor** — ZP + MTF + BWT for DIRM, NAVM, ANTz chunks (`bzz_new`)
- **Text layer extraction** — TXTz/TXTa chunk parsing with full zone hierarchy (`text`)
- **Annotation parsing** — ANTz/ANTa chunks: hyperlinks, map areas, background color (`annotation`)
- **Bookmarks** — NAVM table-of-contents parsing (`DjVuDocument::bookmarks`)
- **Multi-page documents** — DJVM bundle format with DIRM directory chunk
- **Page rendering** — composite foreground mask + background wavelet into RGBA output
- **Progressive rendering** — incremental BG44 wavelet refinement (`Page::render_scaled_progressive`)
- **Thumbnails** — TH44 embedded thumbnail extraction (`Page::thumbnail`)
- **High-level API** — `Document` / `Page` (requires `std` feature)
- **New document model** — `DjVuDocument` / `DjVuPage` built on clean-room codecs
- **CLI tool** — `djvu info`, `djvu render --format png|pdf|cbz`, `djvu text` subcommands
- **Rasterized PDF export** — `djvu render --format pdf` embeds each page as an RGB image (FlateDecode)
- **CBZ export** — `djvu render --format cbz` produces a comic-book ZIP with PNG pages
- **`no_std` support** — IFF/BZZ/JB2/IW44/ZP modules work with `alloc` only

[Unreleased]: https://github.com/matyushkin/djvu-rs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/matyushkin/djvu-rs/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/matyushkin/djvu-rs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/matyushkin/djvu-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/matyushkin/djvu-rs/releases/tag/v0.1.0
