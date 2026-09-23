"""Pictures that are genuinely big, for the big-file gate's second axis.

The byte sweep (`bigfiles.py --axis size`) grows a small sample with ballast its readers skip, so
its picture stays small. Two kinds of big file it cannot make:

* WIDE: a 30000-pixel panorama or a gigapixel scan trips `decode::limits::MAX_DIM` (16384) and
  the pixel budgets, not the byte ceiling. These are written with ImageMagick, in the formats
  such pictures are actually saved as, at 17000x2500 (past MAX_DIM, the shape of a stitched
  panorama, 42 Mpx - small enough for ImageMagick's float pixels to stay under a gigabyte).
* HEAVY: an uncompressed raster whose PIXELS are past the 256 MiB input ceiling - a 300 MB
  scan saved as TIFF, PPM, FITS, TGA, BMP. Written here row by row with numpy (ImageMagick would
  need gigabytes to hold one), at 11000x9500 RGB.

There is no normal-size twin of either, so each comes with a reference: the same picture, in the
same format, written at a size every decoder takes. A surface's result for the big file must match
its result for the reference (both fitted to the same box).

    python scripts/bigfiles/bigpixels.py <out-dir>      # writes and lists the pairs
"""

import os
import shutil
import struct
import subprocess
import sys

import numpy as np

MAGICK = shutil.which("magick") or "magick"
WIDE = (17000, 2500)
WIDE_REF = (2000, 294)  # 25 whole squares across, like the big one
WIDE_FORMATS = ["png", "jpg", "tif", "bmp", "tga", "psd", "webp", "jp2", "gif", "exr", "heic", "avif",
                "psb", "tiff64"]
# Where a format cannot hold the wide picture, the biggest it can.
LIMIT = {"webp": (16383, 2409)}
# ImageMagick's coder name where it is not the extension.
CODER = {"tiff64": "TIFF64"}
HEAVY = (11000, 9500)
HEAVY_REF = (1100, 950)
# One channel: two bytes a sample (PGM MAXVAL 65535, FITS BITPIX 16) on a bigger canvas, so these
# too are past the 256 MiB ceiling (11000x9500 at one byte a pixel is only 100 MB).
GREY = {"pgm", "fits"}
HEAVY_GREY = (13000, 11000)
HEAVY_GREY_REF = (1300, 1100)
HEAVY_FORMATS = ["ppm", "pgm", "pam", "pfm", "fits", "tga", "bmp", "tif", "farbfeld", "tiff64"]


def picture(size, out):
    """A picture with detail at every scale: a ramp times a checkerboard whose squares are
    1/25 of the width (about 10 px on a 256 px tile, far from the pixel grid's aliasing), so
    the same picture at any size keeps the same squares on a tile.
    (ImageMagick's `pattern:checkerboard` has fixed 15-pixel squares, which vanish at tile size
    on a 17000-pixel picture and not on a 2048 one: the two stop being the same picture.)"""
    w, h = size
    cell = max(2, w // 25)
    ext = os.path.splitext(out)[1][1:]
    target = f"{CODER[ext]}:{out}" if ext in CODER else out
    subprocess.run([
        MAGICK, "-size", f"{w}x{h}", "gradient:#ff4000-#0040ff",
        "(", "-size", "2x2", "xc:white", "-fill", "gray40", "-draw", "point 0,0 point 1,1",
        "-sample", f"{cell * 2}x{cell * 2}!", "-write", "mpr:tile", "+delete", ")",
        "(", "-size", f"{w}x{h}", "tile:mpr:tile", ")",
        "-compose", "multiply", "-composite", "-depth", "8", target,
    ], check=True, capture_output=True)


def heavy_row(w, h, y, grey=False):
    """Row `y` (0 = top) of the heavy picture: a ramp in each channel and a checkerboard of 1/25
    the width, as uint8 (w,) or (w, 3)."""
    cell = max(2, w // 25)
    x = np.arange(w)
    check = ((x // cell + y // cell) % 2) * 60
    r = x * 255 // max(1, w - 1)
    g = np.full(w, y * 255 // max(1, h - 1))
    rgb = np.clip(np.stack([r, g, 255 - r], axis=1) - check[:, None], 0, 255).astype(np.uint8)
    return rgb.mean(axis=1).astype(np.uint8) if grey else rgb


# Formats that store the BOTTOM row first.
BOTTOM_UP = {"bmp", "pfm", "fits"}
HEADERS = {
    "ppm": lambda w, h: f"P6\n{w} {h}\n255\n".encode(),
    "pgm": lambda w, h: f"P5\n{w} {h}\n65535\n".encode(),
    "pam": lambda w, h: f"P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n".encode(),
    "pfm": lambda w, h: f"PF\n{w} {h}\n-1.0\n".encode(),
    "farbfeld": lambda w, h: b"farbfeld" + struct.pack(">II", w, h),
    "tga": lambda w, h: struct.pack("<BBBHHBHHHHBB", 0, 0, 2, 0, 0, 0, 0, 0, w, h, 24, 0x20),
    "bmp": lambda w, h: b"BM" + struct.pack("<IHHI", 54 + ((w * 3 + 3) & ~3) * h, 0, 0, 54)
    + struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, ((w * 3 + 3) & ~3) * h, 2835, 2835, 0, 0),
    "fits": lambda w, h: "".join(c.ljust(80) for c in [
        "SIMPLE  =                    T", "BITPIX  =                   16",
        "NAXIS   =                    2", f"NAXIS1  = {w:>20}", f"NAXIS2  = {h:>20}",
        "BZERO   =                32768", "END"]).ljust(2880).encode("ascii"),
}


def encode_row(ext, r, w):
    if ext == "farbfeld":
        rgba = np.concatenate([r, np.full((w, 1), 255, np.uint8)], axis=1).astype(">u2") * 257
        return rgba.tobytes()
    if ext == "tga":
        return r[:, ::-1].tobytes()  # BGR, top-down (descriptor bit 5)
    if ext == "bmp":
        raw = r[:, ::-1].tobytes()
        return raw + bytes((-len(raw)) % 4)
    if ext == "pfm":
        return (r.astype("<f4") / 255.0).tobytes()
    if ext == "fits":
        return (r.astype(np.int32) * 257 - 32768).astype(">i2").tobytes()
    if ext == "pgm":
        return (r.astype(">u2") * 257).tobytes()
    return r.tobytes()


def write_heavy(ext, size, out):
    """Write the heavy picture at `size` as `ext`, one row at a time."""
    w, h = size
    if ext in ("tif", "tiff64"):
        return write_tiff(size, out, big=ext == "tiff64")
    grey = ext in GREY
    order = range(h - 1, -1, -1) if ext in BOTTOM_UP else range(h)
    n = 0
    with open(out, "wb") as f:
        f.write(HEADERS[ext](w, h))
        for y in order:
            data = encode_row(ext, heavy_row(w, h, y, grey), w)
            f.write(data)
            n += len(data)
        if ext == "fits":
            f.write(bytes((-n) % 2880))


def write_tiff(size, out, big):
    """A baseline RGB strip TIFF, one strip per row, classic or BigTIFF."""
    w, h = size
    row = w * 3
    word, pack = (8, "<Q") if big else (4, "<I")
    first = 16 if big else 8
    offsets_at = first + row * h
    counts_at = offsets_at + word * h
    bps_at = counts_at + word * h
    ifd_at = bps_at + 8
    short, long_, long8 = 3, 4, 16
    table = long8 if big else long_
    # BitsPerSample's three shorts fit inside a BigTIFF entry; a classic one points at them.
    bps = (8 | 8 << 16 | 8 << 32) if big else bps_at
    tags = [
        (256, long_, 1, w), (257, long_, 1, h), (258, short, 3, bps), (259, short, 1, 1),
        (262, short, 1, 2), (273, table, h, offsets_at), (277, short, 1, 3), (278, long_, 1, 1),
        (279, table, h, counts_at), (284, short, 1, 1),
    ]
    entry = struct.Struct("<HHQQ" if big else "<HHII")
    with open(out, "wb") as f:
        if big:
            f.write(b"II" + struct.pack("<HHHQ", 43, 8, 0, ifd_at))
        else:
            f.write(b"II" + struct.pack("<HI", 42, ifd_at))
        for y in range(h):
            f.write(heavy_row(w, h, y).tobytes())
        f.write(b"".join(struct.pack(pack, first + i * row) for i in range(h)))
        f.write(b"".join(struct.pack(pack, row) for _ in range(h)))
        f.write(struct.pack("<HHHH", 8, 8, 8, 0))
        f.write(struct.pack("<Q" if big else "<H", len(tags)))
        for tag, typ, n, value in tags:
            f.write(entry.pack(tag, typ, n, value))
        f.write(struct.pack(pack, 0))


# Cases this machine could not generate, with why: the report lists them, so a gap is named
# rather than silently shrinking the axis.
NOT_GENERATED = {}


def make(out_dir):
    """Write every pair into `out_dir`; returns [(case id, big path, reference path)]."""
    os.makedirs(out_dir, exist_ok=True)
    pairs = []
    for ext in WIDE_FORMATS:
        size = LIMIT.get(ext, WIDE)
        big = os.path.join(out_dir, f"wide.{ext}")
        ref = os.path.join(out_dir, f"wide-ref.{ext}")
        try:
            picture(size, big)
            picture(WIDE_REF, ref)
            pairs.append((f"{ext}~wide", big, ref))
        except subprocess.CalledProcessError as e:
            why = e.stderr.decode(errors="replace").strip().splitlines()[0][:160]
            NOT_GENERATED[f"{ext}~wide"] = f"NOT MEASURED: this ImageMagick cannot write it ({why})"
            print(f"bigpixels: ImageMagick cannot write {ext}: {why}")
    for ext in HEAVY_FORMATS:
        big = os.path.join(out_dir, f"heavy.{ext}")
        # The same picture in the same format at a tenth of the size: the pattern scales with
        # the width, and the format's own reader (and display rules: FITS's stretch) is what
        # draws both.
        ref = os.path.join(out_dir, f"heavy-ref.{ext}")
        size, ref_size = (HEAVY_GREY, HEAVY_GREY_REF) if ext in GREY else (HEAVY, HEAVY_REF)
        write_heavy(ext, size, big)
        write_heavy(ext, ref_size, ref)
        pairs.append((f"{ext}~heavy", big, ref))
    return pairs


def main():
    pairs = make(sys.argv[1])
    for case, big, ref in pairs:
        print(f"{case}\t{os.path.getsize(big) >> 20} MiB\t{big}\t{ref}")


if __name__ == "__main__":
    main()
