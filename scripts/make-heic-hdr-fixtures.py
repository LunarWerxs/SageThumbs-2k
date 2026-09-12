"""Twin HEICs of ONE scene for the HDR test in src/decode/tests/colour.rs: the PQ / BT.2020
10-bit picture (the shape an HDR-base HEIC is; iPhones write HLG, same code path), and its
sRGB / BT.709 control. Same scene as the JPEG XL / AVIF twins, drawn here again because the
samples must reach the encoder at 16 bits and PIL reads a 16-bit RGB PNG down to 8.

    python scripts/make-heic-hdr-fixtures.py tests/fixtures/heic

Needs `pillow-heif` (pip), whose wheel bundles libheif with the x265 encoder. Lossless 4:4:4,
so the encoder contributes no error of its own.
"""
import struct
import sys
from pathlib import Path

import pillow_heif

W, H = 320, 200
out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)


def pq_oetf(y_nits):
    y = max(y_nits, 0.0) / 10000.0
    m1, m2, c1, c2, c3 = 0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875
    yp = y ** m1
    return ((c1 + c2 * yp) / (1 + c3 * yp)) ** m2


def srgb_oetf(l):
    l = min(max(l, 0.0), 1.0)
    return 12.92 * l if l <= 0.0031308 else 1.055 * l ** (1 / 2.4) - 0.055


# Scene in linear light relative to 203-nit diffuse white (1.0 = 203 nits), BT.709 primaries.
def scene(x, y):
    if y < H // 2:
        v = x / (W - 1)
        return (v, v, v)
    patches = [(1, 0, 0), (0, 1, 0), (0, 0, 1), (1, 1, 0), (0, 1, 1), (1, 0, 1)]
    p = patches[min(x * 6 // W, 5)]
    return tuple(0.6 * c for c in p)


# BT.709 linear -> BT.2020 linear, for the PQ file's primaries.
M709_2020 = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]]

pq, sdr = bytearray(), bytearray()
for y in range(H):
    for x in range(W):
        r, g, b = scene(x, y)
        r2 = sum(M709_2020[0][i] * v for i, v in enumerate((r, g, b)))
        g2 = sum(M709_2020[1][i] * v for i, v in enumerate((r, g, b)))
        b2 = sum(M709_2020[2][i] * v for i, v in enumerate((r, g, b)))
        for v in (r2, g2, b2):
            pq += struct.pack("<H", int(round(pq_oetf(v * 203.0) * 65535)))
        for v in (r, g, b):
            sdr += struct.pack("<H", int(round(srgb_oetf(v) * 65535)))


def write(name, data, primaries, transfer, matrix):
    img = pillow_heif.from_bytes(mode="RGB;16", size=(W, H), data=bytes(data))
    img.save(out / name, quality=-1, chroma=444, bit_depth=10,
             color_primaries=primaries, transfer_characteristics=transfer,
             matrix_coefficients=matrix, full_range_flag=1)
    print("wrote", out / name, (out / name).stat().st_size, "bytes")


write("scene-pq2020.heic", pq, 9, 16, 9)
write("scene-sdr709.heic", sdr, 1, 13, 1)
