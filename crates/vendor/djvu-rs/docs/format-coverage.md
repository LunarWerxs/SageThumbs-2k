# Format coverage

Chunk-level coverage of the DjVu v3 format, for readers who need to know
exactly what decodes and what encodes. The fresh-encode versus
existing-document mutation contract is expanded in
[`writer-coverage.md`](writer-coverage.md).

| Format element | Decode | Encode |
|----------------|--------|--------|
| IFF container (`FORM:DJVU`, `FORM:DJVM`) | ✓ zero-copy parser | ✓ |
| JB2 bilevel images (`Sjbz`), shared dictionaries (`Djbz` via `INCL`) | ✓ ZP arithmetic coding + symbol dictionary | ✓ incl. multi-page shared Djbz |
| IW44 wavelet images (`BG44` / `FG44`) | ✓ planar YCbCr, multiple refinement chunks | ✓ color and grayscale |
| G4/MMR fax images (`Smmr`, ITU-T T.6) | ✓ | ✓ (explicit `BilevelCodec::Smmr` / `--bilevel-codec smmr`) |
| JPEG background/foreground (`BGjp` / `FGjp`) | ✓ | — (encoder emits IW44) |
| Foreground palette (`FGbz`) | ✓ | ✓ (layered encoder) |
| BZZ compression (BWT + MTF + ZP) | ✓ | ✓ |
| Text layer (`TXTa` / `TXTz`), zone hierarchy down to characters | ✓ | ✓ (incl. OCR injection) |
| Annotations (`ANTa` / `ANTz`): hyperlinks, map areas, colors | ✓ | ✓ |
| Bookmarks (`NAVM`) | ✓ | ✓ |
| Multi-page directory (`DIRM`), bundled and indirect | ✓ | ✓ (DjVuLibre-clean directory v1) |
| Thumbnails (`TH44`) | ✓ | ✓ (`--thumbnails`) |
| Metadata (`ANTz` `(metadata …)`; legacy `METa` / `METz` read only) | ✓ | ✓ (`PageEncoder::with_metadata`; `set_metadata` on documents and pages) |
| Legacy standalone `FORM:BM44` / `FORM:PM44` files | ✓ | — |
| Unknown chunk IDs | preserved byte-exact for round-trip | n/a |

The codec internals are also published as standalone workspace crates for
focused consumers: [`djvu-iff`](../crates/djvu-iff), [`djvu-bzz`](../crates/djvu-bzz),
[`djvu-bitmap`](../crates/djvu-bitmap), [`djvu-jb2`](../crates/djvu-jb2),
[`djvu-pixmap`](../crates/djvu-pixmap), [`djvu-iw44`](../crates/djvu-iw44), and
[`djvu-zp`](../crates/djvu-zp). All of them (and the codec modules of the main
crate) are `no_std`-compatible with `alloc` only, and are continuously fuzzed
via in-tree libFuzzer targets and OSS-Fuzz project files.
