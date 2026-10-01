# rust_h264, vendored with a high bit depth patch

`crates/vendor/rust_h264` is the source of [roticv/rust_h264](https://github.com/roticv/rust_h264)
at trunk commit `c9987ca14829e2bec68c277ff0821321870de9ee` (2026-09-08, after 0.4.0, carrying
its fuzzer fix) with `rust_h264.patch` applied. Only `src/`, the licences and the README are
copied; upstream's `testdata/`, `fuzz/` and `examples/` are not.

It decodes the H.264 that Windows' own decoder cannot open, in the throwaway `st2k h264-frame`
child (`src/bin/vdec/h264.rs`, feature `h264-video`): above all **High 10**, the 10-bit 4:2:0
encode anime releases standardised on (issue #52). Upstream supports 8-bit only.

## What the patch changes

- **High bit depth, 8 to 14 bits, luma and chroma independently** (4:2:0 only). Samples are
  `u16` everywhere a pixel is stored or predicted, and the public `Frame` gains
  `bit_depth_luma` / `bit_depth_chroma`. Implemented from ITU-T H.264 FRExt: `QpBdOffset`
  (negative QPY, the mb_qp_delta range and wrap, QP' in dequantisation), the chroma QP mapping
  with `-QpBdOffsetC`, dequant for qP past 51, Clip1 at the stream's depth, `1 << (bd - 1)`
  for unavailable intra neighbours, I_PCM samples of `BitDepth` bits, deblocking thresholds
  scaled by `1 << (bd - 8)`, weighted-prediction offsets scaled the same way.
- **Refused by name** at activation: chroma formats other than 4:2:0 (High 4:2:2, High 4:4:4
  Predictive), separate colour planes, lossless transform bypass, bit depths past 14.
- **The aarch64 NEON paths are gone** (they were `u8`-only); the scalar code is the only path.
- **`Sps` keeps `video_full_range_flag` and `matrix_coefficients`** from the VUI (upstream
  skipped them) so the child converts to RGB in the stream's own colour space.
- **Two upstream 8-bit bugs fixed** on the way, both in CAVLC High profile with the 8x8
  transform and deblocking on (every upstream CAVLC 8x8 test stream is no-deblock, which is how
  they hid): the CAVLC path never recorded `mb_is_8x8dct`, so the deblocker filtered the inner
  4x4 edges an 8x8 transform does not have; and the bS=2 "has coefficients" test read each 4x4
  count, where with the 8x8 transform it is per 8x8 block (CAVLC spreads one 8x8 block's
  coefficients over its four 4x4 counts).
- **Two more spec details**: `second_chroma_qp_index_offset` is now used for Cr, and the 4:2:0
  chroma DC dequant is the exact spec formula (upstream rounded). Together they took frame 0 of
  an 8-bit x264 CAVLC stream from 584 wrong Cb samples to none.

## How it was checked (2026-10-01)

All 185 upstream tests still pass bit-exact, plus new ones, 200 in all: eight High 10 streams
written by x264 through ffmpeg 8.1.2 and compared sample-for-sample with ffmpeg's own 10-bit
decode (CABAC IBBP with 3 refs and weightp; CAVLC P; QP 1, so negative QPY and huge
coefficients, in CAVLC and CABAC; all-intra CABAC 8x8 and CAVLC 4x4; a fade with b-pyramid,
weightp and weightb; QP 60 with deblock 2:2), a QP wrap test, and the 8-bit CAVLC 8x8 deblocking
stream that exposed the two upstream bugs.

## Regenerating

Extract upstream at the commit above, `git apply rust_h264.patch`, copy `src/` here. The patch
is a plain `git diff` of `src/`.
