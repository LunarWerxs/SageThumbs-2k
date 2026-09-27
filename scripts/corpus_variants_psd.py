"""Photoshop (PSD / PSB) authoring variants (see corpus-variants.py)."""

import struct


# ---- Photoshop --------------------------------------------------------------------------------
def psd_variants(b):
    v = set()
    if not b.startswith(b"8BPS"):
        return v
    ver, = struct.unpack(">H", b[4:6])
    channels, h, w, depth, mode = struct.unpack(">HIIHH", b[12:26])
    v |= _psd_colour_variants(ver, depth, mode)
    i = 26
    cm_len, = struct.unpack(">I", b[i:i + 4]); i += 4 + cm_len
    res_len, = struct.unpack(">I", b[i:i + 4]); res = b[i + 4:i + 4 + res_len]; i += 4 + res_len
    if b"8BIM\x04\x0c" in res:
        v.add("has-thumbnail-1036")
    i, layers = _psd_layers(b, i, ver)
    v.add("has-layers" if layers > 0 else "no-layers")
    # Merged image data follows. Photoshop ALWAYS writes the section; with "Maximize
    # Compatibility" off it writes a blank (uniform) composite instead of the artwork, so the
    # tell is whether the composite is uniform, not whether it is there.
    v.add(composite_kind(b, i, w, h, channels, ver, mode))
    return v


def _psd_colour_variants(ver, depth, mode):
    v = {"psb" if ver == 2 else "psd"}
    v.add({1: "8-bit", 16: "16-bit", 32: "32-bit"}.get(depth, f"{depth}-bit"))
    v.add({0: "bitmap", 1: "greyscale", 2: "indexed", 3: "rgb", 4: "cmyk", 7: "multichannel",
           8: "duotone", 9: "lab"}.get(mode, f"mode-{mode}"))
    return v


def _psd_layers(b, i, ver):
    # The Layer and Mask Information section is present even for a flattened file (Photoshop
    # writes it with an empty layer info block), so its LENGTH is not the tell: the layer
    # COUNT inside it is. Layer info = length (u32, u64 for PSB) + i16 count (negative when the
    # first alpha channel holds the merged result's transparency; the magnitude is the count).
    wide = 8 if ver == 2 else 4
    fmt = ">Q" if ver == 2 else ">I"
    lm_len, = struct.unpack(fmt, b[i:i + wide])
    lm_start = i + wide
    layers = 0
    if lm_len > 0:
        layers = _psd_flat_layer_count(b, lm_start, wide, fmt)
        if layers == 0:
            layers = _psd_tagged_layer_count(b[lm_start:lm_start + lm_len], wide)
    return lm_start + lm_len, layers


def _psd_flat_layer_count(b, lm_start, wide, fmt):
    li_len, = struct.unpack(fmt, b[lm_start:lm_start + wide])
    if li_len > 0:
        return abs(struct.unpack(">h", b[lm_start + wide:lm_start + wide + 2])[0])
    return 0


def _psd_tagged_layer_count(section, wide):
    # 16- and 32-bit files keep their layers in the `Lr16` / `Lr32` tagged blocks instead,
    # with the main layer info length at zero (real-16bit.psd read as flat until this).
    for tag in (b"Lr16", b"Lr32"):
        at = section.find(tag)
        if at == -1:
            continue
        off = at + 4 + wide  # tag, block length, then the count directly
        if off + 2 <= len(section):
            layers = abs(struct.unpack(">h", section[off:off + 2])[0])
            if layers > 0:
                return layers
    return 0


def composite_kind(b, i, w, h, channels, ver, mode):
    if len(b) - i < 2:
        return "no-composite"
    comp, = struct.unpack(">H", b[i:i + 2])
    i += 2
    if comp == 1:  # RLE: a table of per-row byte counts, then the packed rows
        return _composite_rle(b, i, w, h, channels, ver, mode)
    if comp == 0:  # raw planar samples
        return _composite_raw(b, i, w, h, channels, mode)
    return "has-composite"


def _composite_rle(b, i, w, h, channels, ver, mode):
    # "Blank" is what Photoshop writes there with Maximize Compatibility off: every row one
    # colour, and that colour paper white (255 in every channel; 0 for a bitmap). A flat-colour
    # ARTWORK is uniform too (real-flat.psd is a red rectangle) and is a real composite. RLE
    # rows of a uniform image are two runs at most (a run is 2 bytes).
    rows = h * channels
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


def _composite_raw(b, i, w, h, channels, mode):
    # Raw rows are sampled for a second value; a uniform composite is blank paper white.
    n = w * h * channels
    data = b[i:i + n]
    if len(data) < n:
        return "no-composite"
    uniform = data.count(data[:1]) == len(data)
    return "composite-blank" if uniform and data[0] == 255 and mode in PAPER_WHITE_MODES else "has-composite"


# Greyscale, RGB, CMYK (stored inverted, so 255 is no ink) and Lab all write paper white as
# 255; an indexed or bitmap file's 0/255 is a palette entry or ink, never a blank marker.
PAPER_WHITE_MODES = {1, 3, 4, 9}

# `no-composite` (the section absent altogether) is not a variant any writer seen here
# produces - Photoshop always writes the section and blanks it instead - so it is reported
# when found and never wanted.
PSD_WANTED = {"8-bit", "16-bit", "32-bit", "rgb", "cmyk", "greyscale", "indexed", "lab",
              "has-layers", "no-layers", "has-composite", "composite-blank",
              "has-thumbnail-1036"}
