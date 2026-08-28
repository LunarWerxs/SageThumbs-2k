#!/usr/bin/env python3
"""Render a corpus with TWO st2k builds and report every sample whose PICTURE changed.

WHY THIS EXISTS. `regression.ps1` asks one question: did a non-empty PNG come out. It cannot
see a decoder that still produces a thumbnail and produces the WRONG one, and it counts an
extension as passing if ANY sample of it rendered, so a broken big sample hides behind a
working small one. Both blind spots are how the 2.0.0 XCF layer-budget bug shipped: 15-layer
GIMP files rendered a perfectly valid thumbnail of the wrong layer, and every gate stayed
green.

Comparing PIXELS between a known-good build and a candidate build closes both, across every
format at once, with no per-format work and no judgement calls. Run it before a release
against the previous release's portable `st2k.exe` (they are all in `dist\\`) and require every
difference to be one you meant:

  python scripts/compare-renders.py --corpus ..\\test-corpus --out D:\\rendercmp ^
      --old D:\\old\\st2k.exe --new D:\\.DevScratch\\build-cache\\st2k-target\\release\\st2k.exe

A second mode checks a rendered colour against what the file is KNOWN to flatten to, which is
what `make-xcf-fixture.py` builds its files to make possible:

  python scripts/compare-renders.py --corpus ..\\test-corpus --out D:\\rendercmp ^
      --new D:\\.DevScratch\\build-cache\\st2k-target\\release\\st2k.exe --expect expected-colors.txt

`expected-colors.txt` is `filename<TAB>r,g,b` per line; blank lines and `#` comments ignored.
"""

import argparse
import concurrent.futures as cf
import os
import subprocess
import sys

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


def normalized(path):
    with Image.open(path) as im:
        return im.convert("RGBA").resize((COMPARE_EDGE, COMPARE_EDGE), Image.BILINEAR)


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
        im = im.convert("RGBA")
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


def build_arg_parser():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--out", default=None)
    ap.add_argument("--new", default=None)
    ap.add_argument("--old", help="the known-good build; omit when using --expect")
    ap.add_argument("--expect", help="file of `name<TAB>r,g,b` known flattened colours")
    ap.add_argument("--rendered", default=None,
                    help="with --expect: check PNGs already in this directory (named "
                         "<stem>_<ext>.png, as regression.ps1 writes them) instead of "
                         "rendering them again")
    ap.add_argument("--size", type=int, default=256)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--jobs", type=int, default=6)
    ap.add_argument("--threshold", type=float, default=2.0)
    return ap


def validate_args(ap, a):
    if not a.old and not a.expect:
        ap.error("need --old (differential mode) or --expect (known-colour mode)")
    if not a.new and not a.rendered:
        ap.error("need --new (a build to render with) or --rendered (existing PNGs)")
    if a.old and not a.out:
        ap.error("differential mode needs --out to render into")


def list_corpus_files(corpus):
    return [os.path.join(corpus, f) for f in sorted(os.listdir(corpus))
            if os.path.isfile(os.path.join(corpus, f)) and not f.startswith("_")]


def print_expect_report(job_count, bad, missing):
    print(f"=== {job_count} files with a known flattened colour, "
          f"{job_count - len(bad)} correct ===")
    for name, w, got, verdict in bad:
        print(f"  {name:<44} want rgb{w}  got {got}  [{verdict}]")
    # A manifest entry with no sample behind it is a silently EMPTY check, which is the
    # failure mode this whole file exists to stop. Say so; do not quietly pass.
    for name in missing:
        print(f"  {name:<44} NOT IN THE CORPUS (re-run build-corpus.ps1)")


def run_expect_mode(a, files):
    want = load_expected(a.expect)
    jobs = [(a.new, f, a.out, a.size, a.timeout, want[os.path.basename(f)], a.rendered)
            for f in files if os.path.basename(f) in want]
    missing = sorted(set(want) - {os.path.basename(f) for f in files})
    bad = []
    with cf.ThreadPoolExecutor(max_workers=a.jobs) as pool:
        for name, w, got, verdict in pool.map(expect_job, jobs):
            if verdict != "ok":
                bad.append((name, w, got, verdict))
    print_expect_report(len(jobs), bad, missing)
    return 1 if (bad or missing) else 0


def classify_pair(name, ra, rb, delta, threshold):
    """Sort one compare_job result into its bucket name, or None to count as 'same'."""
    if ra == "ok" and rb != "ok":
        return "lost", (name, rb)
    if ra != "ok" and rb == "ok":
        return "gained", (name, ra)
    if ra != "ok":
        return "skip", None
    if isinstance(delta, str):
        return "error", (name, delta)
    if delta >= threshold:
        return "changed", (name, delta)
    return "same", None


def run_differential_pool(files, jobs, threshold, worker_count):
    changed, lost, gained, same, errs = [], [], [], 0, []
    buckets = {"lost": lost, "gained": gained, "changed": changed, "error": errs}
    with cf.ThreadPoolExecutor(max_workers=worker_count) as pool:
        for i, (name, ra, rb, delta) in enumerate(pool.map(compare_job, jobs), 1):
            if i % 25 == 0:
                print(f"  ...{i}/{len(files)}", file=sys.stderr, flush=True)
            kind, entry = classify_pair(name, ra, rb, delta, threshold)
            if kind == "same":
                same += 1
            elif kind != "skip":
                buckets[kind].append(entry)
    return changed, lost, gained, same, errs


def print_differential_report(total, same, lost, gained, changed, errs):
    print(f"\n=== {total} samples, {same} pixel-identical ===")
    print(f"\nLOST a thumbnail ({len(lost)}):")
    for n, why in sorted(lost):
        print(f"  {n:<44} new={why}")
    print(f"\nGAINED a thumbnail ({len(gained)}):")
    for n, why in sorted(gained):
        print(f"  {n:<44} old={why}")
    print(f"\nPICTURE CHANGED ({len(changed)}), worst first:")
    for n, d in sorted(changed, key=lambda x: -x[1]):
        print(f"  {n:<44} mean abs delta {d:6.1f}")
    if errs:
        print(f"\nUNREADABLE OUTPUT ({len(errs)}):")
        for n, e in errs:
            print(f"  {n:<44} {e}")


def run_differential_mode(a, files):
    jobs = [(a.old, a.new, f, a.out, a.size, a.timeout) for f in files]
    changed, lost, gained, same, errs = run_differential_pool(files, jobs, a.threshold, a.jobs)
    print_differential_report(len(files), same, lost, gained, changed, errs)
    return 1 if (lost or changed or errs) else 0


def main():
    ap = build_arg_parser()
    a = ap.parse_args()
    validate_args(ap, a)

    a.out = a.out or a.rendered
    os.makedirs(a.out, exist_ok=True)
    files = list_corpus_files(a.corpus)

    if a.expect:
        return run_expect_mode(a, files)
    return run_differential_mode(a, files)


if __name__ == "__main__":
    sys.exit(main())
