"""TIFF and HEIF / AVIF authoring variants (see corpus-variants.py)."""

import struct


# ---- TIFF --------------------------------------------------------------------------------------
def tiff_variants(b):
    v = set()
    if b[:4] not in (b"II*\x00", b"MM\x00*"):
        return v
    ifds, tags = _tiff_first_ifds(b)
    v.add("multi-page" if ifds > 1 else "single-page")
    if 37724 in tags:
        v.add("photoshop-layers")
    if 34665 in tags:
        v.add("has-exif")
    if 259 in tags:
        v.add("compressed-tag-present")
    return v


def _tiff_first_ifds(b):
    """The IFD count (capped at 64) and the tag set of the first-IFD chain."""
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
    return ifds, tags


TIFF_WANTED = {"single-page", "multi-page", "photoshop-layers", "has-exif"}


# ---- HEIF / AVIF -------------------------------------------------------------------------------
def _boxes(b, start, end, depth=0):
    """Yield (type, payload_start, payload_end) for the ISOBMFF boxes in b[start:end]."""
    i = start
    while i + 8 <= end:
        size, = struct.unpack(">I", b[i:i + 4])
        kind = b[i + 4:i + 8]
        hdr = 8
        if size == 1:
            size, = struct.unpack(">Q", b[i + 8:i + 16]); hdr = 16
        elif size == 0:
            size = end - i
        if size < hdr:
            return
        yield kind, i + hdr, min(i + size, end)
        i += size


def heif_variants(b):
    v = set()
    top = list(_boxes(b, 0, len(b)))
    if not top or top[0][0] != b"ftyp":
        return v
    _, s, e = top[0]
    brands = {b[j:j + 4] for j in range(s, e, 4)}
    v.add("avif" if b"avif" in brands or b"avis" in brands else "heic")
    v.add("sequence" if brands & {b"msf1", b"avis", b"hevc"} else "still")
    meta = next(((s2, e2) for k, s2, e2 in top if k == b"meta"), None)
    if meta is None:
        return v
    inner = {k: (s2, e2) for k, s2, e2 in _boxes(b, meta[0] + 4, meta[1])}  # full box: version+flags
    if b"iprp" in inner:
        v |= _heif_property_variants(b, *inner[b"iprp"])
    if b"iinf" in inner:
        v |= _heif_item_variants(b, *inner[b"iinf"])
    return v


def _heif_property_variants(b, ps, pe):
    """The item properties (`ipco`): alpha plane, colour box, HDR metadata, grid, bit depth."""
    v = set()
    ipco = next(((s3, e3) for k, s3, e3 in _boxes(b, ps, pe) if k == b"ipco"), None)
    if not ipco:
        return v
    props = [k for k, _, _ in _boxes(b, *ipco)]
    v.add("alpha" if b"auxC" in props else "no-alpha")
    v.add("colr" if b"colr" in props else "no-colr")
    v.add("hdr-metadata" if b"clli" in props or b"mdcv" in props else "no-hdr-metadata")
    v.add("grid" if b"grid" in props else "single-item")
    v |= _heif_bit_depth(b, *ipco)
    return v


def _heif_bit_depth(b, ps, pe):
    # pixi carries the bit depth per channel
    for k, s3, e3 in _boxes(b, ps, pe):
        if k == b"pixi" and e3 - s3 >= 6:
            return {f"{b[s3 + 5]}-bit"}
    return set()


def _heif_item_variants(b, s2, e2):
    """The item info (`iinf`): a derived grid item, an Exif item."""
    v = set()
    body = b[s2:e2]
    v.add("grid" if b"grid" in body else "single-item")
    if b"Exif" in body:
        v.add("has-exif")
    return v


HEIF_WANTED = {"heic", "avif", "still", "sequence", "alpha", "no-alpha", "colr", "no-colr",
               "hdr-metadata", "no-hdr-metadata", "grid", "single-item", "8-bit", "10-bit"}
