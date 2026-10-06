"""Photoshop files saved the two ways that take the baked preview away (issue #55).

Every Photoshop-written sample in the corpus carries resource 1036, the small JPEG preview, and
the thumbnail path reads that first. So every colour mode and depth reached SageThumbs' own
composite and layer readers only through unit tests; on real files, end to end, they were
never exercised. The files issue #55 came with had no preview at all, and that is what other
programs write and what Photoshop writes with its previews turned off. This writes, beside each
sample that has a preview:

    <name>-nopreview.<ext>   resource 1036 removed: Preferences > File Handling > Image Previews:
                             Never Save. The thumbnail must come from the stored composite.
    <name>-noflat.<ext>      also "Maximize PSD and PSB File Compatibility: Never": resource 1057's
                             hasRealMergedData set to 0 and the composite left blank paper white,
                             as Photoshop writes it. The thumbnail must come from the layers. Only
                             for samples with layers and a 1057 resource.

Everything else is the sample's own bytes, untouched (the layer section's offsets are relative,
so dropping a resource moves nothing inside it).

    python scripts/corpus_psd_shapes.py              # write what is missing or stale
    python scripts/corpus_psd_shapes.py --check      # exit 1 when one is missing or stale
"""
import argparse
import os
import struct
import sys

from corpus_variants_psd import psd_variants

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.normpath(os.path.join(HERE, "..", "..", "test-corpus"))

THUMBNAIL = 1036
OLD_THUMBNAIL = 1033
VERSION_INFO = 1057
SHAPES = ("-nopreview", "-noflat")


def resources(section):
    """The Image Resource blocks of `section`: (id, whole block bytes, data offset in block)."""
    out, i = [], 0
    while i + 12 <= len(section):
        sig = section[i:i + 4]
        if sig not in (b"8BIM", b"MeSa", b"AgHg", b"PHUT", b"DCSR"):
            raise ValueError("resource block %d does not start with a signature" % len(out))
        rid, = struct.unpack(">H", section[i + 4:i + 6])
        name_len = section[i + 6]
        name_total = (1 + name_len + 1) & ~1  # the Pascal name, padded to even
        size_at = i + 6 + name_total
        size, = struct.unpack(">I", section[size_at:size_at + 4])
        data_at = size_at + 4
        end = data_at + size + (size & 1)
        out.append((rid, section[i:end], data_at - i))
        i = end
    return out


def split(b):
    """The file's sections: header+colour mode data, resource section, layer section, image data."""
    if not b.startswith(b"8BPS"):
        raise ValueError("not a Photoshop file")
    psb = struct.unpack(">H", b[4:6])[0] == 2
    i = 26
    cm_len, = struct.unpack(">I", b[i:i + 4])
    i += 4 + cm_len
    res_len, = struct.unpack(">I", b[i:i + 4])
    res = b[i + 4:i + 4 + res_len]
    head_end, i = i, i + 4 + res_len
    wide = 8 if psb else 4
    lm_len, = struct.unpack(">Q" if psb else ">I", b[i:i + wide])
    layers_end = i + wide + lm_len
    return psb, b[:head_end], res, b[head_end + 4 + res_len:layers_end], b[layers_end:]


def packbits_row(value, n):
    """One row of `n` bytes, all `value`, PackBits-packed the way Photoshop packs a blank row."""
    out = bytearray()
    while n > 0:
        run = min(n, 128)
        out += bytes([0, value]) if run == 1 else bytes([257 - run, value])
        n -= run
    return bytes(out)


def blank_composite(b, psb, image):
    """Image data that is paper white in every channel: what Photoshop stores for the composite
    with Maximize Compatibility off (255 is white in grey, RGB, Lab's L and, inverted, CMYK),
    in the compression the sample's own composite uses (Photoshop packs 8-bit composites and
    stores 16- and 32-bit ones raw)."""
    channels, h, w, depth = struct.unpack(">HIIH", b[12:24])
    row_bytes = (w * depth + 7) // 8
    rows = channels * h
    if struct.unpack(">H", image[:2])[0] == 0:
        return struct.pack(">H", 0) + b"\xff" * (row_bytes * rows)
    row = packbits_row(255, row_bytes)
    count = struct.pack(">I" if psb else ">H", len(row))
    return struct.pack(">H", 1) + count * rows + row * rows


def merged_flag(blocks):
    """Resource 1057's hasRealMergedData, or None when the file has no 1057."""
    for rid, block, data_at in blocks:
        if rid == VERSION_INFO:
            return block[data_at + 4]
    return None


def shape(b, which):
    """`b` saved as `which` (one of SHAPES), or None when that shape does not apply to it."""
    psb, head, res, layers, image = split(b)
    blocks = resources(res)
    if not any(rid == THUMBNAIL for rid, _, _ in blocks):
        return None
    if which == "-noflat":
        # Without layers there is no picture left to draw; a flag already 0 means the sample
        # was saved this way already, and its -nopreview is the same file.
        if "has-layers" not in psd_variants(b) or merged_flag(blocks) != 1:
            return None
        image = blank_composite(b, psb, image)
    kept = []
    for rid, block, data_at in blocks:
        if rid in (THUMBNAIL, OLD_THUMBNAIL):
            continue
        if which == "-noflat" and rid == VERSION_INFO:
            block = block[:data_at + 4] + b"\x00" + block[data_at + 5:]
        kept.append(block)
    res = b"".join(kept)
    return head + struct.pack(">I", len(res)) + res + layers + image


def donors(corpus):
    for name in sorted(os.listdir(corpus)):
        stem, ext = os.path.splitext(name)
        if ext.lower() in (".psd", ".psb") and not stem.endswith(SHAPES):
            yield name, stem, ext


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=CORPUS)
    ap.add_argument("--check", action="store_true", help="report missing or stale shapes; write nothing")
    args = ap.parse_args()
    written, stale = 0, []
    for name, stem, ext in donors(args.corpus):
        with open(os.path.join(args.corpus, name), "rb") as fh:
            b = fh.read()
        for which in SHAPES:
            want = shape(b, which)
            dest = os.path.join(args.corpus, stem + which + ext)
            have = open(dest, "rb").read() if os.path.exists(dest) else None
            if want is None and have is not None:
                # Written once from a donor that no longer takes this shape: it is not a file
                # Photoshop would write from that donor, so it goes.
                if args.check:
                    stale.append(os.path.basename(dest) + " (should not exist)")
                else:
                    os.remove(dest)
                    print("  removed %s (its donor does not take this shape)" % os.path.basename(dest))
                continue
            if want is None or have == want:
                continue
            if args.check:
                stale.append(os.path.basename(dest))
                continue
            with open(dest, "wb") as fh:
                fh.write(want)
            written += 1
            print("  wrote %-34s %9d bytes  (from %s)" % (os.path.basename(dest), len(want), name))
    print("photoshop shapes: %d written%s" % (written, ", %d missing or stale" % len(stale) if stale else ""))
    for s in stale:
        print("  " + s)
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
