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
import hashlib
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

# SHA-256 of every candidate this script currently resolves as the smallest-CC0 sample
# for its extension (raw.pixls.us, computed once and pinned here - checked in download()
# below), so a changed or hijacked download is caught instead of silently becoming the
# new RAW fixture the decoder is regression-tested against. Keyed by URL, not extension,
# because the "smallest CC0" pick can change if the catalogue changes: a URL missing from
# this table just means nobody has pinned it yet (download() warns and proceeds anyway,
# same as before this existed) - it is not treated as a hijack.
PINNED_SHA256 = {
    "https://raw.pixls.us/getfile.php/2851/nice/Hasselblad - H3D - 16bit (4:3).3FR": "34fdb93cb215e4f419ae0f7252c57b881fa381003124b4f9c143d27741f211b3",  # 3fr
    "https://raw.pixls.us/getfile.php/1582/nice/Sony - ILCE-7S - 14bit 14bit compressed (3:2).ARW": "a35ebb2fbec929daa5beb20d1ce5c15a8aac7b1a7a231455387f3df8a7442e07",  # arw
    "https://raw.pixls.us/getfile.php/2102/nice/Canon - EOS 40D - sRAW2 (sRAW) (3:2).CR2": "ba644e7dd2abe74eca260e67f0206ff113bf0f62e710f8130611e964d6be5bf1",  # cr2
    "https://raw.pixls.us/getfile.php/4659/nice/Canon - EOS R6 - 3:2.CR3": "74abb0a113d075ad9887a058082f40dd2a938c4813a08474d82356f11a027778",  # cr3
    "https://raw.pixls.us/getfile.php/2073/nice/Canon - PowerShot G1 - RAW (4:3).crw": "f39be5acf14057ce04527fd8d0574e57ec8ed5fed082d7bfaff763075a44ee8a",  # crw
    "https://raw.pixls.us/getfile.php/1347/nice/Kodak - DCS760C - 12bit (3:2).DCR": "d0e6bd0a3339daff884e73a08fed8e75b20fe05bba438fc185e3f0bd019d0da8",  # dcr
    "https://raw.pixls.us/getfile.php/7317/nice/Blackmagic - Micro Cinema Camera - 12bit (16:9).dng": "4c65b8cda205087cfb94d8931811e53e15eb4df538ad67c1b4a3e76c1185b277",  # dng
    "https://raw.pixls.us/getfile.php/2680/nice/Epson - R-D1 - 12bit (3:2).ERF": "09d8e533d93116294a9f3e161ed868e929454c7b946e083b644bd6bb75bcb5e4",  # erf
    "https://raw.pixls.us/getfile.php/1640/nice/Hasselblad - H5D-40 - 16bit (4:3).fff": "209d464e13702de5f4fdb830ac26ac140c98a8d0dc3e81c5ba61d93677330832",  # fff
    "https://raw.pixls.us/getfile.php/8010/nice/Phase One - P40+ - IIQ S (4:3).IIQ": "61aa32c5947d386f0d5ee465c5183185df6c9cd7ff7eb66ffec12e84b487878c",  # iiq
    "https://raw.pixls.us/getfile.php/2345/nice/Kodak - KODAK P712 ZOOM DIGITAL CAMERA - NAN.KDC": "b1c3870c8c794f0271053a80a15296e3a9b575ab736796cdb53929d50c86a084",  # kdc
    "https://raw.pixls.us/getfile.php/4510/nice/Minolta - RD 175.MDC": "b7dfc1e9c895610c7170d00a03d3eff6e689f87bb750574c94c056ee4905ced4",  # mdc
    "https://raw.pixls.us/getfile.php/562/nice/Mamiya - ZD.MEF": "bcd63507c3c4cc3ea1bad2945e3f88cb4677a1a3c762e1acc38df4445714eb7c",  # mef
    "https://raw.pixls.us/getfile.php/3196/nice/Leaf - Aptus 22 - 16bit (4:3).mos": "8a1a101cb3940b0648af54a721d92e6fb8612dd29a0909036ea2753196a00ec2",  # mos
    "https://raw.pixls.us/getfile.php/7795/nice/Minolta - DiMAGE 5 - 4:3.MRW": "d60bfd80bcb1f7b9c88b14a5e46ea27885bbf92d990ab30b3939288678fea0d9",  # mrw
    "https://raw.pixls.us/getfile.php/4282/nice/Nikon - Nikon COOLSCAN IV ED - uncompressed (4:3).nef": "268d9a98920a9f3ea3ebf6a2b9ff68b956df74ac0e46b980bee69e7ef3ebc172",  # nef
    "https://raw.pixls.us/getfile.php/3381/nice/Nikon - COOLPIX P1000 - 12bit 12bit uncompressed (4:3).NRW": "1d9470ee083f914e51aa0be782161b432f27f87dbef551a59ec435f5dbc289dd",  # nrw
    "https://raw.pixls.us/getfile.php/5424/nice/Olympus - E-10 - 16bit (4:3).ORF": "2bfdade72439017a60a47aad1e5bbcb1aca36f2be7a21e94a4257a678fb6f4da",  # orf
    "https://raw.pixls.us/getfile.php/2857/nice/Olympus - E-M5 Mark II - 16bit 16bit \"normal\" (4:3).ORI": "f2c7a1dfbfcc6093b28373a1616569b8bd9c2d91cb708480b8312d510c57fbd6",  # ori
    "https://raw.pixls.us/getfile.php/2239/nice/Pentax - K10D - 12bit 12bit compressed (3:2).PEF": "e35ae4154a468be3154f5f462e884ba5941f010d3e8f23d347fbec14809f44d3",  # pef
    "https://raw.pixls.us/getfile.php/2726/nice/Fujifilm - FinePix S5000 - 4:3.RAF": "dabd5e74521a6980156be9fd4b88d0c37b0fe4d0e0e6f5c12db8cffff1b76297",  # raf
    "https://raw.pixls.us/getfile.php/7008/nice/Panasonic - DMC-LX7 - 1:1.RW2": "d142a23aca836053ed53e9ce3cb3ed2d434541d734d71a94a6eefadcd08bd31b",  # rw2
    "https://raw.pixls.us/getfile.php/2811/nice/Leica - D-LUX 5 - 1:1.RWL": "ceeb5e5bf2358c04706e807bc023971e085d969b6f36491a57ecff051a05e8d3",  # rwl
    "https://raw.pixls.us/getfile.php/3221/nice/Sony - DSC-R1 - 14bit 14bit uncompressed (3:2).SR2": "921c5f2513dd1671a9089e73fc778fc3573bc4cb70a3a555d0414661d0e62274",  # sr2
    "https://raw.pixls.us/getfile.php/1351/nice/Sony - DSC-F828 - 4:3.SRF": "07739f94bcab0c7e198d418ac65172d0dce04bb84a7683ecffd0bfe4b32f6d79",  # srf
    "https://raw.pixls.us/getfile.php/3668/nice/Samsung - NX500 - 12bit 12bit normal compression (3:2).SRW": "156e43118811bcecb6fcc600b5f41ee1773c4f00b6a5b2dd72e177e35f6596ab",  # srw
    "https://raw.pixls.us/getfile.php/6942/nice/Sinarback - Sinarback eVolution 75, Sinar p3 / f3 - 16bit (4:3).sti": "b249f3f2362a79808029b2f69170afec60d0485521f4ea296cecd596cee7fae0",  # sti
    "https://raw.pixls.us/getfile.php/1116/nice/Sigma - DP1 - 3:2.X3F": "44508f7df4191aacaecb6461e1841dae002e5b71a4f95a00aa3f52d1c6d6bc5f",  # x3f
}

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


def sha256_of(path):
    """SHA-256 hex digest of a file on disk, read in chunks (RAW samples run tens of MB)."""
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def download(url, dest):
    req = urllib.request.Request(encode(url), headers={"User-Agent": "curl/8.4.0"})
    tmp = dest + ".part"
    with urllib.request.urlopen(req, timeout=300) as resp, open(tmp, "wb") as fh:
        while True:
            chunk = resp.read(1 << 20)
            if not chunk:
                break
            fh.write(chunk)
    digest = sha256_of(tmp)
    expected = PINNED_SHA256.get(url)
    if expected is not None and digest != expected:
        os.remove(tmp)
        raise ValueError(
            "SHA-256 mismatch for %s:\n  expected %s\n  actual   %s" % (url, expected, digest)
        )
    # Only ever move a complete, hash-verified file into the corpus: a truncated or
    # substituted sample would be indistinguishable from a decoder regression on the next
    # gate run.
    os.replace(tmp, dest)
    if expected is None:
        print("  (no pinned digest yet for this URL - sha256=%s; add it to PINNED_SHA256)" % digest)
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
