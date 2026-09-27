"""Camera RAW authoring variants: the container shape and the embedded previews (see
corpus-variants.py)."""

import struct


# ---- camera RAW (TIFF-based) ----------------------------------------------------------------------
def _tiff_ifds(b):
    """Every IFD's (tag -> (type, count, value_or_offset)) for a TIFF-shaped RAW, breadth first."""
    u16, u32 = _tiff_readers(b)
    if u16 is None:
        return []
    ifds, queue, seen = [], [u32(4)], set()
    while queue and len(ifds) < 32:
        off = queue.pop(0)
        if off in seen or off <= 0 or off + 2 > len(b):
            continue
        seen.add(off)
        n = u16(off)
        if n > 512 or off + 2 + n * 12 + 4 > len(b):
            continue
        tags = _tiff_read_tags(b, off, n, u16, u32)
        ifds.append(tags)
        queue.append(u32(off + 2 + n * 12))          # next IFD
        queue.extend(_tiff_child_offsets(b, tags, u32))
    return ifds


def _tiff_readers(b):
    if b[:2] == b"II":
        return (lambda o: struct.unpack("<H", b[o:o + 2])[0],
                lambda o: struct.unpack("<I", b[o:o + 4])[0])
    if b[:2] == b"MM":
        return (lambda o: struct.unpack(">H", b[o:o + 2])[0],
                lambda o: struct.unpack(">I", b[o:o + 4])[0])
    return None, None


def _tiff_read_tags(b, off, n, u16, u32):
    tags = {}
    for k in range(n):
        e = off + 2 + k * 12
        tag, typ, cnt = u16(e), u16(e + 2), u32(e + 4)
        val = u32(e + 8) if typ in (4, 9, 13) or (typ == 3 and cnt > 2) else (u16(e + 8) if typ == 3 else u32(e + 8))
        tags[tag] = (typ, cnt, val)
    return tags


def _tiff_child_offsets(b, tags, u32):
    """Offsets referenced by the SubIFD (0x14A) and Exif IFD (0x8769) tags."""
    offsets = []
    for sub in (0x14A, 0x8769):
        if sub not in tags:
            continue
        _, cnt, val = tags[sub]
        if cnt == 1:
            offsets.append(val)
        elif val + cnt * 4 <= len(b):
            offsets.extend(u32(val + 4 * j) for j in range(min(cnt, 8)))
    return offsets


def raw_variants(b):
    v = set()
    ifds = _tiff_ifds(b)
    if not ifds:
        return _raw_non_tiff_variants(b)
    v.add("tiff-based")
    if b[:4] == b"II\x55\x00":
        v.add("panasonic-rw2")
    previews = _raw_previews(ifds)
    if 0xC612 in ifds[0]:
        v.add("dng")
    if any(0x8769 in t for t in ifds):
        v.add("has-exif")
    v |= _raw_preview_variants(previews)
    return v


def _raw_non_tiff_variants(b):
    v = set()
    if b[:16] == b"FUJIFILMCCD-RAW ":
        v.add("fuji-raf")
        v.add("has-jpeg-preview" if b"\xff\xd8\xff" in b[:2 << 20] else "no-jpeg-preview")
    elif b[:4] == b"FOVb":
        v.add("sigma-x3f")
    return v


def _raw_previews(ifds):
    """(width, height) of every IFD that carries an embedded preview."""
    previews = []
    for tags in ifds:
        comp = tags.get(0x103, (0, 0, 0))[2]
        w = tags.get(0x100, (0, 0, 0))[2]
        h = tags.get(0x101, (0, 0, 0))[2]
        if 0x201 in tags or comp in (6, 7):
            previews.append((w, h))
    return previews


def _raw_preview_variants(previews):
    if not previews:
        return {"no-embedded-preview"}
    big = max(max(w, h) for w, h in previews)
    return {"preview-large" if big >= 1024 else ("preview-small" if big > 0 else "preview-size-unknown"),
            "several-previews" if len(previews) > 1 else "one-preview"}


RAW_WANTED = {"tiff-based", "fuji-raf", "dng", "has-exif", "no-embedded-preview", "preview-large",
              "preview-small", "one-preview", "several-previews"}
