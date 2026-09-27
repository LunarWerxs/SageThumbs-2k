"""The corpus-real.json manifest, and fetching or deriving one sample (see fetch-real-samples.py)."""

import gzip
import hashlib
import io
import json
import os
import shutil
import urllib.parse
import urllib.request
import zipfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST = os.path.join(HERE, "corpus-real.json")


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
    except (OSError, EOFError, zlib.error, zipfile.BadZipFile, KeyError, RuntimeError, NotImplementedError):
        # An unreadable, truncated or entry-less wrapper simply does not hold the donor.
        return False


def ext_of(name):
    return name.rsplit(".", 1)[-1].lower()
