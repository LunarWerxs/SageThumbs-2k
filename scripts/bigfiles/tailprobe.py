"""Which corpus samples keep their picture when bytes are appended after them?

The big-file gate grows every format past the size gates by adding ballast the format ignores.
Appending zeros after the file is the cheapest ballast (a sparse range costs no disk), and it is
only honest for formats whose readers stop at their own end. This measures that per sample
instead of assuming it: render the sample, render a copy with 1 MiB of zeros appended, compare.

    python scripts/bigfiles/tailprobe.py [--out tail.json] [--st2k <exe>] [--jobs 8]

Writes {file: "tail" | "breaks" | "unrendered"} to --out (default tmp/bigfiles/tail.json).
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

from PIL import Image, ImageChops

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
CORPUS = os.path.abspath(os.path.join(ROOT, "..", "test-corpus"))
DEFAULT_ST2K = None  # the release st2k.exe in cargo's target directory (see `main`)


def render(st2k, path, out):
    """`st2k thumbnail` at 256; the decoded PNG, or None when nothing rendered."""
    try:
        r = subprocess.run([st2k, "thumbnail", path, out, "--size", "256"],
                           capture_output=True, timeout=120)
    except subprocess.TimeoutExpired:
        return None
    if r.returncode != 0 or not os.path.isfile(out):
        return None
    try:
        return Image.open(out).convert("RGBA")
    except OSError:
        return None


def same_picture(a, b):
    if a.size != b.size:
        return False
    diff = ImageChops.difference(a, b).convert("L")
    hist = diff.histogram()
    mean = sum(i * n for i, n in enumerate(hist)) / max(1, a.size[0] * a.size[1])
    return mean < 2.0


def probe(st2k, work, name):
    src = os.path.join(CORPUS, name)
    base = os.path.join(work, name)
    orig = render(st2k, src, base + ".orig-out.png")
    if orig is None:
        return name, "unrendered"
    padded = base + ".padded-in" + os.path.splitext(name)[1]
    shutil.copyfile(src, padded)
    with open(padded, "ab") as f:
        f.write(b"\0" * (1 << 20))
    got = render(st2k, padded, base + ".padded-out.png")
    os.remove(padded)
    return name, "tail" if got is not None and same_picture(orig, got) else "breaks"


def samples():
    """One file per format: every `real*` and `sample*` file in the corpus."""
    for n in sorted(os.listdir(CORPUS)):
        p = os.path.join(CORPUS, n)
        if os.path.isfile(p) and (n.startswith("real") or n.startswith("sample")) and "." in n:
            yield n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=os.path.join(ROOT, "tmp", "bigfiles", "tail.json"))
    ap.add_argument("--st2k", default=DEFAULT_ST2K)
    ap.add_argument("--jobs", type=int, default=8)
    a = ap.parse_args()
    if a.st2k is None:
        from bigfiles import TARGET
        a.st2k = os.path.join(TARGET, "st2k.exe")
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    work = tempfile.mkdtemp(prefix="st2k-tailprobe-", dir=os.environ.get("ST2K_SCRATCH"))
    try:
        with ThreadPoolExecutor(a.jobs) as pool:
            results = dict(pool.map(lambda n: probe(a.st2k, work, n), list(samples())))
    finally:
        shutil.rmtree(work, ignore_errors=True)
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=1, sort_keys=True)
    counts = {}
    for v in results.values():
        counts[v] = counts.get(v, 0) + 1
    print(f"tailprobe: {len(results)} samples -> {counts}; wrote {a.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
