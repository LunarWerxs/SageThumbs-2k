"""Growers for archives: a stored entry added last, or a gap the header skips
(see ballast.py)."""

import struct

from sparse import CHUNK, SparseWriter


def _crc_zeros(n):
    """CRC-32 of `n` zero bytes, without holding them."""
    import zlib
    crc, zero = 0, bytes(CHUNK)
    while n:
        k = min(n, CHUNK)
        crc = zlib.crc32(zero[:k], crc)
        n -= k
    return crc


def _vint(v):
    out = bytearray()
    while True:
        b = v & 0x7F
        v >>= 7
        out.append(b | (0x80 if v else 0))
        if not v:
            return bytes(out)


def _read_vint(b, at):
    v = shift = 0
    while True:
        c = b[at]
        v |= (c & 0x7F) << shift
        at += 1
        shift += 7
        if not c & 0x80:
            return v, at


def _rar4_blocks(data):
    """(offset, type, total length) of each RAR 4 block after the marker."""
    at, out = 7, []
    while at + 7 <= len(data):
        _crc, kind, flags, size = struct.unpack("<HBHH", data[at:at + 7])
        add = struct.unpack("<I", data[at + 7:at + 11])[0] if flags & 0x8000 else 0
        if kind == 0x74 and flags & 0x100:
            add += struct.unpack("<I", data[at + 32:at + 36])[0] << 32
        out.append((at, kind, size + add))
        if size < 7:
            break
        at += size + add
    return out


def _rar4_entry(name, n):
    import zlib
    fields = struct.pack("<IIBIIBBHI", n & 0xFFFFFFFF, n & 0xFFFFFFFF, 2, _crc_zeros(n), 0x5A000000,
                         29, 0x30, len(name), 0x20)
    fields += struct.pack("<II", n >> 32, n >> 32) + name
    body = struct.pack("<BHH", 0x74, 0x8000 | 0x100, 7 + len(fields)) + fields
    return struct.pack("<H", zlib.crc32(body) & 0xFFFF) + body


def _rar5_blocks(data):
    """(offset, type, total length) of each RAR 5 block after the signature."""
    at, out = 8, []
    while at + 6 <= len(data):
        size, h = _read_vint(data, at + 4)
        kind, p = _read_vint(data, h)
        flags, p = _read_vint(data, p)
        if flags & 1:
            _, p = _read_vint(data, p)
        extra = _read_vint(data, p)[0] if flags & 2 else 0
        total = (h - at) + size + extra
        out.append((at, kind, total))
        if not size:
            break
        at += total
    return out


def _rar5_entry(name, n):
    import zlib
    fields = _vint(2) + _vint(2)  # header type: file; header flags: data area present
    fields += _vint(n)            # data size
    fields += _vint(4) + _vint(n) + _vint(0x20) + struct.pack("<I", _crc_zeros(n))
    fields += _vint(0) + _vint(2) + _vint(len(name)) + name  # stored, Windows, the name
    head = _vint(len(fields)) + fields
    return struct.pack("<I", zlib.crc32(head)) + head


def rar_last(src, dst, size):
    """A stored ballast entry added LAST, before the end-of-archive block: a big RAR is its
    entries, and the reader has to walk every header to list them (RAR 4 and RAR 5)."""
    data = open(src, "rb").read()
    rar5 = data.startswith(b"Rar!\x1a\x07\x01\x00")
    blocks = _rar5_blocks(data) if rar5 else _rar4_blocks(data)
    end_kind = 5 if rar5 else 0x7B
    cut = next((at for at, kind, _ in blocks if kind == end_kind), len(data))
    name = b"zzzz-bigfiles-ballast.bin"
    n = max(0, size - len(data) - 64)
    entry = (_rar5_entry if rar5 else _rar4_entry)(name, n)
    with SparseWriter(dst) as w:
        w.write(data[:cut] + entry)
        w.zeros(n)
        w.write(data[cut:])


def sevenzip_gap(src, dst, size):
    """Ballast between the packed streams and the end header, the start header's offset and
    CRC moved to match: a big 7z is its packed data, and its index sits at the far end."""
    import zlib
    data = open(src, "rb").read()
    offset, length, crc = struct.unpack("<QQI", data[12:32])
    at = 32 + offset
    n = max(0, size - len(data))
    start = struct.pack("<QQI", offset + n, length, crc)
    with SparseWriter(dst) as w:
        w.write(data[:8] + struct.pack("<I", zlib.crc32(start)) + start + data[32:at])
        w.zeros(n)
        w.write(data[at:])


def tar_last(src, dst, size):
    """A ballast entry added LAST, before the end-of-archive blocks: a big tar is its entries,
    and listing it means walking every header."""
    data = open(src, "rb").read()
    at = 0
    while at + 512 <= len(data) and data[at:at + 512] != bytes(512):
        n = int(data[at + 124:at + 136].split(b"\0")[0].strip() or b"0", 8)
        at += 512 + -(-n // 512) * 512
    n = max(0, size - len(data) - 1024)
    n -= n % 512
    header = bytearray(512)
    header[0:26] = b"zzzz-bigfiles-ballast.bin"
    header[100:108] = b"0000644\0"
    header[108:116] = b"0000000\0"
    header[116:124] = b"0000000\0"
    header[124:136] = b"%011o\0" % n
    header[136:148] = b"00000000000\0"
    header[156:157] = b"0"
    header[257:263] = b"ustar\0"
    header[263:265] = b"00"
    header[148:156] = b" " * 8
    header[148:156] = b"%06o\0 " % sum(header)
    with SparseWriter(dst) as w:
        w.write(data[:at] + bytes(header))
        w.zeros(n)
        w.write(data[at:])
