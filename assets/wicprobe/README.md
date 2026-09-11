# AVIF colour probes

Seven ~360-byte AVIF files that ship inside the binary. `src/decode/wicprobe.rs` decodes them
through Windows' own AV1 codec on the first AVIF of each process and compares the result with
what they are known to contain, so the decision "can WIC be trusted with this file's colour"
is a measurement of the codec that is installed rather than a table someone wrote down once.

Read `src/decode/wicprobe.rs` for the history. The short version: the table it replaced was
measured against AV1 Video Extension 2.0.24.0, Microsoft shipped 2.0.30.0, two rows changed,
and the fix for issue #9 quietly turned back into issue #9 for most AVIF on the web.

| file | what it pins |
|---|---|
| `avif-8bit-bt709.avif` | 8-bit, `nclx` matrix 1: ordinary web AVIF (Chrome, Squoosh) |
| `avif-8bit-bt601.avif` | 8-bit, `nclx` matrix 6: what `avifenc` writes unless told otherwise |
| `avif-8bit-nocolr.avif` | 8-bit with no `colr` box, so the decoder is guessing |
| `avif-10bit-bt709.avif` | 10-bit, `nclx` matrix 1 |
| `avif-10bit-bt601.avif` | 10-bit, `nclx` matrix 6 |
| `avif-10bit-mono.avif` | 10-bit monochrome: no chroma planes, so no matrix to misread |
| `avif-10bit-pq2020.avif` | 10-bit PQ / BT.2020: the HDR shape (issue #39), which the codec returns as linear floats |

Each is 32x32: four 16x16 flat patches whose values are duplicated as constants in
`wicprobe.rs`, encoded LOSSLESS in 4:4:4 so the encoder contributes no error of its own and the
grader can sample patch centres without a resampling artefact reaching the number. The PQ
probe is graded AFTER the HDR path (the codec's float hand-off, our rescale and tone map), so
its constants are tone-mapped sRGB - 188 for full white, not 255 - and `--verify` derives
them from the same arithmetic and prints them beside the dav1d check.

Regenerate (needs ffmpeg with libaom-av1 on PATH):

```
python scripts/make-wic-probes.py
python scripts/make-wic-probes.py --verify
```

**If you change a patch value, change it in `wicprobe.rs` too.** The constants there are what
the probes are graded against, and nothing checks that the two agree beyond `--verify`, which
decodes these files with libdav1d and asserts they still contain what the Rust side expects.
