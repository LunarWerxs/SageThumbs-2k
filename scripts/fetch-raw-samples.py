# Fetch one real camera-RAW sample per extension from raw.pixls.us into the test
# corpus.
#
#   python scripts\fetch-raw-samples.py --list      # what WOULD be fetched; fetches nothing
#   python scripts\fetch-raw-samples.py             # fetch everything still missing
#   python scripts\fetch-raw-samples.py --ext nef arw
#
# Why this exists: 34 of the RAW extensions in formats.rs had NO sample of any
# kind, so every gate in the repo was silent about them. They are also the one
# family we cannot synthesize - a RAW file is a specific camera's sensor dump
# plus a maker-specific embedded preview, and our decoder's whole job is finding
# that preview. A hand-built stand-in would only test our own assumptions.
#
# raw.pixls.us is the community RAW sample repository (the one darktable and
# RawTherapee test against). Most of it is CC0; this script prefers CC0 and
# falls back to another licence only when that extension has nothing else, which
# is fine because the corpus is a local sibling directory that is never
# committed or redistributed.
#
# Picks the SMALLEST sample per extension on purpose. These files are tens of MB
# and the corpus renders serially in the regression gate; the goal is coverage of
# the format, not of any particular camera.

import argparse
import json
import os
import re
import sys
import urllib.parse
import urllib.request

INDEX_URL = "https://raw.pixls.us/json/getrepository.php?set=all"

# Every RAW extension in formats.rs RAW_EXTS, plus the three RAW-family entries
# that live in the misc FORMATS block (pwp/rmf/sti). Extensions the repository
# has nothing for are reported, not silently dropped.
TARGETS = """
    3fr arw bay cap cr2 cr3 crw dcr dcs dng drf erf fff iiq k25 kdc mdc mef mos
    mrw nef nrw orf ori pef ptx pwp pxn raf rmf rw2 rwl sr2 srf srw sti x3f
""".split()

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.normpath(os.path.join(HERE, "..", "..", "test-corpus"))
CACHE = os.path.join(CORPUS, "_pixls-index.json")

SIZE_RE = re.compile(r"\((\d+(?:\.\d+)?)(KB|MB|GB)\)")
HREF_RE = re.compile(r"href='([^']+)'")
UNITS = {"KB": 1 / 1024.0, "MB": 1.0, "GB": 1024.0}


def fetch_index(refresh=False):
    """The repository listing, cached beside the corpus (it is ~1.2 MB)."""
    if os.path.exists(CACHE) and not refresh:
        with open(CACHE, "rb") as fh:
            return json.load(fh)
    req = urllib.request.Request(INDEX_URL, headers={"User-Agent": "curl/8.4.0"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        raw = resp.read()
    os.makedirs(CORPUS, exist_ok=True)
    with open(CACHE, "wb") as fh:
        fh.write(raw)
    return json.loads(raw)


def candidates(index):
    """{ext: (megabytes, is_cc0, url)} for the smallest CC0 sample of each ext."""
    best = {}
    for row in index.get("data", []):
        if len(row) < 8:
            continue
        licence_html, file_html = row[5], row[7]
        href = HREF_RE.search(file_html)
        size = SIZE_RE.search(file_html)
        if not href or not size:
            continue
        url = href.group(1)
        megabytes = float(size.group(1)) * UNITS[size.group(2)]
        cc0 = "publicdomain/zero" in licence_html
        ext = url.rsplit(".", 1)[-1].lower()
        if ext not in TARGETS:
            continue
        # CC0 always wins over a licensed file; within a licence tier, smallest wins.
        prev = best.get(ext)
        better = prev is None or (cc0, -megabytes) > (prev[1], -prev[0])
        if better:
            best[ext] = (megabytes, cc0, url)
    return best


def encode(url):
    """The listing embeds raw spaces and parentheses in the path."""
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, urllib.parse.quote(parts.path), parts.query, "")
    )


def download(url, dest):
    req = urllib.request.Request(encode(url), headers={"User-Agent": "curl/8.4.0"})
    tmp = dest + ".part"
    with urllib.request.urlopen(req, timeout=300) as resp, open(tmp, "wb") as fh:
        while True:
            chunk = resp.read(1 << 20)
            if not chunk:
                break
            fh.write(chunk)
    # Only ever move a complete file into the corpus: a truncated sample would be
    # indistinguishable from a decoder regression on the next gate run.
    os.replace(tmp, dest)
    return os.path.getsize(dest)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true", help="report only; download nothing")
    ap.add_argument("--ext", nargs="*", help="limit to these extensions")
    ap.add_argument("--refresh", action="store_true", help="re-fetch the repository index")
    ap.add_argument("--corpus", default=CORPUS)
    args = ap.parse_args()

    wanted = [e.lower().lstrip(".") for e in args.ext] if args.ext else TARGETS
    best = candidates(fetch_index(args.refresh))

    todo, have, absent = [], [], []
    for ext in sorted(wanted):
        existing = [
            f
            for f in os.listdir(args.corpus)
            if f.lower().endswith("." + ext) and not f.endswith(".part")
        ]
        if existing:
            have.append(ext)
            continue
        if ext not in best:
            absent.append(ext)
            continue
        todo.append((ext,) + best[ext])

    if have:
        print("already in the corpus (%d): %s" % (len(have), " ".join(have)))
    if absent:
        print("NOT IN THE REPOSITORY (%d): %s" % (len(absent), " ".join(absent)))
    if not todo:
        print("nothing to fetch")
        return 0

    total = sum(t[1] for t in todo)
    print("to fetch: %d files, %.0f MB" % (len(todo), total))
    for ext, megabytes, cc0, url in todo:
        print("  %-5s %7.1f MB  %s" % (ext, megabytes, "CC0" if cc0 else "licensed"))
    if args.list:
        return 0

    failed = []
    for ext, megabytes, _cc0, url in todo:
        dest = os.path.join(args.corpus, "sample." + ext)
        try:
            size = download(url, dest)
            print("  ok   %-5s %8.1f MB -> %s" % (ext, size / 1048576.0, dest), flush=True)
        except Exception as exc:  # noqa: BLE001 - report and keep going
            failed.append(ext)
            print("  FAIL %-5s %s" % (ext, exc), flush=True)

    if failed:
        print("failed (%d): %s" % (len(failed), " ".join(failed)))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
