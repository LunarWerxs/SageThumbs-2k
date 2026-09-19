"""Which AUTHORING VARIANTS of each container format the test corpus actually holds.

Issues #44 and #45 (2026-09-19) got through fifty-odd releases and every full-codebase pass
because every pass graded "does this file produce a thumbnail" against the corpus, and all
three Illustrator samples in it are PDF-compatible, single-artboard files. A file saved without
"Create PDF Compatible File" produces a thumbnail too - of Illustrator's notice page. The hole
was in the SAMPLE SET, by save-time option, and no amount of reading code finds a variant the
corpus never contains.

This is the instrument for that hole. For each format whose authoring options change what is
INSIDE the file, it classifies every corpus sample by those options and prints, per format, the
variants present and the variants with NO sample. Exit 0 always: the output is the report; a
gate that wants a number reads the `MISSING` lines.

    python scripts/corpus-variants.py            # both corpora
    python scripts/corpus-variants.py --json     # machine-readable

Variant definitions are deliberately about what the FILE holds, never about what SageThumbs does
with it, so the report stays true when the decoder changes.
"""
import json
import os
import re
import struct
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
CORPORA = [ROOT / "test-corpus", ROOT / "test-corpus-real"]


def read_head(p, n=4 << 20):
    with open(p, "rb") as f:
        return f.read(n)


# ---- Adobe Illustrator ----------------------------------------------------------------------
def ai_variants(b):
    v = set()
    if b.startswith(b"%PDF-"):
        v.add("pdf-based")
        m = re.search(rb"/Count (\d+)", b)
        pages = int(m.group(1)) if m else 1
        v.add("multi-artboard" if pages > 1 else "single-artboard")
        # The placeholder page is what a non-PDF-compatible save writes; its text is in the
        # UI language, so the only stable tell is the absence of real page content. A PDF-
        # compatible file carries /AIMetaData AND content streams for the artwork; a
        # non-compatible one carries the private data and a tiny page. Approximation that
        # holds on every sample seen: the PDF half before the private data is a few KB.
        priv = b.find(b"%!PS-Adobe")
        pdf_half = priv if priv > 0 else len(b)
        v.add("pdf-compatible" if pdf_half > 20_000 else "no-pdf-compatible")
    elif b.startswith(b"%!PS"):
        v.add("postscript-based")
    if b"%AI7_Thumbnail" in b:
        v.add("has-private-thumbnail")
    return v


AI_WANTED = {"pdf-based", "postscript-based", "single-artboard", "multi-artboard",
             "pdf-compatible", "no-pdf-compatible", "has-private-thumbnail"}


# ---- Photoshop --------------------------------------------------------------------------------
def psd_variants(b):
    v = set()
    if not b.startswith(b"8BPS"):
        return v
    ver, = struct.unpack(">H", b[4:6])
    v.add("psb" if ver == 2 else "psd")
    channels, h, w, depth, mode = struct.unpack(">HIIHH", b[12:26])
    v.add({1: "8-bit", 16: "16-bit", 32: "32-bit"}.get(depth, f"{depth}-bit"))
    v.add({0: "bitmap", 1: "greyscale", 2: "indexed", 3: "rgb", 4: "cmyk", 7: "multichannel",
           8: "duotone", 9: "lab"}.get(mode, f"mode-{mode}"))
    i = 26
    cm_len, = struct.unpack(">I", b[i:i + 4]); i += 4 + cm_len
    res_len, = struct.unpack(">I", b[i:i + 4]); res = b[i + 4:i + 4 + res_len]; i += 4 + res_len
    if b"8BIM\x04\x0c" in res:
        v.add("has-thumbnail-1036")
    if ver == 2:
        lm_len, = struct.unpack(">Q", b[i:i + 8]); i += 8 + lm_len
    else:
        lm_len, = struct.unpack(">I", b[i:i + 4]); i += 4 + lm_len
    v.add("has-layers" if lm_len > 0 else "no-layers")
    # Merged image data follows: present when "Maximize Compatibility" was on (or no layers).
    v.add("has-composite" if len(b) - i > 2 + (w * h * channels * depth // 8) // 50 else "no-composite")
    return v


PSD_WANTED = {"8-bit", "16-bit", "32-bit", "rgb", "cmyk", "greyscale", "indexed", "lab",
              "has-layers", "no-layers", "has-composite", "no-composite", "has-thumbnail-1036"}


# ---- PDF ---------------------------------------------------------------------------------------
def pdf_variants(b):
    v = set()
    if not b.startswith(b"%PDF-"):
        return v
    m = re.search(rb"/Count (\d+)", b)
    pages = int(m.group(1)) if m else 1
    v.add("multi-page" if pages > 1 else "single-page")
    v.add("encrypted" if b"/Encrypt" in b else "not-encrypted")
    v.add("has-images" if b"/Subtype /Image" in b or b"/Subtype/Image" in b else "no-images")
    v.add("has-text" if b"/Font" in b else "no-text")
    v.add("linearized" if b"/Linearized" in b[:2048] else "not-linearized")
    return v


PDF_WANTED = {"single-page", "multi-page", "encrypted", "not-encrypted", "has-images",
              "no-images", "has-text", "no-text"}


# ---- EPS ---------------------------------------------------------------------------------------
def eps_variants(b):
    v = set()
    if b.startswith(bytes([0xC5, 0xD0, 0xD3, 0xC6])):
        v.add("dos-eps")
        tiff_off, tiff_len = struct.unpack("<II", b[20:28])
        wmf_off, wmf_len = struct.unpack("<II", b[12:20])
        v.add("tiff-preview" if tiff_off and tiff_len else ("wmf-only-preview" if wmf_off and wmf_len else "no-preview"))
    elif b.startswith(b"%!PS"):
        v.add("plain-ps")
        if b"%%BeginPreview" in b:
            v.add("epsi-preview")
        elif b"%BeginPhotoshop" in b:
            v.add("photoshop-preview")
        elif b"%AI7_Thumbnail" in b:
            v.add("illustrator-thumbnail")
        else:
            v.add("no-preview")
    return v


EPS_WANTED = {"dos-eps", "plain-ps", "tiff-preview", "wmf-only-preview", "epsi-preview",
              "photoshop-preview", "illustrator-thumbnail", "no-preview"}


# ---- TIFF --------------------------------------------------------------------------------------
def tiff_variants(b):
    v = set()
    if b[:4] not in (b"II*\x00", b"MM\x00*"):
        return v
    le = b[:2] == b"II"
    u16 = (lambda o: struct.unpack("<H" if le else ">H", b[o:o + 2])[0])
    u32 = (lambda o: struct.unpack("<I" if le else ">I", b[o:o + 4])[0])
    ifds = 0
    off = u32(4)
    tags = set()
    while off and off + 2 <= len(b) and ifds < 64:
        n = u16(off)
        for k in range(n):
            e = off + 2 + k * 12
            if e + 12 > len(b):
                break
            tags.add(u16(e))
        ifds += 1
        nxt = off + 2 + n * 12
        if nxt + 4 > len(b):
            break
        off = u32(nxt)
    v.add("multi-page" if ifds > 1 else "single-page")
    if 37724 in tags:
        v.add("photoshop-layers")
    if 34665 in tags:
        v.add("has-exif")
    if 259 in tags:
        v.add("compressed-tag-present")
    return v


TIFF_WANTED = {"single-page", "multi-page", "photoshop-layers", "has-exif"}

FAMILIES = {
    "ai": (ai_variants, AI_WANTED),
    "psd": (psd_variants, PSD_WANTED),
    "psb": (psd_variants, PSD_WANTED),
    "pdf": (pdf_variants, PDF_WANTED),
    "eps": (eps_variants, EPS_WANTED),
    "tif": (tiff_variants, TIFF_WANTED),
    "tiff": (tiff_variants, TIFF_WANTED),
}


def main():
    as_json = "--json" in sys.argv
    present = defaultdict(lambda: defaultdict(list))
    for corpus in CORPORA:
        if not corpus.is_dir():
            continue
        for p in sorted(corpus.iterdir()):
            ext = p.suffix.lower().lstrip(".")
            if ext not in FAMILIES or not p.is_file():
                continue
            fn, _ = FAMILIES[ext]
            try:
                for var in fn(read_head(p)):
                    present[ext][var].append(p.name)
            except Exception as e:  # a malformed sample is itself a finding
                present[ext][f"unparseable:{type(e).__name__}"].append(p.name)
    report = {}
    for ext, (_, wanted) in FAMILIES.items():
        have = present.get(ext, {})
        report[ext] = {
            "samples": sorted({n for names in have.values() for n in names}),
            "present": {k: sorted(v) for k, v in sorted(have.items())},
            "missing": sorted(w for w in wanted if w not in have),
        }
    if as_json:
        print(json.dumps(report, indent=2))
        return
    for ext, r in report.items():
        print(f"== .{ext}: {len(r['samples'])} sample(s)")
        for k, names in r["present"].items():
            print(f"   {k:24s} {len(names)}  {', '.join(names[:4])}{' ...' if len(names) > 4 else ''}")
        if r["missing"]:
            print(f"   MISSING {ext}: {', '.join(r['missing'])}")


if __name__ == "__main__":
    main()
