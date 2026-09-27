# Fetch the REAL-WORLD samples the test corpus is pinned to, and prove the corpus still
# holds them.
#
#   python scripts\fetch-real-samples.py            # download whatever is missing
#   python scripts\fetch-real-samples.py --check    # no network: every file present, every
#                                                   # SHA-256 right, every alias equal to its donor
#   python scripts\fetch-real-samples.py --list     # what the manifest holds, by extension
#   python scripts\fetch-real-samples.py --ext tga pcx
#
# Why this exists. Until 2026-09-17 most of the corpus was ONE picture: `build-corpus.ps1`
# writes `_base.png` out through ImageMagick once per format, so ~150 of the ~390 samples were
# the same image in the same writer's dialect, and another ~60 were byte-copies of a neighbour
# under a second extension. That proves the extension is hooked and that we can read what
# ImageMagick writes. It says nothing about the file a user actually has, which came out of
# Photoshop, a camera, a slicer or a 1994 paint program, and the bugs this project keeps
# finding live exactly there (EPSI's blank line, the PQ JPEG XL, the 4:2:0 jxl transcode,
# InDesign's strip of page over grey). "One sample per format tests the WRITER, not the
# format."
#
# So every registered extension now has a file somebody else's software wrote, taken from an
# upstream test suite or sample server that is public and stable: metadata-extractor-images
# (real cameras and phones), TwelveMonkeys (bug-report attachments for legacy formats), Apache
# Tika and POI (real Office, iWork, CAD and e-book files), OpenImageIO-images, libvips,
# Pillow, FFmpeg's FATE suite, lofty-rs, and the like. `scripts\corpus-real.json` is the
# manifest: a URL pinned to a commit where the host allows it, the SHA-256, the size and
# where it came from. A changed or hijacked upstream file fails the digest here rather than
# quietly becoming the new "known good".
#
# The corpus is a local sibling directory that is never committed or redistributed, which is
# why upstream licences are not a constraint on what may sit in it; only URLs and digests
# live in this repository.
#
# DERIVED. Two wrappers are the donor's bytes inside a standard container, and are written
# here the way the real producers write them: `.emz` / `.wmz` are a gzip of the metafile
# (`gzip_of`), `.fbz` is a zip holding the `.fb2` (`zip_of`). `--check` opens the wrapper and
# compares what is inside with the donor, so a different zlib cannot make it cry wolf.
#
# ALIASES. Some registered extensions are the same format under another name (`.icb` is
# Targa, `.jfif` is JPEG, `.blend1` is what Blender renames yesterday's `.blend` to). For
# those the honest real sample is the donor's bytes under the alias name, and the manifest
# says so with `alias_of` plus the reason. An alias across two DIFFERENT formats is never
# written here: it would render through the donor's decoder and prove nothing about the
# extension it pretends to be (that is how `sample.pxn`, a copy of a DNG, asserted a black
# square for months).

import argparse
import os
import sys

from real_samples_checks import (
    Tally,
    check_content_is_its_format,
    check_coverage_rendered,
    list_manifest,
    settle_derived,
    settle_download,
)
from real_samples_manifest import ext_of, load

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.normpath(os.path.join(HERE, "..", "..", "test-corpus"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="verify the corpus against the manifest; no network")
    ap.add_argument("--list", action="store_true", help="print the manifest by extension and exit")
    ap.add_argument("--ext", nargs="*", help="limit to these extensions")
    ap.add_argument("--corpus", default=CORPUS)
    ap.add_argument("--rendered", help="with --check: the folder regression.ps1 rendered into")
    args = ap.parse_args()

    samples, _by_file, doc = load()
    if args.ext:
        want = {e.lower().lstrip(".") for e in args.ext}
        samples = [s for s in samples if ext_of(s["file"]) in want]
    if args.list:
        list_manifest(samples)
        return 0

    os.makedirs(args.corpus, exist_ok=True)
    tally = Tally()
    # Downloads first, so every alias below has its donor.
    for s in (s for s in samples if "url" in s):
        settle_download(s, args.corpus, args.check, tally)
    for s in (s for s in samples if "url" not in s):
        settle_derived(s, args.corpus, args.check, tally)
    if args.check and args.rendered and not args.ext:
        check_coverage_rendered(doc.get("coverage", {}), args.corpus, args.rendered, tally)
    if args.check:
        check_content_is_its_format(samples, args.corpus, tally)

    print("real samples: %d already in place, %d downloaded, %d alias/derived files written"
          % (tally.present, tally.fetched, tally.copied))
    if tally.problems:
        print("PROBLEMS (%d):" % len(tally.problems))
        for p in tally.problems:
            print("  " + p)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
