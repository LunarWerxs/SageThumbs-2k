"""Twin 16-bit TIFFs of ONE scene for the HDR test in src/decode/tests/colour.rs: the PQ /
BT.2020 picture tagged with a real BT.2020-PQ ICC profile (the way an HDR TIFF from a colour
pipeline is tagged; TIFF has no cICP), and its sRGB / BT.709 control with no profile.

    cargo run --example make-hdr-icc -- tests/fixtures/tiff/bt2020-pq.icc
    python scripts/make-tiff-hdr-fixtures.py tests/fixtures/tiff

The scene is the JPEG XL twins' (scripts/make-jxl-hdr-fixtures.py draws it). ImageMagick
assigns the profile without converting the samples, which is the point: the samples stay
PQ-encoded and the profile is what says so. Needs `magick` on PATH.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)
icc = out / "bt2020-pq.icc"
if not icc.exists():
    raise SystemExit(f"{icc} is missing: run `cargo run --example make-hdr-icc -- {icc}` first")


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if r.returncode != 0:
        raise SystemExit(f"{cmd[0]} failed:\n{r.stderr}")
    return r.stdout


with tempfile.TemporaryDirectory() as tmp:
    run([sys.executable, str(HERE / "make-jxl-hdr-fixtures.py"), tmp])
    pq_png = Path(tmp) / "scene-pq2020.png"
    sdr_png = Path(tmp) / "scene-sdr709.png"
    # `-strip` first so the PNG's own cICP/gAMA carry nothing into the TIFF; `-profile` on a
    # profile-less image ASSIGNS (no conversion), so the 16-bit samples are copied verbatim.
    run(["magick", str(pq_png), "-strip", "-profile", str(icc), "-depth", "16",
         "-compress", "zip", str(out / "scene-pq2020.tif")])
    run(["magick", str(sdr_png), "-strip", "-depth", "16", "-compress", "zip",
         str(out / "scene-sdr709.tif")])
    # Prove the samples were not converted: the ramp's white must still read the PQ signal.
    white = run(["magick", str(out / "scene-pq2020.tif"), "-format", "%[fx:int(p{318,25}.g*1000)]", "info:"])
    if abs(int(white) - 580) > 5:
        raise SystemExit(f"the PQ TIFF's white reads {white}/1000, expected ~580 (the raw PQ signal)")
for name in ("scene-pq2020.tif", "scene-sdr709.tif"):
    print("wrote", out / name, (out / name).stat().st_size, "bytes")
