"""Two 16-bit PNGs of the SAME scene: one tagged PQ/BT.2020 (cICP 9,16,0,1), one BT.709
(cICP 1,1,0,1). cjxl keeps the cICP as the JXL colour encoding, which gives a PQ JPEG XL
and its SDR twin - the shape issue #38 describes. Scene: a grey ramp plus six colour
patches, reference white at 203 nits so a correct render puts the ramp's top near sRGB white.
"""
import struct, sys, zlib, math
from pathlib import Path

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

# scene in linear light relative to 203-nit diffuse white (1.0 = 203 nits), BT.709 primaries
def scene(x, y):
    if y < H // 2:
        v = x / (W - 1)
        return (v, v, v)
    patches = [(1, 0, 0), (0, 1, 0), (0, 0, 1), (1, 1, 0), (0, 1, 1), (1, 0, 1)]
    p = patches[min(x * 6 // W, 5)]
    return tuple(0.6 * c for c in p)

# BT.709 linear -> BT.2020 linear (for the PQ file's primaries)
M709_2020 = [[0.6274, 0.3293, 0.0433], [0.0691, 0.9195, 0.0114], [0.0164, 0.0880, 0.8956]]

def png(path, cicp, rgb16_rows):
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)
    raw = b"".join(b"\x00" + row for row in rgb16_rows)
    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 16, 2, 0, 0, 0))
    data += chunk(b"cICP", bytes(cicp))
    data += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    path.write_bytes(data)

pq_rows, sdr_rows = [], []
for y in range(H):
    pq_row, sdr_row = bytearray(), bytearray()
    for x in range(W):
        r, g, b = scene(x, y)
        r2 = sum(M709_2020[0][i] * v for i, v in enumerate((r, g, b)))
        g2 = sum(M709_2020[1][i] * v for i, v in enumerate((r, g, b)))
        b2 = sum(M709_2020[2][i] * v for i, v in enumerate((r, g, b)))
        for v in (r2, g2, b2):
            pq_row += struct.pack(">H", int(round(pq_oetf(v * 203.0) * 65535)))
        for v in (r, g, b):
            sdr_row += struct.pack(">H", int(round(srgb_oetf(v) * 65535)))
    pq_rows.append(bytes(pq_row)); sdr_rows.append(bytes(sdr_row))

png(out / "scene-pq2020.png", (9, 16, 0, 1), pq_rows)
png(out / "scene-sdr709.png", (1, 13, 0, 1), sdr_rows)  # 13 = sRGB transfer
print("wrote", out / "scene-pq2020.png", out / "scene-sdr709.png")
