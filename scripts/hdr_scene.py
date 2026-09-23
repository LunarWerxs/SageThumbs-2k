"""The ONE copy of the HDR test scene and its transfer functions.

`make-jxl-hdr-fixtures.py`, `make-heic-hdr-fixtures.py` and `make-wic-probes.py` each carried
a byte-identical `pq_oetf` / `srgb_oetf` / `srgb_eotf` and the same BT.709 -> BT.2020 matrix,
and the first two also redrew the same 320x200 scene pixel for pixel. That is not a clone
detector mistaking two tables for each other: it is one piece of colour arithmetic pasted three
times, and three copies of an OETF is three chances for a fixture and the decoder test that
reads it to disagree about what the file is supposed to contain.

The scene: a grey ramp across the top half, six 0.6-scaled colour patches across the bottom, in
linear light relative to a 203-nit diffuse white (so 1.0 = 203 nits and a correct render puts
the ramp's top near sRGB white). `render_rows` returns it twice - once through PQ with BT.2020
primaries, once through the sRGB transfer with BT.709 - which is exactly the twin-file shape
`crates/codecs/src/decode/tests/colour.rs` asserts on.

Import it as a sibling (`from hdr_scene import ...`); `sys.path[0]` is the running script's own
directory, which is `scripts/`. The generators keep their own names and command lines, so
`make-tiff-hdr-fixtures.py` and `make-avif-hdr-fixtures.py`, which subprocess the JPEG XL one
for its PNGs, are unaffected.
"""
import struct

W, H = 320, 200

# BT.709 linear -> BT.2020 linear, the inverse of `primaries_to_bt709` in crates/codecs/src/decode/cicp.rs.
M709_2020 = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]]


def pq_oetf(y_nits):
    """SMPTE ST 2084 (PQ): absolute luminance in nits -> a code value in [0, 1]."""
    y = max(y_nits, 0.0) / 10000.0
    m1, m2, c1, c2, c3 = 0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875
    yp = y ** m1
    return ((c1 + c2 * yp) / (1 + c3 * yp)) ** m2


def srgb_oetf(l):
    """Linear light in [0, 1] -> an sRGB-encoded code value in [0, 1]."""
    l = min(max(l, 0.0), 1.0)
    return 12.92 * l if l <= 0.0031308 else 1.055 * l ** (1 / 2.4) - 0.055


def srgb_eotf(v):
    """An sRGB-encoded code value in [0, 1] -> linear light. The inverse of `srgb_oetf`."""
    return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4


def scene(x, y):
    """The scene in linear light relative to 203-nit diffuse white, BT.709 primaries."""
    if y < H // 2:
        v = x / (W - 1)
        return (v, v, v)
    patches = [(1, 0, 0), (0, 1, 0), (0, 0, 1), (1, 1, 0), (0, 1, 1), (1, 0, 1)]
    p = patches[min(x * 6 // W, 5)]
    return tuple(0.6 * c for c in p)


def render_rows(byteorder=">"):
    """The scene as twin 16-bit RGB rasters: `(pq_rows, sdr_rows)`, one packed row per scanline.

    `pq_rows` is PQ-encoded with BT.2020 primaries; `sdr_rows` is sRGB-encoded with BT.709.
    `byteorder` is a struct prefix - PNG wants big-endian (`">"`), pillow-heif's `RGB;16`
    wants the host's little-endian (`"<"`).
    """
    fmt = byteorder + "H"
    pq_rows, sdr_rows = [], []
    for y in range(H):
        pq_row, sdr_row = bytearray(), bytearray()
        for x in range(W):
            r, g, b = scene(x, y)
            wide = [sum(M709_2020[i][j] * v for j, v in enumerate((r, g, b))) for i in range(3)]
            for v in wide:
                pq_row += struct.pack(fmt, int(round(pq_oetf(v * 203.0) * 65535)))
            for v in (r, g, b):
                sdr_row += struct.pack(fmt, int(round(srgb_oetf(v) * 65535)))
        pq_rows.append(bytes(pq_row))
        sdr_rows.append(bytes(sdr_row))
    return pq_rows, sdr_rows
