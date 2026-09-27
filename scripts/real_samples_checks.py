"""Settling each sample against the corpus, and what a --check run finds (see fetch-real-samples.py)."""

import http.client
import os

from real_samples_manifest import derive, donor_key, download, ext_of, holds, sha256_of


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
    except (OSError, ValueError, http.client.HTTPException) as exc:
        # Network, disk and digest failures: report the sample and keep going with the rest.
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
