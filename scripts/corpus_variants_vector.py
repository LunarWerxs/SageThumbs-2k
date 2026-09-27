"""Illustrator, PDF, EPS and CorelDRAW authoring variants (see corpus-variants.py)."""

import re
import struct


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


# ---- CorelDRAW -----------------------------------------------------------------------------------
def cdr_variants(b):
    v = set()
    if b[:4] == b"RIFF" and b[8:11] == b"CDR":
        v.add("riff-cdr")
        v.add("version-" + b[11:12].decode("latin-1"))
        v.add("has-bmp-preview" if b"BM" in b[:64 << 10] and b"DISP" in b[:64 << 10] else "no-bmp-preview")
    elif b[:2] == b"PK":
        v.add("zip-cdr")  # X4+ can wrap the RIFF in a zip container
        v.add("has-riff-inside" if b"RIFF" in b else "no-riff-inside")
    return v


CDR_WANTED = {"riff-cdr", "zip-cdr", "has-bmp-preview", "no-bmp-preview"}
