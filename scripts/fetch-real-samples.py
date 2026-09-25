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
import gzip
import hashlib
import io
import json
import os
import shutil
import sys
import urllib.parse
import urllib.request
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST = os.path.join(HERE, "corpus-real.json")
CORPUS = os.path.normpath(os.path.join(HERE, "..", "..", "test-corpus"))


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest().upper()


def encode(url):
    """Upstream paths carry spaces, parentheses and non-ASCII names."""
    parts = urllib.parse.urlsplit(url)
    path = urllib.parse.quote(urllib.parse.unquote(parts.path))
    return urllib.parse.urlunsplit((parts.scheme, parts.netloc, path, parts.query, ""))


def download(url, dest, expected):
    req = urllib.request.Request(encode(url), headers={"User-Agent": "curl/8.4.0"})
    tmp = dest + ".part"
    with urllib.request.urlopen(req, timeout=300) as resp, open(tmp, "wb") as fh:
        shutil.copyfileobj(resp, fh, 1 << 20)
    digest = sha256_of(tmp)
    if digest != expected.upper():
        os.remove(tmp)
        raise ValueError("SHA-256 mismatch\n      expected %s\n      actual   %s" % (expected.upper(), digest))
    # Only a complete, digest-verified file ever lands in the corpus: a truncated or swapped
    # sample is indistinguishable from a decoder regression on the next gate run.
    os.replace(tmp, dest)
    return os.path.getsize(dest)


def load():
    with open(MANIFEST, "r", encoding="utf-8") as fh:
        doc = json.load(fh)
    samples = doc["samples"]
    existing = doc.get("existing", {})
    by_file = {s["file"]: s for s in samples}
    if len(by_file) != len(samples):
        raise SystemExit("corpus-real.json lists the same file twice")
    for s in samples:
        kinds = [k for k in ("url", "alias_of", "gzip_of", "zip_of") if k in s]
        if len(kinds) != 1:
            raise SystemExit("%s: exactly one of url / alias_of / gzip_of / zip_of is required" % s["file"])
        if kinds[0] != "url":
            # A donor is a pinned download here, or a real sample another script already
            # manages (`existing`); never another derived file, so every chain is one hop.
            donor = by_file.get(s[kinds[0]])
            if not ((donor is not None and "url" in donor) or s[kinds[0]] in existing):
                raise SystemExit("%s: %s must name a pinned download or an `existing` real sample" % (s["file"], kinds[0]))
            if not s.get("why"):
                raise SystemExit("%s: say `why` - what makes this the honest file for its extension" % s["file"])
    return samples, by_file, doc


def donor_key(s):
    return next(k for k in ("alias_of", "gzip_of", "zip_of") if k in s)


def derive(s, donor_bytes):
    """The bytes a derived sample should hold, built the way its real producers build them."""
    if "alias_of" in s:
        return donor_bytes
    if "gzip_of" in s:
        buf = io.BytesIO()
        with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0) as gz:
            gz.write(donor_bytes)
        return buf.getvalue()
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        info = zipfile.ZipInfo(s["entry"], date_time=(2026, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        z.writestr(info, donor_bytes)
    return buf.getvalue()


def holds(s, path, donor_bytes):
    """Does the derived file on disk still carry the donor? Compared by CONTENT, not digest."""
    try:
        with open(path, "rb") as fh:
            data = fh.read()
        if "alias_of" in s:
            return data == donor_bytes
        if "gzip_of" in s:
            return gzip.decompress(data) == donor_bytes
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            return z.read(s["entry"]) == donor_bytes
    except Exception:  # noqa: BLE001 - an unreadable wrapper simply does not hold the donor
        return False


def ext_of(name):
    return name.rsplit(".", 1)[-1].lower()


class Tally:
    """What one run did, and what it found wrong."""

    def __init__(self):
        self.present = 0
        self.fetched = 0
        self.copied = 0
        self.problems = []


def list_manifest(samples):
    for s in sorted(samples, key=lambda s: (ext_of(s["file"]), s["file"])):
        where = s.get("source") or ("%s %s" % (donor_key(s).replace("_", " "), s[donor_key(s)]))
        print("  %-10s %-34s %s" % (ext_of(s["file"]), s["file"], where))
    print("%d samples, %d extensions" % (len(samples), len({ext_of(s["file"]) for s in samples})))


def settle_download(s, corpus, check, tally):
    """One pinned download: already right, wrong, missing, or fetched now."""
    dest = os.path.join(corpus, s["file"])
    if os.path.exists(dest):
        if sha256_of(dest) == s["sha256"].upper():
            tally.present += 1
        else:
            tally.problems.append("%s: on disk but NOT the pinned file (delete it and re-run to restore)" % s["file"])
        return
    if check:
        tally.problems.append("%s: missing" % s["file"])
        return
    try:
        size = download(s["url"], dest, s["sha256"])
        tally.fetched += 1
        print("  ok   %-34s %9d bytes  %s" % (s["file"], size, s.get("source", "")), flush=True)
    except Exception as exc:  # noqa: BLE001 - report and keep going
        tally.problems.append("%s: %s" % (s["file"], exc))


def settle_derived(s, corpus, check, tally):
    """One alias or wrapper: holds its donor, or is (re)written from it."""
    dest = os.path.join(corpus, s["file"])
    donor = os.path.join(corpus, s[donor_key(s)])
    if not os.path.exists(donor):
        tally.problems.append("%s: its donor %s is not in the corpus" % (s["file"], s[donor_key(s)]))
        return
    with open(donor, "rb") as fh:
        donor_bytes = fh.read()
    if os.path.exists(dest) and holds(s, dest, donor_bytes):
        tally.present += 1
        return
    if check:
        tally.problems.append("%s: %s" % (s["file"], "no longer holds its donor" if os.path.exists(dest) else "missing"))
        return
    with open(dest, "wb") as fh:
        fh.write(derive(s, donor_bytes))
    tally.copied += 1


def check_coverage_rendered(coverage, corpus, rendered, tally):
    """The claim the manifest makes is "every covered extension has a REAL file that renders",
    and only a render proves the second half. One rendering sample per extension is enough: a
    second real sample that fails is the per-file baseline's business, not this gate's."""

    def drew(name, ext):
        # regression.ps1 writes "<basename>_<ext>.png" per sample, empty or absent on failure.
        png = os.path.join(rendered, "%s_%s.png" % (name.rsplit(".", 1)[0], ext))
        return os.path.exists(png) and os.path.getsize(png) > 0

    for ext, files in sorted(coverage.items()):
        missing = [f for f in files if not os.path.exists(os.path.join(corpus, f))]
        for f in missing:
            tally.problems.append("%s: named as the real .%s sample but not in the corpus" % (f, ext))
        if not missing and not any(drew(f, ext) for f in files):
            tally.problems.append(".%s: none of its real samples rendered (%s)" % (ext, ", ".join(files)))


# Registered formats whose files ARE markup, so a sample that opens with `<svg` / `<?xml` is
# what it claims to be.
MARKUP_FORMATS = {"svg", "fb2", "html", "htm", "xhtml", "xml", "dae", "kml", "xaml", "drawio", "mm"}


def check_content_is_its_format(samples, corpus, tally):
    """A pinned sample must BE its format, not merely carry its extension.

    Found 2026-09-23 by the big-file gate: `real.3gp` (and `real.3g2`, its alias) and
    `real.ggb` were SVG files - a mime-type ICON of a 3GP file, and a schematic - pinned by
    URL and SHA like any other sample. Every check passed them, because an SVG renders, so
    "3GP thumbnails on a real file" had been a false claim since the manifest was built. The
    two shapes a wrong download takes are checked here: markup standing in for a binary
    format (an icon, an error page), and a Git LFS pointer (the 130-byte stub a raw URL
    returns for a file kept in LFS)."""
    for s in samples:
        ext = ext_of(s["file"])
        path = os.path.join(corpus, s["file"])
        if not os.path.exists(path):
            continue
        with open(path, "rb") as f:
            head = f.read(512)
        text = head.lstrip().lower()
        if head.startswith(b"version https://git-lfs"):
            tally.problems.append("%s: a Git LFS pointer, not the file (pin the media.githubusercontent.com URL)" % s["file"])
        elif ext not in MARKUP_FORMATS and text.startswith((b"<svg", b"<?xml", b"<!doctype html", b"<html")):
            tally.problems.append("%s: its content is SVG/HTML/XML markup, not a .%s file" % (s["file"], ext))


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
