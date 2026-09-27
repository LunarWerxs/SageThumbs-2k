"""Render one sample with st2k and measure the picture it produced (see compare-renders.py)."""

import os
import subprocess

# pip install -r scripts/requirements-dev.txt
from PIL import Image, ImageChops


# Compared at a common small size so an encoder's own rounding cannot masquerade as a change.
COMPARE_EDGE = 128


def render(exe, src, out, size, timeout):
    if os.path.exists(out):
        os.remove(out)
    try:
        subprocess.run([exe, "thumbnail", src, out, "--size", str(size)],
                       capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "timeout"
    return "ok" if os.path.exists(out) and os.path.getsize(out) > 0 else "none"


def as_8bit(im):
    """Narrow a high-bit-depth image to 8 bits BEFORE Pillow gets to convert it.

    Pillow's `convert("RGBA")` on a 16-bit image CLAMPS at 255 instead of scaling, so a
    perfectly good 16-bit grey render reads back as pure white and this gate calls it a wrong
    picture. That is a false RED, which is worse than a missed check: it trains you to explain
    away the one gate whose whole job is to catch a plausible-looking wrong render.

    Found 2026-09-08, when a 10-bit AVIF started coming back through ImageMagick (whose bundled
    build is Q16) instead of Windows' codec. Magick correctly detects a flat neutral image as
    GREYSCALE and writes 16-bit grey; the pixels were exactly right (12352/65535 == 48/255) and
    this script reported white. Any 16-bit render would have done the same, in any format.
    """
    if im.mode in ("I", "I;16", "I;16B", "I;16L", "I;16N", "F"):
        # Via "I" (32-bit) because point() cannot take a function on the I;16 variants.
        return im.convert("I").point(lambda v: v * (1 / 256), "L")
    return im


def normalized(path):
    with Image.open(path) as im:
        return as_8bit(im).convert("RGBA").resize((COMPARE_EDGE, COMPARE_EDGE), Image.BILINEAR)


def mean_delta(a_png, b_png):
    """Mean absolute per-channel difference. 0 = identical, 255 = maximally different."""
    hist = ImageChops.difference(normalized(a_png), normalized(b_png)).histogram()
    weighted = total = 0
    for channel in range(4):
        for value, count in enumerate(hist[channel * 256:(channel + 1) * 256]):
            weighted += value * count
            total += count
    return weighted / max(total, 1)


def centre(png):
    with Image.open(png) as im:
        im = as_8bit(im).convert("RGBA")
        return im.getpixel((im.width // 2, im.height // 2))


def compare_job(job):
    exe_old, exe_new, src, outdir, size, timeout = job
    name = os.path.basename(src)
    a = os.path.join(outdir, f"old__{name}.png")
    b = os.path.join(outdir, f"new__{name}.png")
    ra = render(exe_old, src, a, size, timeout)
    rb = render(exe_new, src, b, size, timeout)
    if ra != "ok" or rb != "ok":
        return (name, ra, rb, None)
    try:
        return (name, ra, rb, mean_delta(a, b))
    except Exception as e:                              # an unreadable PNG is itself the news
        return (name, ra, rb, f"unreadable: {e}")


def expect_job(job):
    exe_new, src, outdir, size, timeout, want, rendered = job
    name = os.path.basename(src)
    if rendered is not None:
        # Reuse what regression.ps1 already rendered rather than paying for it twice. It
        # names outputs "<stem>_<ext>.png" so same-extension samples cannot race on one path.
        stem, _, ext = name.rpartition(".")
        b = os.path.join(rendered, f"{stem}_{ext.lower()}.png")
        rb = "ok" if os.path.exists(b) and os.path.getsize(b) > 0 else "none"
    else:
        b = os.path.join(outdir, f"new__{name}.png")
        rb = render(exe_new, src, b, size, timeout)
    if rb != "ok":
        return (name, want, None, rb)
    got = centre(b)
    close = all(abs(g - w) <= 8 for g, w in zip(got[:3], want))
    return (name, want, got, "ok" if close else "WRONG COLOUR")


def load_expected(path):
    want = {}
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            name, _, rgb = line.partition("\t")
            want[name.strip()] = tuple(int(v) for v in rgb.strip().split(","))
    return want
