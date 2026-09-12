"""Regenerate the AVIF colour probes in `assets/wicprobe/`.

    python scripts/make-wic-probes.py            # rewrite the probes
    python scripts/make-wic-probes.py --verify   # decode them and check they still say what
                                                 # `decode/wicprobe.rs` expects (no rewrite)

These are the files `src/decode/wicprobe.rs` compiles into the binary to measure what
Microsoft's AV1 WIC codec does with colour on the machine it is running on. See that module for
why a measured answer replaced a hand-written table (issue #9, twice).

Shape, and why each part of it is load-bearing:

  * 32x32, four 16x16 flat patches. Flat so the grader can sample patch CENTRES and be immune
    to any resampling; four because that is enough to separate the three failure modes we have
    seen (a YUV matrix error moves saturated colour and leaves grey alone, a range error moves
    grey, a transfer error moves everything by a curve).
  * LOSSLESS 4:4:4. The probe must measure the DECODER, so the encoder is not allowed to
    contribute error. 4:2:0 would blur chroma across the patch boundaries and 4:2:2 would blur
    it horizontally; both would put the encoder inside the measurement.
  * One file per colour-signalling class Windows has ever treated differently. BT.709 and
    BT.601 have swapped places between extension versions, so they must stay separate probes
    however similar they look.
  * The no-`colr` probe is made by RENAMING the box in the BT.709 file rather than re-encoding
    without one, so it is the same pixels with the signal removed and nothing else varies.
  * The PQ probe (issue #39) is the one HDR shape: 10-bit, BT.2020 primaries, SMPTE ST 2084
    transfer, the file an HDR-base AVIF is. The codec hands it back as linear floats rather
    than 8-bit sRGB, so what the Rust side grades is the float hand-off plus its own tone map,
    and the expected values are TONE-MAPPED sRGB, not the patches as encoded.

Needs ffmpeg with libaom-av1 (the encoder `avifenc` itself uses) on PATH. Verification decodes
with libdav1d, i.e. not the codec under test.
"""
import os
import subprocess
import sys

import numpy as np
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "assets", "wicprobe")
S = 16

# Must match EXPECT_COLOUR / EXPECT_MONO in src/decode/wicprobe.rs.
COLOUR = [(255, 0, 0), (0, 255, 0), (128, 128, 128), (222, 178, 145)]
MONO = [32, 96, 160, 224]

# The PQ probe's patches, in LINEAR light relative to a 203-nit diffuse white, BT.709
# primaries: the same red, green and skin as COLOUR, with the grey at half of white so the
# tone map has a curve to be wrong about. The file carries them PQ-encoded in BT.2020 (see
# `pq_chart`); what the Rust side expects back is EXPECT_PQ in src/decode/wicprobe.rs, which
# `--verify` derives and prints so the two can be compared by eye.
PQ_LINEAR = [(1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.5, 0.5, 0.5), None]  # None: skin, from COLOUR

CASES = [
    # file stem,          pix_fmt,       ffmpeg matrix name, strip the colr box?
    ("avif-8bit-bt709",   "yuv444p",     "bt709",            False),
    ("avif-8bit-bt601",   "yuv444p",     "smpte170m",        False),
    ("avif-8bit-nocolr",  "yuv444p",     "bt709",            True),
    ("avif-10bit-bt709",  "yuv444p10le", "bt709",            False),
    ("avif-10bit-bt601",  "yuv444p10le", "smpte170m",        False),
    ("avif-10bit-mono",   "gray10le",    "bt709",            False),
    ("avif-10bit-pq2020", "yuv444p10le", "bt2020nc",         False),
    ("avif-10bit-nocolr", "yuv444p10le", "bt709",            True),
]

# BT.709 linear -> BT.2020 linear, the inverse of `primaries_to_bt709` in src/decode/cicp.rs.
M709_2020 = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]]


def srgb_eotf(v):
    return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4


def srgb_oetf(l):
    l = min(max(l, 0.0), 1.0)
    return 12.92 * l if l <= 0.0031308 else 1.055 * l ** (1 / 2.4) - 0.055


def pq_oetf(nits):
    y = max(nits, 0.0) / 10000.0
    m1, m2, c1, c2, c3 = 0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875
    yp = y ** m1
    return ((c1 + c2 * yp) / (1 + c3 * yp)) ** m2


def pq_linear_patches():
    skin = tuple(srgb_eotf(v / 255.0) for v in COLOUR[3])
    return [p if p is not None else skin for p in PQ_LINEAR]


def pq_signal_patches():
    """Each PQ patch as the file carries it: BT.2020 primaries, PQ-encoded, in [0, 1]."""
    out = []
    for r, g, b in pq_linear_patches():
        lin2020 = [sum(M709_2020[i][j] * v for j, v in enumerate((r, g, b))) for i in range(3)]
        out.append(tuple(pq_oetf(v * 203.0) for v in lin2020))
    return out


def tone_mapped_patches():
    """What `tone_map_float` in src/decode/color.rs makes of each PQ patch: Reinhard, then the
    sRGB curve, then `(x * 255 + 0.5)` floored - the same arithmetic, so EXPECT_PQ is derived
    here rather than typed from a calculator."""
    def tone(c):
        c = max(c, 0.0)
        t = c / (1.0 + c)
        return max(0, min(255, int(srgb_oetf(t) * 255.0 + 0.5)))
    return [tuple(tone(c) for c in p) for p in pq_linear_patches()]


def run(cmd):
    return subprocess.run(cmd, capture_output=True, text=True, timeout=600)


def write_charts(tmp):
    colour = np.zeros((S * 2, S * 2, 3), dtype=np.uint8)
    for i, rgb in enumerate(COLOUR):
        r, c = divmod(i, 2)
        colour[r * S:(r + 1) * S, c * S:(c + 1) * S] = rgb
    Image.fromarray(colour).save(os.path.join(tmp, "chart.png"))
    gray = np.zeros((S * 2, S * 2), dtype=np.uint8)
    for i, v in enumerate(MONO):
        r, c = divmod(i, 2)
        gray[r * S:(r + 1) * S, c * S:(c + 1) * S] = v
    Image.fromarray(gray, mode="L").save(os.path.join(tmp, "gray.png"))
    # PIL writes no 16-bit RGB PNG, so the PQ chart is raw rgb48le for ffmpeg's rawvideo demuxer.
    pq = np.zeros((S * 2, S * 2, 3), dtype="<u2")
    for i, rgb in enumerate(pq_signal_patches()):
        r, c = divmod(i, 2)
        pq[r * S:(r + 1) * S, c * S:(c + 1) * S] = [int(round(v * 65535)) for v in rgb]
    pq.tofile(os.path.join(tmp, "chart-pq.rgb48"))


def encode(stem, src, dst, pix_fmt, matrix, strip):
    cmd = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error"]
    if "pq" in stem:
        cmd += ["-f", "rawvideo", "-pix_fmt", "rgb48le", "-s", f"{S * 2}x{S * 2}"]
        primaries, trc = "bt2020", "smpte2084"
    else:
        primaries, trc = "bt709", "iec61966-2-1"
    cmd += ["-i", src]
    if not pix_fmt.startswith("gray"):
        # Convert RGB -> YUV with the matrix we are about to declare, so pixels and tag agree.
        cmd += ["-vf", f"scale=out_color_matrix={matrix}:out_range=pc"]
    cmd += ["-c:v", "libaom-av1", "-still-picture", "1", "-cpu-used", "0",
            "-aom-params", "lossless=1", "-pix_fmt", pix_fmt,
            "-color_primaries", primaries, "-color_trc", trc,
            "-colorspace", matrix, "-color_range", "pc", "-f", "avif", dst]
    r = run(cmd)
    if r.returncode != 0:
        raise SystemExit(f"encode failed for {dst}:\n{r.stderr}")
    if strip:
        # Rename rather than remove: every later box keeps its offset, so an ISOBMFF reader
        # cannot fail for a reason that has nothing to do with the missing colour signal.
        b = bytearray(open(dst, "rb").read())
        i = b.find(b"colr")
        if i < 0:
            raise SystemExit(f"{dst} has no colr box to strip")
        b[i:i + 4] = b"xxxx"
        open(dst, "wb").write(bytes(b))


def patches(path):
    a = np.asarray(Image.open(path).convert("RGB")).astype(np.float64)
    out = []
    for i in range(4):
        r, c = divmod(i, 2)
        out.append(a[r * S + 4:r * S + 12, c * S + 4:c * S + 12].reshape(-1, 3).mean(axis=0))
    return np.array(out)


def expected_from_reference_decoder(stem):
    """What libdav1d, which applies no transfer, must return for a probe: the patches as
    encoded. For the PQ probe that is the raw PQ signal on an 8-bit scale, NOT EXPECT_PQ."""
    if "mono" in stem:
        return np.array([[v] * 3 for v in MONO], dtype=np.float64)
    if "pq" in stem:
        return np.array([[v * 255.0 for v in p] for p in pq_signal_patches()])
    return np.array(COLOUR, dtype=np.float64)


def verify(tmp):
    ok = True
    for stem, _, _, _ in CASES:
        p = os.path.join(OUT, stem + ".avif")
        if not os.path.exists(p):
            print(f"MISSING {stem}.avif")
            ok = False
            continue
        png = os.path.join(tmp, stem + ".png")
        r = run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", p,
                 "-pix_fmt", "rgb24", "-frames:v", "1", png])
        if r.returncode != 0 or not os.path.exists(png):
            print(f"UNDECODABLE {stem}.avif")
            ok = False
            continue
        worst = float(np.abs(patches(png) - expected_from_reference_decoder(stem)).max())
        # The Rust side's TOLERANCE is 4; a probe whose own reference decode is not comfortably
        # inside that has no room left to measure the codec with.
        flag = "ok " if worst <= 2.0 else "BAD"
        if worst > 2.0:
            ok = False
        print(f"  {flag} {stem:<20} dav1d worst error {worst:5.1f}  "
              f"{os.path.getsize(p):>4} bytes")
    print("  EXPECT_PQ in src/decode/wicprobe.rs must read:",
          [list(p) for p in tone_mapped_patches()])
    print("probes verified" if ok else "PROBES FAILED VERIFICATION")
    return 0 if ok else 1


def main():
    os.makedirs(OUT, exist_ok=True)
    tmp = os.path.join(OUT, "_tmp")
    os.makedirs(tmp, exist_ok=True)
    try:
        if "--verify" not in sys.argv:
            write_charts(tmp)
            for stem, pix_fmt, matrix, strip in CASES:
                if "pq" in stem:
                    src = os.path.join(tmp, "chart-pq.rgb48")
                else:
                    src = os.path.join(tmp, "gray.png" if "mono" in stem else "chart.png")
                dst = os.path.join(OUT, stem + ".avif")
                encode(stem, src, dst, pix_fmt, matrix, strip)
                print(f"  wrote {stem}.avif  {os.path.getsize(dst)} bytes")
        return verify(tmp)
    finally:
        for f in os.listdir(tmp):
            os.remove(os.path.join(tmp, f))
        os.rmdir(tmp)


if __name__ == "__main__":
    raise SystemExit(main())
