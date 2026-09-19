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
    python scripts/corpus-variants.py --gate     # exit 1 if a variant the committed baseline
                                                 # lists has DISAPPEARED (the ratchet: the
                                                 # corpus may only gain variants, never lose
                                                 # one); MISSING variants print, never fail
    python scripts/corpus-variants.py --write-baseline   # after adding samples, record them

The baseline is `scripts/corpus-variants-baseline.txt` (one `ext:variant` per line, committed).
`verify.ps1` runs the gate; a sample deleted by mistake fails there, not at the next report.

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
    # The Layer and Mask Information section is present even for a flattened file (Photoshop
    # writes it with an empty layer info block), so its LENGTH is not the tell: the layer
    # COUNT inside it is. Layer info = length (u32, u64 for PSB) + i16 count (negative when the
    # first alpha channel holds the merged result's transparency; the magnitude is the count).
    wide = 8 if ver == 2 else 4
    fmt = ">Q" if ver == 2 else ">I"
    lm_len, = struct.unpack(fmt, b[i:i + wide]); lm_start = i + wide; i = lm_start + lm_len
    layers = 0
    if lm_len > 0:
        li_len, = struct.unpack(fmt, b[lm_start:lm_start + wide])
        if li_len > 0:
            layers = abs(struct.unpack(">h", b[lm_start + wide:lm_start + wide + 2])[0])
        # 16- and 32-bit files keep their layers in the `Lr16` / `Lr32` tagged blocks instead,
        # with the main layer info length at zero (real-16bit.psd read as flat until this).
        section = b[lm_start:lm_start + lm_len]
        for tag in (b"Lr16", b"Lr32"):
            at = section.find(tag)
            if at != -1 and layers == 0:
                off = at + 4 + wide  # tag, block length, then the count directly
                if off + 2 <= len(section):
                    layers = abs(struct.unpack(">h", section[off:off + 2])[0])
    v.add("has-layers" if layers > 0 else "no-layers")
    # Merged image data follows. Photoshop ALWAYS writes the section; with "Maximize
    # Compatibility" off it writes a blank (uniform) composite instead of the artwork, so the
    # tell is whether the composite is uniform, not whether it is there. RLE rows of a uniform
    # image are two runs at most (a run is 2 bytes); raw rows are sampled for a second value.
    v.add(composite_kind(b, i, w, h, channels, ver, mode))
    return v


def composite_kind(b, i, w, h, channels, ver, mode):
    if len(b) - i < 2:
        return "no-composite"
    comp, = struct.unpack(">H", b[i:i + 2])
    i += 2
    rows = h * channels
    # "Blank" is what Photoshop writes there with Maximize Compatibility off: every row one
    # colour, and that colour paper white (255 in every channel; 0 for a bitmap). A flat-colour
    # ARTWORK is uniform too (real-flat.psd is a red rectangle) and is a real composite.
    if comp == 1:  # RLE: a table of per-row byte counts, then the packed rows
        wide, fmt = (4, ">I") if ver == 2 else (2, ">H")
        if len(b) < i + rows * wide:
            return "no-composite"
        counts = [struct.unpack(fmt, b[i + k * wide:i + k * wide + wide])[0] for k in range(rows)]
        runs_per_row = (w + 127) // 128  # a uniform row is one run per 128 pixels
        if not all(c <= runs_per_row * 2 for c in counts):
            return "has-composite"
        first = b[i + rows * wide:i + rows * wide + counts[0]]
        value = first[1] if len(first) >= 2 and first[0] > 128 else None
        return "composite-blank" if value == 255 and mode in PAPER_WHITE_MODES else "has-composite"
    if comp == 0:  # raw planar samples
        n = w * h * channels
        data = b[i:i + n]
        if len(data) < n:
            return "no-composite"
        uniform = data.count(data[:1]) == len(data)
        return "composite-blank" if uniform and data[0] == 255 and mode in PAPER_WHITE_MODES else "has-composite"
    return "has-composite"


# Greyscale, RGB, CMYK (stored inverted, so 255 is no ink) and Lab all write paper white as
# 255; an indexed or bitmap file's 0/255 is a palette entry or ink, never a blank marker.
PAPER_WHITE_MODES = {1, 3, 4, 9}

# `no-composite` (the section absent altogether) is not a variant any writer seen here
# produces - Photoshop always writes the section and blanks it instead - so it is reported
# when found and never wanted.
PSD_WANTED = {"8-bit", "16-bit", "32-bit", "rgb", "cmyk", "greyscale", "indexed", "lab",
              "has-layers", "no-layers", "has-composite", "composite-blank",
              "has-thumbnail-1036"}


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


BASELINE = Path(__file__).resolve().parent / "corpus-variants-baseline.txt"


def main():
    as_json = "--json" in sys.argv
    gate = "--gate" in sys.argv
    write_baseline = "--write-baseline" in sys.argv
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
    have_now = {f"{ext}:{k}" for ext, r in report.items() for k in r["present"] if not k.startswith("unparseable:")}
    if write_baseline:
        BASELINE.write_text("\n".join(sorted(have_now)) + "\n", encoding="utf-8")
        print(f"wrote {len(have_now)} present variants to {BASELINE.name}")
        return
    for ext, r in report.items():
        print(f"== .{ext}: {len(r['samples'])} sample(s)")
        for k, names in r["present"].items():
            print(f"   {k:24s} {len(names)}  {', '.join(names[:4])}{' ...' if len(names) > 4 else ''}")
        if r["missing"]:
            print(f"   MISSING {ext}: {', '.join(r['missing'])}")
    if gate:
        if not any(c.is_dir() for c in CORPORA):
            print("corpus-variants: NOT MEASURED - no test corpus on this machine")
            sys.exit(2)
        if not BASELINE.is_file():
            print(f"corpus-variants: no baseline at {BASELINE.name} (run --write-baseline)")
            sys.exit(2)
        expected = {l.strip() for l in BASELINE.read_text(encoding="utf-8").splitlines() if l.strip()}
        lost = sorted(expected - have_now)
        gained = sorted(have_now - expected)
        if gained:
            print(f"corpus-variants: {len(gained)} variant(s) present but not in the baseline - run --write-baseline: {', '.join(gained)}")
        if lost:
            print(f"corpus-variants: FAIL - {len(lost)} variant(s) the baseline lists are GONE from the corpus: {', '.join(lost)}")
            sys.exit(1)
        print(f"corpus-variants: ok - all {len(expected)} baselined variants present")


if __name__ == "__main__":
    main()
