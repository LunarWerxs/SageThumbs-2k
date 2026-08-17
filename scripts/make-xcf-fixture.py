#!/usr/bin/env python3
"""Write real GIMP XCF v011 files at any canvas size and layer count.

WHY THIS EXISTS. The corpus had exactly two `.xcf` samples, 1.8 KB and 206 KB, both a
handful of layers. That is the whole of what `src/container/xcf.rs` was ever tested
against, and it is why a user could report on 2026-08-17 that "xcf don't work anymore
with new versions for big files" and there was nothing in the repo able to reproduce or
refute it. Authoring a big layered `.xcf` by hand means installing GIMP and painting;
this writes one in a second.

The layout mirrors what GIMP 3 actually writes, verified field by field against
`test-corpus/sample-gimp3.xcf`: magic `gimp xcf v011`, 64-bit file offsets, RGB base
type, precision 150 (8-bit gamma), `PROP_COMPRESSION` = 1 (RLE), 64x64 tiles.

THE POINT OF THE COLOURS. Every layer is one flat colour from a fixed palette, bottom
layer first, so a correctly flattened file has a KNOWN centre pixel: the top layer's
colour. That is what turns "did a thumbnail come out" - all the regression harness can
ask - into "is it the RIGHT thumbnail". The 2.0.0 layer-budget bug produced a perfectly
valid PNG of the wrong layer, and only the colour could tell.

  python scripts/make-xcf-fixture.py --matrix ..\\test-corpus
  python scripts/make-xcf-fixture.py out.xcf --width 6000 --height 4000 --layers 15
  python scripts/make-xcf-fixture.py big.xcf --width 4000 --height 4000 --layers 5 --noise

`--noise` writes incompressible pixel data, which is the only way to build a genuinely
large file on disk: a solid layer RLE-compresses to a few bytes however big it is.
"""

import argparse
import os
import struct

TILE = 64

# Distinct, easily-told-apart layer colours, bottom layer first.
PALETTE = [
    (220, 40, 40),    # red     <- bottom
    (240, 140, 20),   # orange
    (230, 220, 30),   # yellow
    (60, 180, 60),    # green
    (40, 120, 230),   # blue
    (140, 60, 200),   # purple
]


def u32(v):
    return struct.pack(">I", v)


def u64(v):
    return struct.pack(">Q", v)


def rle_solid_tile(npix, rgba):
    """One tile, every pixel `rgba`, in GIMP's per-byte-plane RLE."""
    out = bytearray()
    for plane in range(4):
        # opcode 127 = long run: u16 length, then the repeated value.
        out += bytes([127]) + struct.pack(">H", npix) + bytes([rgba[plane]])
    return bytes(out)


def rle_literal_tile(npix, payload):
    """One tile of incompressible bytes: a long literal run per plane."""
    out = bytearray()
    for plane in range(4):
        # opcode 128 = long literal: u16 length, then that many raw bytes.
        out += bytes([128]) + struct.pack(">H", npix) + payload[:npix]
    return bytes(out)


def build(path, width, height, layers, noise=False, transparent_below=0):
    tiles_x = (width + TILE - 1) // TILE
    tiles_y = (height + TILE - 1) // TILE
    edge_w = width - (tiles_x - 1) * TILE
    edge_h = height - (tiles_y - 1) * TILE

    head = bytearray(b"gimp xcf v011\0")
    head += u32(width) + u32(height)
    head += u32(0)                            # base type: RGB
    head += u32(150)                          # precision: 8-bit gamma
    head += u32(17) + u32(1) + bytes([1])     # PROP_COMPRESSION = RLE
    head += u32(0) + u32(0)                   # PROP_END

    def name_of(i):
        return f"Layer {i}\0".encode()

    # Every pointer in this format is an ABSOLUTE file offset, so the entire layout has
    # to be resolved before a single byte is written.
    ptr_list_len = 8 * layers + 8 + 8          # layers + terminator + empty channel list
    layer_lens = [4 + 4 + 4 + 4 + len(name_of(i)) + (12 + 12 + 16 + 8) + 8 + 8
                  for i in range(layers)]
    hier_len = 4 + 4 + 4 + 8 + 8
    level_len = 4 + 4 + 8 * (tiles_x * tiles_y) + 8

    # One encoded blob per distinct tile SHAPE, reused across the grid.
    blobs = []
    for i in range(layers):
        colour = PALETTE[i % len(PALETTE)]
        alpha = 0 if i < transparent_below else 255
        rgba = (colour[0], colour[1], colour[2], alpha)
        shapes = {}
        for tw in {TILE, edge_w}:
            for th in {TILE, edge_h}:
                npix = tw * th
                shapes[(tw, th)] = (rle_literal_tile(npix, os.urandom(TILE * TILE))
                                    if noise else rle_solid_tile(npix, rgba))
        blobs.append(shapes)

    def shape(tx, ty):
        return (min(TILE, width - tx * TILE), min(TILE, height - ty * TILE))

    offsets, cur = [], len(head) + ptr_list_len
    for i in range(layers):
        l_off = cur
        h_off = l_off + layer_lens[i]
        lv_off = h_off + hier_len
        p = lv_off + level_len
        tile_offs = []
        for ty in range(tiles_y):
            for tx in range(tiles_x):
                tile_offs.append(p)
                p += len(blobs[i][shape(tx, ty)])
        offsets.append((l_off, h_off, lv_off, tile_offs))
        cur = p

    with open(path, "wb") as f:
        f.write(head)
        # GIMP writes the layer list TOP-first; the decoder composites in reverse.
        for i in reversed(range(layers)):
            f.write(u64(offsets[i][0]))
        f.write(u64(0))    # end of layer list
        f.write(u64(0))    # empty channel list

        for i in range(layers):
            l_off, h_off, lv_off, tile_offs = offsets[i]
            name = name_of(i)
            f.write(u32(width) + u32(height))
            f.write(u32(1))                                  # layer type: RGBA
            f.write(u32(len(name)) + name)
            f.write(u32(6) + u32(4) + u32(255))              # PROP_OPACITY
            f.write(u32(8) + u32(4) + u32(1))                # PROP_VISIBLE
            f.write(u32(15) + u32(8) + u32(0) + u32(0))      # PROP_OFFSETS
            f.write(u32(0) + u32(0))                         # PROP_END
            f.write(u64(h_off) + u64(0))                     # hierarchy, no layer mask

            f.write(u32(width) + u32(height) + u32(4))       # hierarchy, bpp = RGBA8
            f.write(u64(lv_off) + u64(0))

            f.write(u32(width) + u32(height))                # level 0
            for off in tile_offs:
                f.write(u64(off))
            f.write(u64(0))

            for ty in range(tiles_y):
                for tx in range(tiles_x):
                    f.write(blobs[i][shape(tx, ty)])

    return os.path.getsize(path), PALETTE[(layers - 1) % len(PALETTE)]


# The set worth keeping around, each entry chosen because it broke something real.
#   layers-over-budget   - more layer area than `MAX_LAYER_PIXELS`; 2.0.0 rendered the
#                          wrong layer here and reported success
#   layers-transparent   - the same overrun where the lower layers are transparent, which
#                          is how it turned into no thumbnail at all
#   big-canvas-2-layers  - the 12000x12000 file xcf.rs's own comments promise to render;
#                          two layers of it already exceed the budget
#   wide-canvas          - a canvas near MAX_DIM on one edge
MATRIX = {
    "sample-xcf-layers-over-budget.xcf":  dict(width=6000, height=4000, layers=15),
    "sample-xcf-layers-transparent.xcf":  dict(width=6000, height=4000, layers=15,
                                               transparent_below=12),
    "sample-xcf-big-canvas-2-layers.xcf": dict(width=12000, height=12000, layers=2),
    "sample-xcf-wide-canvas.xcf":         dict(width=16000, height=1200, layers=3),
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out", help="output file, or the target DIRECTORY with --matrix")
    ap.add_argument("--matrix", action="store_true",
                    help="write the standard big-XCF set into the given directory")
    ap.add_argument("--width", type=int, default=1024)
    ap.add_argument("--height", type=int, default=1024)
    ap.add_argument("--layers", type=int, default=1)
    ap.add_argument("--noise", action="store_true",
                    help="incompressible pixels, so the file is as big as its pixel count")
    ap.add_argument("--transparent-below", type=int, default=0,
                    help="make the bottom N layers fully transparent")
    a = ap.parse_args()

    jobs = ({name: dict(path=os.path.join(a.out, name), **spec)
             for name, spec in MATRIX.items()} if a.matrix else
            {os.path.basename(a.out): dict(path=a.out, width=a.width, height=a.height,
                                           layers=a.layers, noise=a.noise,
                                           transparent_below=a.transparent_below)})
    for name, spec in jobs.items():
        path = spec.pop("path")
        size, top = build(path, **spec)
        # A --noise file's pixels are random, so it has no known flattened colour and must not
        # claim one: `compare-renders.py --expect` would then report a failure that is the
        # generator lying rather than the decoder being wrong.
        flat = "random pixels, no known colour" if spec.get("noise") else f"flattens to rgb{top}"
        print(f"{name}: {size / 1048576:.1f} MB, {flat}")


if __name__ == "__main__":
    main()
