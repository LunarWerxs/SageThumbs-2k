"""Grow a file past the size gates WITHOUT changing its picture, cheaply.

Every strategy adds bytes the format's readers skip - ballast - and writes the zeros as a
sparse range (NTFS FSCTL_SET_SPARSE), so a 5 GB twin of a 30 KB sample costs no disk and a
second to make. Only the text strategy writes real bytes: a comment is not zeros.

The strategies, and which files take which, are the gate's business (`bigfiles.py`); each one
here is a pure "src -> dst of `size` bytes, same picture" transform. Which formats tolerate
which is MEASURED (tailprobe.py and the gate's own normal-vs-big comparison), not assumed.
"""

import ctypes
import msvcrt
import os
import struct
import zipfile
from ctypes import wintypes

FSCTL_SET_SPARSE = 0x000900C4
CHUNK = 1 << 20


def _set_sparse(f):
    handle = msvcrt.get_osfhandle(f.fileno())
    returned = wintypes.DWORD()
    ok = ctypes.windll.kernel32.DeviceIoControl(
        wintypes.HANDLE(handle), FSCTL_SET_SPARSE, None, 0, None, 0, ctypes.byref(returned), None)
    if not ok:
        raise OSError("FSCTL_SET_SPARSE failed")


class SparseWriter:
    """A seekable file whose all-zero writes become holes instead of disk writes."""

    def __init__(self, path):
        self.f = open(path, "w+b")
        _set_sparse(self.f)

    def write(self, b):
        # `count` runs in C; `any(b)` walks a megabyte as Python ints and made a 5 GB twin
        # take minutes.
        if b and bytes(b).count(0) == len(b):
            self.f.seek(len(b), os.SEEK_CUR)
            return len(b)
        return self.f.write(b)

    def zeros(self, n):
        self.f.seek(n, os.SEEK_CUR)

    def tell(self):
        return self.f.tell()

    def seek(self, *a):
        return self.f.seek(*a)

    def flush(self):
        self.f.flush()

    def close(self):
        end = self.f.tell()
        self.f.seek(0, os.SEEK_END)
        if self.f.tell() < end:
            # A trailing hole still has to count as file length. NOT `truncate`: on Windows it
            # extends by WRITING zeros (the CRT's _chsize), 5 GB of real disk writes per twin.
            # One real byte at the very end leaves everything before it a hole.
            self.f.seek(end - 1)
            self.f.write(b"\0")
        self.f.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def tail(src, dst, size):
    """Zeros after the file's own end: for formats whose readers stop at their end."""
    data = open(src, "rb").read()
    with SparseWriter(dst) as w:
        w.write(data)
        w.zeros(max(0, size - len(data)))


def zip_last(src, dst, size):
    """The zip rebuilt with a stored ballast entry LAST, so the central directory sits at the
    far end of a big file, as it does in a real 1 GB comic or project archive."""
    with zipfile.ZipFile(src) as zin, SparseWriter(dst) as w:
        with zipfile.ZipFile(w, "w", allowZip64=True) as zout:
            for info in zin.infolist():
                zout.writestr(info, zin.read(info.filename), compress_type=info.compress_type)
            room = size - w.tell() - 1024
            ballast = zipfile.ZipInfo("zzzz-bigfiles-ballast.bin")
            ballast.compress_type = zipfile.ZIP_STORED
            with zout.open(ballast, "w", force_zip64=room >= (1 << 31)) as entry:
                zero = bytes(CHUNK)
                left = max(0, room)
                while left:
                    n = min(left, CHUNK)
                    entry.write(zero[:n])
                    left -= n


def psd_layers(src, dst, size):
    """Ballast inside the Layer and Mask section, where a real big document's size lives."""
    data = open(src, "rb").read()
    psb = struct.unpack(">H", data[4:6])[0] == 2
    at = 26
    at += 4 + struct.unpack(">I", data[at:at + 4])[0]          # colour mode data
    at += 4 + struct.unpack(">I", data[at:at + 4])[0]          # image resources
    fmt, width = (">Q", 8) if psb else (">I", 4)
    length = struct.unpack(fmt, data[at:at + width])[0]
    body = at + width
    extra = max(0, size - len(data))
    if not psb:
        extra = min(extra, 0xFFFFFFFF - length)
    with SparseWriter(dst) as w:
        w.write(data[:at])
        w.write(struct.pack(fmt, length + extra))
        w.write(data[body:body + length])
        w.zeros(extra)
        w.write(data[body + length:])


def isobmff_free(src, dst, size):
    """A top-level `free` box after the file's last box: valid ISO BMFF, skipped by readers."""
    data = open(src, "rb").read()
    room = max(16, size - len(data))
    with SparseWriter(dst) as w:
        w.write(data)
        if room < (1 << 32):
            w.write(struct.pack(">I", room) + b"free")
            w.zeros(room - 8)
        else:
            w.write(struct.pack(">I", 1) + b"free" + struct.pack(">Q", room))
            w.zeros(room - 16)


def _ape_tag_start(data):
    """Where an APEv2 tag (and an ID3v1 tag after it) begins, or None."""
    end = len(data)
    if end >= 128 and data[end - 128:end - 125] == b"TAG":
        end -= 128
    footer = data[end - 32:end]
    if len(footer) < 32 or not footer.startswith(b"APETAGEX"):
        return None
    tag_size, _, flags = struct.unpack("<III", footer[12:24])
    start = end - tag_size
    if flags & (1 << 31):
        start -= 32  # the tag also has a header
    return start if start >= 0 else None


def before_tail_tag(src, dst, size):
    """Ballast between the audio and an APEv2 tag, which readers find from the file's END."""
    data = open(src, "rb").read()
    start = _ape_tag_start(data)
    if start is None:
        raise ValueError("no APEv2 tag at the end")
    with SparseWriter(dst) as w:
        w.write(data[:start])
        w.zeros(max(0, size - len(data)))
        w.write(data[start:])


def xml_comment(src, dst, size):
    """A long comment before the root's closing tag (real bytes: a comment is text)."""
    data = open(src, "rb").read()
    close = data.rstrip().rfind(b"</")
    if close < 0:
        raise ValueError("no closing tag")
    pad = max(0, size - len(data) - 7)
    with open(dst, "wb") as f:
        f.write(data[:close] + b"<!--")
        spaces = b" " * CHUNK
        while pad:
            n = min(pad, CHUNK)
            f.write(spaces[:n])
            pad -= n
        f.write(b"-->" + data[close:])


def fits_hdu(src, dst, size):
    """An IMAGE extension HDU after the primary one, its data unit the ballast: valid FITS, and
    the shape of a real big FITS file (many HDUs). Readers show the primary HDU."""
    data = open(src, "rb").read()
    n = max(2880, size - len(data) - 2880)
    n -= n % 2880
    cards = [
        "XTENSION= 'IMAGE   '",
        "BITPIX  =                    8",
        "NAXIS   =                    1",
        f"NAXIS1  = {n:>20}",
        "PCOUNT  =                    0",
        "GCOUNT  =                    1",
        "END",
    ]
    header = "".join(c.ljust(80) for c in cards).ljust(2880).encode("ascii")
    with SparseWriter(dst) as w:
        w.write(data)
        w.write(header)
        w.zeros(n)


def repeat(src, dst, size):
    """The stream repeated until it is big enough - what a long recording IS for the formats
    whose readers need content all the way through: a chained Ogg stream (valid, and exactly
    what internet radio sends), or a transport stream holding hours of programme. Stuffing
    alone is not honest there: 300 MB of null packets after 40 KB of video is no real file,
    and Windows' TS source gives up on it. Real bytes, so made at the smallest size only."""
    data = open(src, "rb").read()
    # A copy count of 1 mod 10: the frame a reader takes at 30 % of the whole (the default
    # VideoOffset) is then 30 % into one copy, the frame it takes from the normal file. Any
    # other count lands on a different moment, which is not a different-picture bug.
    copies = max(1, -(-size // len(data)))
    copies += (1 - copies) % 10
    with open(dst, "wb") as f:
        for _ in range(copies):
            f.write(data)


def pdf_body(src, dst, size):
    """Ballast in the body, just before the last cross-reference section, with `startxref`
    moved to match: the shape of a real big PDF, whose size is images and fonts between the
    header and the xref at the end - so its first page is NOT in its first megabytes, and a
    reader must use the index. NUL is PDF whitespace, so the run is a valid gap."""
    import re
    data = open(src, "rb").read()
    at = data.rfind(b"startxref")
    m = re.match(rb"startxref(\s+)(\d+)", data[at:])
    if at < 0 or not m:
        raise ValueError(f"{src}: no startxref")
    xref = int(m.group(2))
    room = max(0, size - len(data) - 16)
    tail = data[xref:at] + b"startxref" + m.group(1) + str(xref + room).encode() + data[at + m.end():]
    with SparseWriter(dst) as w:
        w.write(data[:xref])
        w.zeros(room)
        w.write(tail)


def eps_postscript(src, dst, size):
    """A DOS binary EPS whose PostScript section is the big part and whose TIFF/WMF preview
    follows it, the offsets in the 30-byte header moved to match: how a real big EPS looks
    (a placed photo in the PostScript), so its preview is at the far end of the file."""
    data = open(src, "rb").read()
    magic, ps_at, ps_len, wmf_at, wmf_len, tif_at, tif_len, check = struct.unpack("<IIIIIIIH", data[:30])
    if magic != 0xC6D3D0C5:
        raise ValueError(f"{src}: not a DOS binary EPS")
    end = ps_at + ps_len
    room = min(max(0, size - len(data)), 0xFFFFFFFF - max(end, wmf_at, tif_at, len(data)))
    moved = lambda at: at + room if at >= end else at
    header = struct.pack("<IIIIIIIH", magic, ps_at, ps_len + room, moved(wmf_at) if wmf_len else 0, wmf_len,
                         moved(tif_at) if tif_len else 0, tif_len, 0xFFFF)
    with SparseWriter(dst) as w:
        w.write(header + data[30:end])
        w.zeros(room)
        w.write(data[end:])


_FREE, _END, _FATSECT, _DIFSECT = 0xFFFFFFFF, 0xFFFFFFFE, 0xFFFFFFFD, 0xFFFFFFFC


def _cfb_parse(data):
    """A compound file's sector size, header length, sector reader, FAT and chain walker."""
    ss = 1 << struct.unpack("<H", data[0x1E:0x20])[0]
    head = 512 if ss == 512 else ss
    sect = lambda i: data[head + i * ss:head + (i + 1) * ss]
    per = ss // 4
    fat_secs = [v for v in struct.unpack("<109I", data[0x4C:0x4C + 436]) if v < 0xFFFFFFFA]
    d = struct.unpack("<I", data[0x44:0x48])[0]
    while d < 0xFFFFFFFA:
        vals = struct.unpack(f"<{per}I", sect(d))
        fat_secs += [v for v in vals[:-1] if v < 0xFFFFFFFA]
        d = vals[-1]
    fat = [v for f in fat_secs for v in struct.unpack(f"<{per}I", sect(f))]

    def chain(start):
        out, s = [], start
        while s < 0xFFFFFFFA and len(out) <= len(fat):
            out.append(s)
            s = fat[s]
        return out
    return ss, head, sect, chain


def ole_front(src, dst, size):
    """A compound file (legacy Office, 3ds Max, SolidWorks, Outlook) rebuilt with its size in
    FRONT: free sectors first, then every stream, the mini-FAT, the directory, the FAT and the
    DIFAT at the far end - where a big document that grew over many saves keeps them, and so
    nothing a reader needs is in its first megabytes. Every stream's bytes are unchanged."""
    data = open(src, "rb").read()
    ss, head, sect, chain = _cfb_parse(data)
    per = ss // 4
    cutoff = struct.unpack("<I", data[0x38:0x3C])[0]
    dir_chain = chain(struct.unpack("<I", data[0x30:0x34])[0])
    dirb = b"".join(sect(s) for s in dir_chain)
    entries = [bytearray(dirb[i:i + 128]) for i in range(0, len(dirb), 128)]
    first_minifat = struct.unpack("<I", data[0x3C:0x40])[0]
    minifat = chain(first_minifat) if first_minifat < 0xFFFFFFFA else []
    # The streams that live in whole sectors: the root's mini-stream container, and every
    # stream at or past the cutoff.
    placed = []
    for e in entries:
        kind, start, length = e[66], struct.unpack("<I", e[116:120])[0], struct.unpack("<Q", e[120:128])[0]
        if (kind == 5 or (kind == 2 and length >= cutoff)) and start < 0xFFFFFFFA and length:
            placed.append((e, chain(start)))
    body = sum(len(c) for _, c in placed) + len(minifat) + len(dir_chain)
    sectors = max(0, (size - head) // ss)
    ballast = max(0, sectors - body - sectors // (per - 1) - 2)
    # The FAT has to map every sector, its own and the DIFAT's included: grow both until
    # they cover themselves.
    nfat, ndif = 1, 0
    while True:
        need = -(-(ballast + body + nfat + ndif) // per)
        need_dif = max(0, -(-(need - 109) // (per - 1)))
        if need <= nfat and need_dif <= ndif:
            break
        nfat, ndif = max(nfat, need), max(ndif, need_dif)
    fat = [_FREE] * (nfat * per)
    at = ballast
    out_sectors = []

    def lay(sectors):
        nonlocal at
        first = at
        for k, s in enumerate(sectors):
            fat[at] = at + 1 if k + 1 < len(sectors) else _END
            out_sectors.append(sect(s))
            at += 1
        return first
    for e, c in placed:
        e[116:120] = struct.pack("<I", lay(c))
    new_minifat = lay(minifat) if minifat else _END
    dir_at = at
    new_dir = b"".join(entries)
    for k in range(len(dir_chain)):
        fat[at] = at + 1 if k + 1 < len(dir_chain) else _END
        out_sectors.append(new_dir[k * ss:(k + 1) * ss])
        at += 1
    fat_at = at
    for k in range(nfat):
        fat[fat_at + k] = _FATSECT
    dif_at = fat_at + nfat
    for k in range(ndif):
        fat[dif_at + k] = _DIFSECT
    fat_list = list(range(fat_at, fat_at + nfat))
    header = bytearray(data[:head])
    header[0x2C:0x30] = struct.pack("<I", nfat)
    header[0x30:0x34] = struct.pack("<I", dir_at)
    header[0x3C:0x40] = struct.pack("<I", new_minifat)
    header[0x44:0x48] = struct.pack("<I", dif_at if ndif else _END)
    header[0x48:0x4C] = struct.pack("<I", ndif)
    header[0x4C:0x4C + 436] = struct.pack("<109I", *(fat_list[:109] + [_FREE] * (109 - min(109, nfat))))
    difat = []
    rest = fat_list[109:]
    for k in range(ndif):
        chunk = rest[k * (per - 1):(k + 1) * (per - 1)]
        nxt = dif_at + k + 1 if k + 1 < ndif else _END
        difat.append(struct.pack(f"<{per}I", *(chunk + [_FREE] * (per - 1 - len(chunk)) + [nxt])))
    with SparseWriter(dst) as w:
        w.write(bytes(header))
        w.zeros(ballast * ss)
        for s in out_sectors:
            w.write(s)
        packed = struct.pack(f"<{len(fat)}I", *fat)
        w.write(packed)
        for d in difat:
            w.write(d)


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


STRATEGIES = {
    "tail": tail,
    "zip-last": zip_last,
    "psd-layers": psd_layers,
    "isobmff-free": isobmff_free,
    "before-tail-tag": before_tail_tag,
    "xml-comment": xml_comment,
    "fits-hdu": fits_hdu,
    "repeat": repeat,
    "pdf-body": pdf_body,
    "eps-postscript": eps_postscript,
    "ole-front": ole_front,
    "rar-last": rar_last,
    "7z-gap": sevenzip_gap,
    "tar-last": tar_last,
}

# Strategies that write real bytes: made once, at the smallest size only.
PHYSICAL = {"xml-comment", "repeat"}
