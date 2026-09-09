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

CASES = [
    # file stem,          pix_fmt,       ffmpeg matrix name, strip the colr box?
    ("avif-8bit-bt709",   "yuv444p",     "bt709",            False),
    ("avif-8bit-bt601",   "yuv444p",     "smpte170m",        False),
    ("avif-8bit-nocolr",  "yuv444p",     "bt709",            True),
    ("avif-10bit-bt709",  "yuv444p10le", "bt709",            False),
    ("avif-10bit-bt601",  "yuv444p10le", "smpte170m",        False),
    ("avif-10bit-mono",   "gray10le",    "bt709",            False),
]


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


def encode(src, dst, pix_fmt, matrix, strip):
    cmd = ["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", src]
    if not pix_fmt.startswith("gray"):
        # Convert RGB -> YUV with the matrix we are about to declare, so pixels and tag agree.
        cmd += ["-vf", f"scale=out_color_matrix={matrix}:out_range=pc"]
    cmd += ["-c:v", "libaom-av1", "-still-picture", "1", "-cpu-used", "0",
            "-aom-params", "lossless=1", "-pix_fmt", pix_fmt,
            "-color_primaries", "bt709", "-color_trc", "iec61966-2-1",
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
        want = np.array([[v] * 3 for v in MONO] if "mono" in stem else COLOUR, dtype=np.float64)
        worst = float(np.abs(patches(png) - want).max())
        # The Rust side's TOLERANCE is 4; a probe whose own reference decode is not comfortably
        # inside that has no room left to measure the codec with.
        flag = "ok " if worst <= 2.0 else "BAD"
        if worst > 2.0:
            ok = False
        print(f"  {flag} {stem:<20} dav1d worst error {worst:5.1f}  "
              f"{os.path.getsize(p):>4} bytes")
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
                src = os.path.join(tmp, "gray.png" if "mono" in stem else "chart.png")
                dst = os.path.join(OUT, stem + ".avif")
                encode(src, dst, pix_fmt, matrix, strip)
                print(f"  wrote {stem}.avif  {os.path.getsize(dst)} bytes")
        return verify(tmp)
    finally:
        for f in os.listdir(tmp):
            os.remove(os.path.join(tmp, f))
        os.rmdir(tmp)


if __name__ == "__main__":
    raise SystemExit(main())
