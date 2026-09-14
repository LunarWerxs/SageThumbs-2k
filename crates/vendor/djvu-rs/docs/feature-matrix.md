# Feature matrix — supported combinations and targets

This is the explicit, checked-in matrix referenced by
[`api-compatibility.md`](api-compatibility.md) §4. It enumerates the feature
combinations and build targets djvu-rs supports, and which CI gate keeps each
one green. The machine-checked list of combinations lives in
`.github/workflows/api-stability.yml` (the `feature-matrix` job) — this document
is its human-readable companion and must stay in sync with it.

## Targets

| Target | Purpose | Required gate |
|--------|---------|---------------|
| `x86_64` host (Linux) | primary dev/CI target | `Lint`, `Test (stable)` |
| `aarch64` (Linux) | support coverage | `Linux aarch64 smoke` |
| `x86_64-pc-windows-msvc` | Windows support | `Windows MSVC smoke` |
| `wasm32-unknown-unknown` | browser / no_std link test | `wasm32 build check` (required) |

## Feature combinations

"Build" = `cargo check`/`build` must succeed. "Test" = the test suite runs.
Combinations are chosen to cover every documented consumer profile without a
full power-set explosion (which is intractable for ~20 features).

| Combination | Meaning | Checked by |
|-------------|---------|------------|
| `--no-default-features` | `no_std` + `alloc` codec-only (host) | `Test (stable)` → *Build (no_std check)* |
| `--no-default-features` (wasm32) | strict `no_std` link test | `wasm32 build check` |
| *(default)* `std` | decode-only surface, no writer deps | `Lint`, `Test (stable)`, feature-hygiene |
| `std,jpeg` | standalone JPEG decode | feature-matrix job |
| `pdf` | PDF writer (+ owns miniz_oxide/jpeg-encoder) | `Lint (cli+epub)`, feature-matrix job |
| `epub` | EPUB writer (+ owns zip/flate2) | `Lint (cli+epub)`, feature-matrix job |
| `cbz` | CBZ writer | feature-matrix job |
| `tiff` | TIFF writer | feature-matrix job |
| `cli` (= `pdf,cbz` + clap) | the `djvu` binary | `Test (stable)`, Windows/aarch64 smoke |
| `async` | async render + lazy loading | feature-matrix job, README doctests |
| `parallel` | rayon multi-page render | `Test (parallel feature)` |
| `mmap` | memory-mapped I/O | feature-matrix job |
| `serde` | Serialize/Deserialize for public data types | feature-matrix job, `serde_roundtrip` test |
| `image` | `image::ImageDecoder` integration | feature-matrix job, `image_decoder` test |
| `wasm` (wasm32) | wasm-bindgen bindings | `wasm32 build check` |
| `wasm-lazy` (wasm32) | lazy Range-based browser open | `wasm32 build check` |
| `wasm` + `+simd128` (wasm32) | simd128 IW44 kernels | `wasm32 build check` |
| `cli,tiff,async,serde,image,epub` | README doctest union | `README doctests` (required, in `Test (stable)`) |
| `ocr-tesseract` | supported OCR backend | `OCR (tesseract)` (main-only) |

### Combinations deliberately outside the required gates

| Combination | Why it is opt-in |
|-------------|------------------|
| `wasm-threads` (wasm32) | needs nightly + `-Z build-std`; checked by `make wasm-threads-check` |
| `experimental`, `iw44-probe`, `alloc-profile` | dev/experimental surfaces (see §1 of the policy) |
| `ocr-onnx`, `ocr-neural` | experimental / placeholder OCR (out of scope for #695); `ocr-onnx` unit + model-gated tests run in the main-only `OCR (onnx models)` job (#693) |

## Feature-graph invariants

1. **Decode-only default tree (#509).** The default (`std`) feature must not
   enable any writer/encoder dependency (zip, zopfli, jpeg-encoder, clap,
   serde_json). Enforced by `scripts/check_feature_hygiene.sh` in the required
   `Lint` gate.
2. **Writers own their deps.** `pdf` owns `miniz_oxide`+`jpeg-encoder`; `epub`
   /`cbz` own `zip`+`flate2`; `cli` layers `clap`+`serde_json` on top. No
   writer dependency is reachable without its feature.
3. **`std`-implied features.** `pdf`, `epub`, `cbz`, `tiff`, `async`,
   `parallel`, `mmap`, `image`, `wasm`, and all `ocr-*` features imply `std`.
   The `no_std` surface is exactly the codec primitives.
4. **Additivity.** Enabling more features never removes or breaks an item that
   built with fewer. Any two documented features must compose.

## Reproducing the matrix locally

```sh
# no_std (host + wasm32) and default decode-only tree — part of `make check`
make check

# a representative slice of the feature-combination job:
cargo check --no-default-features
cargo check                                   # default std
cargo check --features pdf
cargo check --features epub
cargo check --features cli
cargo check --features async
cargo check --features serde
cargo check --features image
cargo check --features tiff
cargo check --features mmap
cargo check --features parallel
cargo check --features std,jpeg

# wasm targets
cargo check --target wasm32-unknown-unknown --features wasm
cargo check --target wasm32-unknown-unknown --features wasm-lazy
cargo build --no-default-features --target wasm32-unknown-unknown
```
