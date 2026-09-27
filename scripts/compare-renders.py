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
difference to be one you meant. `--new` defaults to `<cargo target dir>\\release\\st2k.exe`
(resolved with `cargo metadata`, so it honours CARGO_TARGET_DIR and any .cargo\\config.toml
redirect - never a hardcoded dev-machine path), so it can usually be left out:

  python scripts/compare-renders.py --corpus ..\\test-corpus --out D:\\rendercmp --old D:\\old\\st2k.exe

A second mode checks a rendered colour against what the file is KNOWN to flatten to, which is
what `make-xcf-fixture.py` builds its files to make possible:

  python scripts/compare-renders.py --corpus ..\\test-corpus --out D:\\rendercmp --expect expected-colors.txt

`expected-colors.txt` is `filename<TAB>r,g,b` per line; blank lines and `#` comments ignored.
Pass --target-dir explicitly to point at a target dir `cargo metadata` would not itself find
(e.g. comparing against a build made with a different CARGO_TARGET_DIR than this shell has).
"""

import argparse
import json
import os
import subprocess
import sys

from compare_renders_modes import classify_pair, run_differential_mode, run_expect_mode

# Not used here: test_compare-renders.py loads THIS file by path and tests these (and
# classify_pair) through it, so they stay importable from here after the move.
from compare_renders_pixels import as_8bit, centre, load_expected, mean_delta


def cargo_target_dir():
    """Ask cargo for its resolved target directory (honours CARGO_TARGET_DIR and any
    .cargo\\config.toml `target-dir` redirect) instead of a hardcoded dev-machine path.
    Returns None on any failure - the caller falls back to requiring --new explicitly."""
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True, text=True, timeout=30, check=True,
        )
        return json.loads(out.stdout)["target_directory"]
    except Exception:
        return None


def build_arg_parser():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--out", default=None)
    ap.add_argument("--new", default=None,
                    help="the candidate st2k.exe; defaults to "
                         "<target-dir>\\release\\st2k.exe")
    ap.add_argument("--target-dir", default=None,
                    help="cargo target dir to resolve --new from when --new is omitted "
                         "(default: `cargo metadata`'s target_directory)")
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


def resolve_new(ap, a):
    """Fill in --new from --target-dir / `cargo metadata` when the caller left it out."""
    if a.new or a.rendered:
        return
    target_dir = a.target_dir or cargo_target_dir()
    if not target_dir:
        ap.error("need --new (a build to render with), --rendered (existing PNGs), or a "
                 "resolvable target dir (--target-dir, or run inside the cargo workspace "
                 "so `cargo metadata` can find one)")
    a.new = os.path.join(target_dir, "release", "st2k.exe")


def validate_args(ap, a):
    if not a.old and not a.expect:
        ap.error("need --old (differential mode) or --expect (known-colour mode)")
    resolve_new(ap, a)
    if a.old and not a.out:
        ap.error("differential mode needs --out to render into")


def list_corpus_files(corpus):
    return [os.path.join(corpus, f) for f in sorted(os.listdir(corpus))
            if os.path.isfile(os.path.join(corpus, f)) and not f.startswith("_")]


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
