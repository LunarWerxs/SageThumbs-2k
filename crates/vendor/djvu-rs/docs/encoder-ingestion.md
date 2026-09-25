# Encoder ingestion and color-management policy

Issue [#694](https://github.com/matyushkin/djvu-rs/issues/694) tracks
normalizing archival image inputs into the internal RGBA [`Pixmap`] consumed by
segmentation and encoding. This document is the supported-input matrix; behaviour
is deterministic and tested.

## Internal representation

All raster ingest paths target **8-bit RGBA** (`djvu_rs::Pixmap`: width × height ×
4 bytes, row-major). Alpha is preserved unless an explicit compositing policy is
selected (`djvu encode --background <COLOR>`).

## PNG (slice 1)

| Source | Bit depth | Internal RGBA | Notes |
|--------|-----------|---------------|-------|
| Grayscale | 1/2/4/8 | R=G=B=sample, A=255 | Low bit depths expanded by the PNG decoder |
| Grayscale + alpha | 8 | R=G=B=gray, A=alpha | |
| RGB | 8 | R,G,B, A=255 | |
| RGBA | 8 | passthrough | |
| Palette (PLTE) | 1/2/4/8 index | palette RGB + alpha | `tRNS` per-entry alpha when present, else 255 |
| Grayscale | 16 | R=G=B=`sample>>8`, A=255 | Truncate high byte, no dithering |
| Grayscale + alpha | 16 | R=G=B=`sample>>8`, A=`sample>>8` | |
| RGB / RGBA | 16 | channels `>>8` | Big-endian samples |

### 16-bit down-conversion

Policy: [`DepthDownconversion::TruncateHighByte`](../src/ingest.rs) — each
16-bit channel keeps its **high byte** (equivalent to `u16 >> 8` for PNG's
big-endian layout). No dithering; identical results on all targets.

### Alpha at ingest

Default: [`AlphaCompositing::Preserve`](../src/ingest.rs). Partial transparency
is **not** flattened onto an undocumented background during file decode.

`djvu encode --background <COLOR>` (RRGGBB hex with optional `#`, or
`white`/`black`) selects [`AlphaCompositing::CompositeOnBackground`](../src/ingest.rs):
every non-opaque pixel is blended onto the solid background at decode time
with deterministic integer rounding — `out = (c·a + bg·(255 − a) + 127) / 255`
per channel — and its alpha becomes 255. The policy applies uniformly to PNG
and TIFF (after orientation); JPEG never carries alpha, and the bilevel TIFF
fast path never expands to RGBA, so both are unaffected.

### ICC / color profiles

Explicit policy: [`IccHandling`](../src/ingest.rs), CLI `djvu encode --icc
<ignore|reject>`. DjVu has no container for ICC profiles and ingest applies
no colour management, so a profile can never survive into the output; the
policy makes that explicit instead of silent.

- `ignore` (default): decode the pixel bytes as-is and drop the profile.
  No colour-space conversion; sRGB-ish bytes pass through.
- `reject`: fail with an error naming the source (PNG `iCCP` chunk, JPEG
  APP2 `ICC_PROFILE` segment, or TIFF InterColorProfile tag 34675) and the
  profile size. All ingest routes enforce it, including the bilevel TIFF
  fast path; for multipage TIFF every page is checked.

A `transform` mode would need a colour-management engine (LCMS-class) and
is out of scope for #694.

## JPEG (current)

| Source | Internal RGBA | Notes |
|--------|---------------|-------|
| RGB (baseline/progressive) | R,G,B, A=255 | via `zune-jpeg` |
| Grayscale | R=G=B=luma, A=255 | |
| CMYK / YCCK (Adobe APP14) | naive transform (below), A=255 | baseline and progressive; `zune-jpeg` handles the Adobe-inverted storage and the YCCK chroma transform, then applies the same profile-free `(255 − ink) · (255 − K) / 255` mix as CMYK TIFF |

## TIFF (slices 2–3, `tiff` feature)

| Source | Internal RGBA | Notes |
|--------|---------------|-------|
| Gray8, GrayA8, RGB8, RGBA8 | PNG-like expansion, A=255 unless present | any compression the `tiff` crate decodes (LZW, Deflate, PackBits, JPEG-in-TIFF) |
| Gray16, GrayA16, RGB16, RGBA16 | channels `>>8` | same [`DepthDownconversion::TruncateHighByte`](../src/ingest.rs) policy as PNG |
| CMYK8 / CMYK16 | naive transform (below), A=255 | 16-bit downconverted first |
| Bilevel (1-bit) WhiteIsZero / BlackIsZero | 0/255 gray, A=255 | uncompressed, PackBits, or CCITT G4 strips; raw strip reader (tiff 0.9 misdecodes sub-byte samples). G4 reuses the [`smmr`](../src/smmr.rs) T.6 decoder, one independent stream per strip; BlackIsZero G4 renders inverted (libtiff-compatible) |
| Gray 2/4-bit | linear scale to 0..255 | uncompressed or PackBits strips |
| Palette 1/2/4/8-bit | ColorMap 16-bit entries `>>8` | uncompressed or PackBits strips |
| Multipage | one `Pixmap` per IFD, stored order | [`decode_tiff_file_to_pixmaps`](../src/png_io.rs); CLI `encode` maps a multipage file to a multi-page bundle |
| YCbCr, planar, tiled bilevel/palette, CCITT RLE/G3, LZW/Deflate bilevel, T6Options uncompressed mode, FillOrder 2 | — | targeted error naming the limitation |

### CMYK → RGB transform

Deterministic, profile-free: `channel = (255 − ink) · (255 − K) / 255` per
channel, alpha 255. No ICC transform is applied (see below); archival CMYK
with embedded profiles renders approximately.

### Multipage TIFF in the CLI

`djvu encode book.tif -o book.djvu` with a multipage file produces a
multi-page bundle under the same rules as a directory input: `--quality auto`
classifies every page (all bilevel → lossless JB2 bundle, else layered),
`--shared-dict-pages` and `--thumbnails` apply, `--bilevel-codec smmr` is
rejected. Page order is stored IFD order. Without an explicit `--dpi`, the
first page's X/YResolution + ResolutionUnit tags set the INFO dpi
(XResolution preferred, cm converted to inch, sane range 25–6000); missing
or unusable tags fall back to 300. An explicit `--dpi` always wins.

### Bilevel TIFF fast path

With `--quality lossless` or `--quality auto`, a TIFF whose pages are all
1-bit single-sample grayscale decodes straight to packed JB2 masks via
[`decode_tiff_file_to_bitmaps`](../src/png_io.rs) — no RGBA expansion (32×
the memory) and no segmentation pass. The masks are identical to what the
default fixed-threshold segmentation produces from the RGBA route, so the
output bytes do not change. Under `--quality auto`, 1-bit pages are bilevel
by construction and resolve to Lossless without pixel statistics (this also
routes blank 1-bit pages to Lossless; the sampling classifier used to send
them layered). Any non-1-bit page falls the whole file back to the RGBA
route; explicit layered profiles (`quality`/`archival`/`photo`) keep the
RGBA route too.

## Orientation

The TIFF Orientation tag (274, values 1–8) is applied **exactly once**, at
ingest, per page — the `tiff` crate itself never applies it. Both the RGBA
route and the bilevel fast path rotate/mirror identically; orientations 5–8
swap page dimensions and make the stored YResolution the visual horizontal
density for the DPI mapping. Out-of-range values are treated as upright
(libtiff-compatible).

JPEG EXIF orientation (the same tag 274, inherited from TIFF, inside the
APP1 `Exif` segment) is applied **exactly once** too, with the same 1–8
mapping — `zune-jpeg` exposes the raw EXIF block and never applies it. Both
EXIF byte orders (`II`/`MM`) are parsed; a malformed EXIF block, a wrong
entry type, or an out-of-range value means upright. PNG has no standard
orientation metadata.

## Planned follow-ups (#694)

None — the initial target coverage is complete. An ICC `transform` mode
(actual colour management) is explicitly out of scope; see above.
