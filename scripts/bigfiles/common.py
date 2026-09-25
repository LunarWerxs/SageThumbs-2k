"""What every part of the big-file gate shares: where things are, the sizes, the surfaces."""

import json
import os
import subprocess


HERE = os.path.dirname(os.path.abspath(__file__))


ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


CORPUS = os.path.abspath(os.path.join(ROOT, "..", "test-corpus"))


def cargo_target():
    """The workspace's cargo target directory, from `cargo metadata` - never a path typed here."""
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ROOT,
                         capture_output=True, text=True, check=True).stdout
    return json.loads(out)["target_directory"]


TARGET = os.path.join(cargo_target(), "release")


MANIFEST = os.path.join(HERE, "big-files.json")


MiB = 1 << 20


SIZES = {"300M": 300 * MiB, "2.2G": 2253 * MiB, "5G": 5120 * MiB}


SURFACE_SIZES = {"cli": ["300M", "2.2G"], "thumb": ["300M"], "pane": ["300M"],
                 "quick": ["300M", "2.2G", "5G"]}


# The pixel axis: one genuinely big file per case, on every surface, against its reference.
BIG = "big"


# The largest file a format can LEGITIMATELY be. A twin past it is not a real file of that
# format (a classic TIFF or a PSD with gigabytes of junk after it), so failing on it would be
# a false alarm. Most formats address their data with 32-bit offsets: 4 GiB. PSD's own limit
# is 2 GB, which is why PSB exists. These carry 64-bit offsets and really do reach 5 GB.
FOUR_GIB = (4 << 30) - 1


# A version-3 compound file (512-byte sectors: legacy Office, SolidWorks, Publisher, Visio)
# is limited to 2 GB by its own specification; 3ds Max writes version 4.
TWO_GIB = (2 << 30) - 1


MAX_SIZE = {"psd": TWO_GIB, **{e: TWO_GIB for e in (
    "doc", "dot", "xls", "xlt", "ppt", "pot", "pps", "pub", "vsd", "sldprt", "sldasm", "slddrw")}}


OVER_4_GIB = {
    "psb", "exr", "xcf", "blend", "fits", "fts", "fit", "mp4", "m4v", "mov", "mkv", "webm", "3gp",
    "3g2", "zip", "cbz", "7z", "cb7", "kra", "ora", "epub", "iso",
}


# This runs on a shared box: never let the gate itself become the thing that starves it.
MEMORY_CEILING = 3 << 30


def guard_memory():
    try:
        import psutil
    except ImportError:
        return
    used = psutil.Process().memory_info().private
    if used > MEMORY_CEILING:
        raise SystemExit(f"bigfiles: stopped, the gate itself holds {used >> 20} MB")


def max_size(ext):
    base = ext.split("~")[0]
    return MAX_SIZE.get(base, 1 << 50 if base in OVER_4_GIB else FOUR_GIB)
