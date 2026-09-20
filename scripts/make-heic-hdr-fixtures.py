"""Twin HEICs of ONE scene for the HDR test in src/decode/tests/colour.rs: the PQ / BT.2020
10-bit picture (the shape an HDR-base HEIC is; iPhones write HLG, same code path), and its
sRGB / BT.709 control. Same scene as the JPEG XL / AVIF twins, and now literally the same
code: `hdr_scene.render_rows` draws it once, because the samples must reach the encoder at 16
bits and PIL reads a 16-bit RGB PNG down to 8.

    python scripts/make-heic-hdr-fixtures.py tests/fixtures/heic

Needs `pillow-heif` (pip), whose wheel bundles libheif with the x265 encoder. Lossless 4:4:4,
so the encoder contributes no error of its own.
"""
import sys
from pathlib import Path

import pillow_heif

from hdr_scene import H, W, render_rows

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)

pq_rows, sdr_rows = render_rows("<")  # pillow-heif's RGB;16 wants host-endian samples
pq, sdr = b"".join(pq_rows), b"".join(sdr_rows)


def write(name, data, primaries, transfer, matrix):
    img = pillow_heif.from_bytes(mode="RGB;16", size=(W, H), data=bytes(data))
    img.save(out / name, quality=-1, chroma=444, bit_depth=10,
             color_primaries=primaries, transfer_characteristics=transfer,
             matrix_coefficients=matrix, full_range_flag=1)
    print("wrote", out / name, (out / name).stat().st_size, "bytes")


write("scene-pq2020.heic", pq, 9, 16, 9)
write("scene-sdr709.heic", sdr, 1, 13, 1)
