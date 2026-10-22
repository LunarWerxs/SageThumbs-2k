# djvu-rs for JavaScript

[![npm](https://img.shields.io/npm/v/djvu-rs)](https://www.npmjs.com/package/djvu-rs)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/matyushkin/djvu-rs/blob/main/LICENSE)

Open and render DjVu files in the browser or Node, with no server and no
native dependencies. WebAssembly build of the pure-Rust
[djvu-rs](https://github.com/matyushkin/djvu-rs) library: no DjVuLibre, no
GPL. Ships TypeScript declarations and a SIMD build that is picked at runtime
when the engine supports it.

| Your task | How |
|-----------|-----|
| Draw a page on a `<canvas>` | `page.render(dpi)` → `ImageData` |
| Open a large book from a URL, fetching only the pages shown | `WasmLazyDocument.open(size, fetchRange)` — [lazy loading](#lazy-loading-over-http) |
| Fast preview, then sharpen | `WasmRenderRequest` with `set_coarse()` / `set_step(n)` |
| Render only the visible viewport | `WasmRenderRequest` with `set_region(x, y, w, h)` |
| Zoomable tiled viewer | `page.render_tile(dpi, size, col, row)` + `tile_cols` / `tile_rows` |
| Extract text, or highlight words | `page.text()`, `page.text_zones_json(dpi)` (pixel boxes) |
| Convert to PDF, encode DjVu, OCR | Not in the JS package — use the [`djvu` CLI](https://github.com/matyushkin/djvu-rs/blob/main/docs/cli.md), [Python](https://pypi.org/project/djvu-rs/), or the Rust crate |

## Install

```sh
npm install djvu-rs
```

## Quick start

```js
import init, { WasmDocument } from 'djvu-rs';

await init();                                   // loads the SIMD or scalar build

const bytes = new Uint8Array(await (await fetch('book.djvu')).arrayBuffer());
const doc = WasmDocument.from_bytes(bytes);
console.log(doc.page_count());

const page = doc.page(0);
const dpi = 150;
const pixels = page.render(dpi);                // Uint8ClampedArray, RGBA
const canvas = document.querySelector('canvas');
canvas.width = page.width_at(dpi);
canvas.height = page.height_at(dpi);
canvas.getContext('2d').putImageData(new ImageData(pixels, canvas.width, canvas.height), 0, 0);

const text = page.text();                       // undefined if the page has no text layer
```

In Node, read the wasm file yourself and pass its bytes to `init`, because
Node's `fetch` cannot load a local file:

```js
import { readFileSync } from 'node:fs';
import init, { wasmSimd128Supported } from 'djvu-rs';

const dir = new URL('.', import.meta.resolve('djvu-rs'));
const variant = wasmSimd128Supported() ? 'simd128' : 'scalar';
await init(readFileSync(new URL(`${variant}/djvu_rs_bg.wasm`, dir)));
```

## Viewer rendering

A `WasmRenderRequest` sets the DPI, an optional region, and the quality. It
renders into a reusable `WasmPixmap`, so the pixels are not copied out of wasm:

```js
import { WasmPixmap, WasmRenderRequest } from 'djvu-rs';

const out = new WasmPixmap();

const preview = new WasmRenderRequest(150);
preview.set_coarse();                           // fast, blurry first frame
page.render_request(preview, out);

const viewport = new WasmRenderRequest(150);
viewport.set_region(0, 400, 1200, 800);         // only what is on screen
page.render_request(viewport, out);
ctx.putImageData(new ImageData(out.view(), out.width(), out.height()), 0, 400);
```

`out.view()` is a live view into wasm memory: draw it before the next render,
or copy it with `out.to_bytes()`. `set_step(n)` renders background chunks
`0..=n` (below `page.bg44_chunk_count()`) for blurry-to-sharp loading.

## Lazy loading over HTTP

`WasmLazyDocument` opens a book from about 64 KiB of the file, then fetches
each page's bytes when you render it. Any server that answers HTTP `Range`
requests works, including static hosting and CDNs. On a 25 MiB, 520-page book,
page 1 renders after downloading 0.25% of the file.

```js
import { WasmLazyDocument } from 'djvu-rs';

const url = 'https://example.org/book.djvu';
const size = Number((await fetch(url, { method: 'HEAD' })).headers.get('Content-Length'));

const doc = await WasmLazyDocument.open(size, async (offset, len) => {
  const r = await fetch(url, { headers: { Range: `bytes=${offset}-${offset + len - 1}` } });
  return new Uint8Array(await r.arrayBuffer());
});
const pm = await doc.render_page(0, 150);       // fetches only page 1
ctx.putImageData(new ImageData(pm.view(), pm.width(), pm.height()), 0, 0);
```

`render_page_progressive(i, dpi, n)` draws a blurry-to-sharp first frame.
For an indirect document (one file per page), `WasmLazyIndirectDocument.open(indexBytes, resolve)`
calls `resolve(name, kind)` once per page file it needs — see
[`examples/wasm`](https://github.com/matyushkin/djvu-rs/tree/main/examples/wasm#indirect-documents).

## Status & limitations

- **Reading and rendering only.** No export, encoding, editing, or OCR in
  the JS package.
- **`init()` is async only;** `initSync()` throws, because the build is
  chosen at runtime.

## Documentation

| Topic | Where |
|-------|-------|
| Exact signatures | `djvu_rs.d.ts` in this package |
| Drag-and-drop demo, lazy-loading benchmark, threaded build | [`examples/wasm`](https://github.com/matyushkin/djvu-rs/tree/main/examples/wasm) |
| The Rust library, CLI, and Python bindings | [djvu-rs README](https://github.com/matyushkin/djvu-rs#readme) |
| Release and versioning contract | [`docs/packaging.md`](https://github.com/matyushkin/djvu-rs/blob/main/docs/packaging.md) |

MIT licensed. Written from the public DjVu v3 specification. All Rust
dependencies compiled into the `.wasm` are permissively licensed (MIT,
Apache-2.0, Zlib, Unicode-3.0, or similar; no GPL/LGPL); their license texts
are in `THIRD_PARTY_NOTICES` in this package.
