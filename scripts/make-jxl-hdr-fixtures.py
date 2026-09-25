"""Two 16-bit PNGs of the SAME scene: one tagged PQ/BT.2020 (cICP 9,16,0,1), one BT.709
(cICP 1,1,0,1). cjxl keeps the cICP as the JXL colour encoding, which gives a PQ JPEG XL
and its SDR twin - the shape issue #38 describes. Scene: a grey ramp plus six colour
patches, reference white at 203 nits so a correct render puts the ramp's top near sRGB white.

The scene and its transfer functions live in `hdr_scene.py`, shared with the HEIC twins and the
WIC probes; this script owns only the PNG container.
"""
import struct, sys, zlib
from pathlib import Path

from hdr_scene import H, W, render_rows

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)


def png(path, cicp, rgb16_rows):
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)
    raw = b"".join(b"\x00" + row for row in rgb16_rows)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 16, 2, 0, 0, 0))
    data += chunk(b"cICP", bytes(cicp))
    data += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    path.write_bytes(data)


pq_rows, sdr_rows = render_rows(">")  # PNG is big-endian

png(out / "scene-pq2020.png", (9, 16, 0, 1), pq_rows)
png(out / "scene-sdr709.png", (1, 13, 0, 1), sdr_rows)  # 13 = sRGB transfer
print("wrote", out / "scene-pq2020.png", out / "scene-sdr709.png")
