# djvu-rs WASM demo

Browser-based DjVu viewer powered by djvu-rs compiled to WebAssembly.

## Build

```sh
# Install wasm-pack if you haven't already:
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

# From the repo root — builds pkg/ inside this directory:
make wasm

# Serve locally (any static file server works):
python3 -m http.server 8080 --directory examples/wasm
# Then open http://localhost:8080
```

## Binary size (measured frontier, #582)

The shipped release profile (fat LTO, `codegen-units = 1`) already produces a
compact ~405 KiB `djvu_rs_bg.wasm`. The measured alternatives are strictly
worse trades (see WASM_SIZE_DIET in `PERF_EXPERIMENTS.md`):

| profile | size | render speed |
|---------|------|--------------|
| release (shipped) | 414.6 KiB | baseline |
| `opt-level = "s"` | 372.7 KiB (−10.1%) | not measured (z is the floor) |
| `opt-level = "z"` | 371.6 KiB (−10.4%) | **~2.8× slower** full-page render |
| + `wasm-opt -Oz` | −0.3…−0.5 KiB extra | — |

`wasm-opt` finds almost nothing after fat LTO, and the encoder code is already
dead-code-eliminated from the viewer surface (the whole decode stack fits in
those 405 KiB). Don't switch profiles for size without re-measuring speed.

## Lazy HTTP Range loading (`wasm-lazy`, #588)

`WasmLazyDocument` opens a remote book from ~one 64 KiB block instead of the
whole file — page 1 of a 25 MiB / 520-page bundle renders after fetching 0.25%
of it (11–19× faster time-to-first-page at 12.5 MiB/s in Chrome; see
`bench_lazy_open.html`). Build the pkg with the feature, then hand `open` the
file length and a range-fetch callback:

```sh
wasm-pack build --target web --out-dir examples/wasm/pkg --release -- --features wasm-lazy
```

```js
const doc = await WasmLazyDocument.open(totalLen, async (offset, len) => {
  const r = await fetch(url, { headers: { Range: `bytes=${offset}-${offset + len - 1}` } });
  return new Uint8Array(await r.arrayBuffer());
});
const pm = await doc.render_page(0, 150);            // fetches just that page
const coarse = await doc.render_page_progressive(0, 150, 1); // blurry-to-sharp
```

Reproduce the benchmark: `python3 examples/wasm/serve_lazy_bench.py
--bandwidth-mib 12.5` (throttled Range server), then open
`http://localhost:8080/bench_lazy_open.html` and press Run.

For a hand-rolled reader instead of the binding, `range_lazy.md` shows the
`AsyncRead + AsyncSeek` integration shape for
`djvu_rs::djvu_async::from_async_reader_lazy_local`.

## npm package

```sh
# Publish to npm (requires npm login):
make wasm
cd examples/wasm/pkg
npm publish
```

`make wasm` emits a dual package: `pkg/djvu_rs.js` is the only entry point,
`pkg/scalar/djvu_rs_bg.wasm` is the fallback artifact, and
`pkg/simd128/djvu_rs_bg.wasm` is selected at runtime when
`WebAssembly.validate()` accepts the SIMD probe. Use `OUT=...` to write the
generated package elsewhere and `FEATURES=wasm-lazy` when publishing the lazy
Range-loading bindings.

## Threaded rendering (`wasm-threads`, opt-in, experimental)

`wasm-threads` layers a rayon Web Worker thread pool (via
[`wasm-bindgen-rayon`](https://docs.rs/wasm-bindgen-rayon)) over the existing
`parallel`-feature compositor/IDWT code paths, so they run multi-threaded in
the browser instead of single-threaded. It is **not** part of the default
`wasm` build and is **not MSRV/stable** — see `make wasm-threads-check` and the
WASM_THREADS entry in `PERF_EXPERIMENTS.md` for the full feasibility writeup,
including a measured result: it helped only marginally (or regressed sharply
on small/downscaled renders) on this codec's per-page workload, because
wasm Worker dispatch/sync overhead is much higher than native OS threads.
Treat this as infrastructure for further experiments, not a shipped win.

### One-time setup

```sh
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add wasm32-unknown-unknown --toolchain nightly
```

### Build

```sh
# Quick compile check:
make wasm-threads-check

# Full wasm-pack build (produces initThreadPool() + worker glue):
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory \
  -C link-arg=--shared-memory -C link-arg=--max-memory=1073741824 -C link-arg=--import-memory \
  -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
  -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base' \
RUSTUP_TOOLCHAIN=nightly \
wasm-pack build . --target web --release --out-dir examples/wasm/pkg-threads \
  -- -Z build-std=panic_abort,std --features wasm-threads
```

### Serve with COOP/COEP + directory-import fallback

The page needs `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` response headers (required for
`SharedArrayBuffer`/`initThreadPool` — without them it throws). Plain
`python3 -m http.server` can't send custom headers, and wasm-bindgen-rayon's
worker glue does a bundler-style bare-directory `import('../../..')` to reach
the main JS module, which plain browser module resolution can't follow either
— so also copy `djvu_rs.js` to `pkg-threads/index.js` as a fallback target and
serve directory URLs with an `index.js`-serving static server, e.g.:

```py
# minimal COOP/COEP + index.js-fallback server — see the snippet used to
# validate this in the WASM_THREADS journal entry for a drop-in version.
```

### JS usage

```js
import init, { WasmDocument, initThreadPool } from './pkg-threads/djvu_rs.js';
await init();
await initThreadPool(navigator.hardwareConcurrency); // spins up the Worker pool
const doc = WasmDocument.from_bytes(new Uint8Array(await (await fetch('book.djvu')).arrayBuffer()));
const pixels = doc.page(0).render(150);
```
