# Writer coverage

This note separates fresh-document encoding from mutation of an existing DjVu
document. The default writer policy remains unchanged.

| Surface | Fresh encode | Existing document |
|---|---|---|
| Bilevel mask | `PageEncoder` emits JB2 `Sjbz` by default | not rewritten by the page encoder |
| G4/MMR mask | `PageEncoder::with_bilevel_codec(BilevelCodec::Smmr)` emits a regular DjVuLibre-compatible `Smmr`; the CLI exposes the same choice with `--bilevel-codec smmr` for single-image input | not rewritten by the page encoder |
| Metadata | `PageEncoder::with_metadata` emits BZZ-compressed `METz`; empty metadata is omitted | `DjVuDocumentMut::page_mut(...).set_metadata` replaces or removes `METa`/`METz` while preserving other chunks |
| JPEG layers | not emitted; `BGjp` / `FGjp` remain decode-only | preserved as unknown or untouched chunks by the mutation model |

The Smmr option is explicit because it is an interoperability profile rather
than a general replacement for JB2. It is pixel-exact, but its simple
horizontal-mode encoder can be substantially larger on text pages. The
metadata APIs are separate so a caller choosing fresh encoding does not imply
permission to rewrite an existing document.

## Interoperability snapshot

On the `ccitt_2.djvu` raster rendered to 1728×2376 pixels on macOS arm64 with
Rust 1.92 and DjVuLibre 3.5.29:

| Profile | Output bytes | Mean encode time (5 runs) | DjVuLibre decode |
|---|---:|---:|---|
| Default JB2 | 8,530 | 38.0 ms | clean; pixel output identical |
| Explicit Smmr | 39,530 | 34.6 ms | clean; pixel output identical |

The full decision record is in `PERF_EXPERIMENTS.md` under
`WRITER_SMMR_METADATA (#685)`.
