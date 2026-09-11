"""Twin AVIFs of ONE scene for the HDR test in src/decode/tests/colour.rs: the PQ / BT.2020
10-bit picture issue #39 describes, and its sRGB / BT.709 control.

    python scripts/make-avif-hdr-fixtures.py tests/fixtures/avif

The scene is the JPEG XL twins' (scripts/make-jxl-hdr-fixtures.py draws it: a grey ramp over
six colour patches, diffuse white at 203 nits), so one assertion shape covers both formats.
Encoded LOSSLESS 4:4:4 so the encoder contributes no error of its own, exactly like the colour
probes in assets/wicprobe. Needs ffmpeg with libaom-av1 on PATH.
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=900)
    if r.returncode != 0:
        raise SystemExit(f"{cmd[0]} failed:\n{r.stderr}")


def encode(src, dst, matrix, primaries, trc):
    run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error", "-i", str(src),
         # Convert RGB -> YUV with the matrix we are about to declare, so pixels and tag agree.
         "-vf", f"scale=out_color_matrix={matrix}:out_range=pc",
         "-c:v", "libaom-av1", "-still-picture", "1", "-cpu-used", "0",
         "-aom-params", "lossless=1", "-pix_fmt", "yuv444p10le",
         "-color_primaries", primaries, "-color_trc", trc,
         "-colorspace", matrix, "-color_range", "pc", "-f", "avif", str(dst)])


with tempfile.TemporaryDirectory() as tmp:
    run([sys.executable, str(HERE / "make-jxl-hdr-fixtures.py"), tmp])
    encode(Path(tmp) / "scene-pq2020.png", out / "scene-pq2020.avif",
           "bt2020nc", "bt2020", "smpte2084")
    encode(Path(tmp) / "scene-sdr709.png", out / "scene-sdr709.avif",
           "bt709", "bt709", "iec61966-2-1")
for name in ("scene-pq2020.avif", "scene-sdr709.avif"):
    print("wrote", out / name, os.path.getsize(out / name), "bytes")
