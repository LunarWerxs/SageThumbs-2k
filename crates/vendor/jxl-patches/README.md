# SageThumbs 2K patch to `jxl-render` / `jxl-oxide`: 1:8 LF-only rendering

**Upstream: <https://github.com/tirr-c/jxl-oxide/pull/505>, tracking issue
<https://github.com/tirr-c/jxl-oxide/issues/78>.
Delete all of this the moment a release carries it** (the two `[patch.crates-io]` lines in the
workspace `Cargo.toml`, `crates/vendor/jxl-render`, `crates/vendor/jxl-oxide`, this directory,
and `scripts/vendor-jxl.ps1`). The call site in `decode/tiers.rs` does not change when it goes.

## Why

JPEG XL was the slowest format in the product by a wide margin: a 12 MP `.jxl` took **~1.7 s**
to thumbnail, against ~30 ms for the same picture as a JPEG. The reason is structural rather
than a slow decoder. A thumbnail cost a FULL-RESOLUTION decode, and **the cost is in pixels,
not bytes** - the corpus sample is 50 KB - so no file-size gate can ever catch it. That is
exactly the case JPEG XL exists to serve, which is why it cannot be waved away as exotic.

Every other big format already avoids this: JPEG has DCT-scaled decoding, AVIF/HEIC have the
OS codec's own reduced-size path, DDS has its mip chain. JPEG XL has the same thing available
and jxl-oxide simply had no way to ask for it: a VarDCT frame codes a complete 8x-downsampled
picture (the LF image) ahead of the HF coefficients, and the decoder already builds it -
dequantized, chroma-from-luma corrected, adaptively smoothed - before any inverse DCT runs.
Stopping there skips essentially the whole decode.

Measured on the 12 MP corpus sample, in the product, `st2k bench-decode --size 256`:

| | before | after |
| --- | --- | --- |
| `jxl-large.jxl` (4000x3000) | 1732.6 ms | **24.3 ms** |

against a full decode downscaled to the same tile: RMSE 1.9%, MAE 1.4%, i.e. under four levels
out of 255, and visually indistinguishable.

## What the patch does

- `RenderContext::request_lf_only` / `JxlImage::set_lf_only` opt into the mode.
- `render_vardct` returns the LF image directly and never allocates the full-resolution
  framebuffer (three f32 planes, ~264 MB on a 66 MP frame).
- `render.rs` returns before the restoration filters, feature rendering and upsampling, all of
  which assume 1:1 geometry. **This is why the mode is an approximation and opt-in.**
- The one 1:1 stage it does NOT skip: a `do_ycbcr` frame's chroma planes are upsampled to the
  LF luma before the LF image is returned (`upsample_jpeg` against the LF region). A
  JPEG-transcoded frame keeps the JPEG's subsampling, so its Cb and Cr LF planes are half-size,
  and the YCbCr conversion downstream requires three planes of one size. Missing until
  2026-09-16 (issue #43): every 4:2:0 or 4:2:2 transcode - which is every phone photo run
  through cjxl - panicked with "Grid size mismatch" on the reduced path, while 4:4:4 and
  non-JPEG files were fine. The 39 conformance files carry no subsampled JPEG reconstruction,
  which is why nothing went red. `tests/fixtures/jxl/jpeg4{20,22}_transcode.jxl` now do.
- `JxlImage::render_size` reports what a frame will actually produce, and the `image`-crate
  integration's `dimensions()` follows it, so `DynamicImage::from_decoder` allocates the right
  buffer.
- Extra channels have no 1:8 representation to decode, so alpha is filled opaque - the same
  answer jxl-oxide's existing incomplete-frame fallback gives.
- `jxl_render::lf_only_applies` is the ONE predicate for whether the mode applies, used by the
  renderer and by `render_size` alike, so the size that gets reported and the render that
  actually happens cannot disagree. **It refuses frames that do not cover the canvas exactly.**
  A frame may be cropped or letterboxed, and the reduction applies to the FRAME while every
  public dimension describes the IMAGE, so the two only agree when the frame is the whole
  canvas. Added 2026-08-20 from upstream review; no conformance file and no corpus sample
  triggers it, which is precisely why it had to be caught by reading rather than by a red test.
- **Patches are stamped onto the LF image; splines refuse the mode** (2026-09-30). Both are
  drawn onto the frame after the VarDCT decode, so the LF image never held them: the
  conformance `patches` file and any cjxl encode of text thumbnailed with nearly all its text
  missing. libjxl codes text glyphs as ADDITIVE patches (subtracted from the picture first),
  and each LF pixel is the mean of an 8x8 block, so `blend::patch_lf` adds every patch pixel
  divided by 64 to the LF pixel it lands in: exactly the LF image of the patched frame. Any
  other blend mode, a YCbCr frame, or a reference that was itself rendered at 1:8 returns
  `NotSupported`, and `decode_jxl` retries those with a full decode. Refusing patches outright
  was tried first and measured: every photo with text in it lost the 1:8 path (conformance
  `bike` 109 ms -> 1215 ms, `progressive` 146 ms -> 2939 ms). Splines are never written by
  cjxl, so they simply refuse. Noise is still skipped; it is grain.
  `tests/fixtures/jxl/text-patches.jxl` pins both the text and the 1:8 size.

**Modular (lossless) frames have no LF image and are unaffected.** They ignore the request and
render at 1:1, which is why `decode_jxl` reads the size back rather than assuming.

## Maintaining it

**The patch files are the source of truth. The vendored copies are GENERATED.** Do not edit
`crates/vendor/jxl-render` or `crates/vendor/jxl-oxide` by hand: the next run of the script
overwrites them and the change disappears with nothing to say so.

```powershell
pwsh scripts\vendor-jxl.ps1            # regenerate at the pinned versions
pwsh scripts\vendor-jxl.ps1 -Check     # verify the committed tree is exactly pristine + patches
pwsh scripts\vendor-jxl.ps1 -Render 0.12.5 -Oxide 0.12.7    # try a newer release
```

Bumping a version is therefore: change the default in the script (or pass the flags), run it,
and either it applies cleanly or `git apply` names the exact hunks that no longer fit. That is
the whole point of doing it this way rather than re-applying edits from memory each time.

The vendored copies are still committed, deliberately, because cargo needs the path
dependencies present at build time and CI checks out the repo without running the script. They
are build inputs; the script is how they are produced.

**The pinned versions must match what `Cargo.lock` would resolve unpatched** (`jxl-render`
0.12.4, `jxl-oxide` 0.12.6). Patching a different codebase than the one that was tested is the
failure this pin exists to prevent.

## The fuzz fixes carried beside it

Three crates also carry panic-to-error fixes found by the deep fuzz sessions. Each exists because
a panic in the decoder aborts the thumbnail host (`panic = "abort"`), and each is dropped the day
an upstream release stops panicking on its regression fixture in `tests/fixtures/jxl/`.

| Patch | Panic | Now |
| --- | --- | --- |
| `jxl-frame.patch` (2026-09-23) | a TOC asked for a group past its entries (`toc.rs`, index out of bounds) | decode error |
| `jxl-frame.patch` (2026-10-02) | progressive passes that leave a downsampling level for no pass to decode (a pass named twice, levels out of order): `jxl-modular` 0.11.3 unwrapped its lookup of the pass for a channel at that level (`image.rs`, `prepare_groups`) | the frame is refused where its passes are read (`lib.rs`), so `jxl-modular` needs no copy here; an entry naming the final pass is skipped, as libjxl treats it, instead of emptying that pass's range |
| `jxl-color.patch` (2026-09-27) | an XYB or unknown colour space, an unknown transfer function or a zero gamma reached `todo!()`, `panic!()` and a division by zero in `colour_encoding_to_icc` | empty profile, which `ColorTransform::new` refuses with `UnsupportedColorEncoding` |
| `jxl-render.patch` (2026-09-27, beside the LF-only hunks) | an LF frame of level 4 indexed `lf_frame[4]` (a valid frame; the slot does not exist); adaptive LF smoothing ran on subsampled chroma planes and failed its equal-length assertion | the index is checked; smoothing with non-4:4:4 chroma is refused as malformed, as libjxl does |

`jxl-color` is pinned at 0.11.0 in `scripts/vendor-jxl.ps1` like the others.
