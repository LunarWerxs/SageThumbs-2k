"""Growers for documents whose size lives in their body: a PDF object, EPS PostScript,
a compound file's sectors (see ballast.py)."""

import struct

from sparse import SparseWriter


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


def _placed_streams(entries, cutoff, chain):
    """The streams that live in whole sectors, with their sector chains: the root's mini-stream
    container, and every stream at or past the cutoff."""
    placed = []
    for e in entries:
        kind, start, length = e[66], struct.unpack("<I", e[116:120])[0], struct.unpack("<Q", e[120:128])[0]
        if (kind == 5 or (kind == 2 and length >= cutoff)) and start < 0xFFFFFFFA and length:
            placed.append((e, chain(start)))
    return placed


def _fat_size(sectors_mapped, per):
    """FAT and DIFAT sector counts that map `sectors_mapped` sectors and themselves: grown until
    they cover their own sectors too."""
    nfat, ndif = 1, 0
    while True:
        need = -(-(sectors_mapped + nfat + ndif) // per)
        need_dif = max(0, -(-(need - 109) // (per - 1)))
        if need <= nfat and need_dif <= ndif:
            return nfat, ndif
        nfat, ndif = max(nfat, need), max(ndif, need_dif)


class _Layout:
    """The sectors after the ballast, laid out one run at a time, and the FAT that chains them."""

    def __init__(self, first, fat_len, ss):
        self.at, self.fat, self.sectors, self.ss = first, [_FREE] * fat_len, [], ss

    def lay(self, blocks):
        """Append `blocks` (sector bytes) as one chain; its first sector number."""
        first = self.at
        for k, block in enumerate(blocks):
            self.fat[self.at] = self.at + 1 if k + 1 < len(blocks) else _END
            self.sectors.append(block)
            self.at += 1
        return first

    def mark(self, count, value):
        """Mark the next `count` sectors (the FAT's or the DIFAT's own) as `value`; the first."""
        first = self.at
        for k in range(count):
            self.fat[first + k] = value
        self.at += count
        return first


def _header_and_difat(data, head, per, fat_list, where):
    """The rewritten header, and the DIFAT sectors listing the FAT past the header's 109.
    `where` = (nfat, ndif, dir_at, dif_at, new_minifat)."""
    nfat, ndif, dir_at, dif_at, new_minifat = where
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
    return header, difat


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
    placed = _placed_streams(entries, cutoff, chain)
    body = sum(len(c) for _, c in placed) + len(minifat) + len(dir_chain)
    sectors = max(0, (size - head) // ss)
    ballast = max(0, sectors - body - sectors // (per - 1) - 2)
    nfat, ndif = _fat_size(ballast + body, per)
    out = _Layout(ballast, nfat * per, ss)
    for e, c in placed:
        e[116:120] = struct.pack("<I", out.lay([sect(s) for s in c]))
    new_minifat = out.lay([sect(s) for s in minifat]) if minifat else _END
    new_dir = b"".join(entries)
    dir_at = out.lay([new_dir[k * ss:(k + 1) * ss] for k in range(len(dir_chain))])
    fat_at = out.mark(nfat, _FATSECT)
    dif_at = out.mark(ndif, _DIFSECT)
    header, difat = _header_and_difat(data, head, per, list(range(fat_at, fat_at + nfat)),
                                      (nfat, ndif, dir_at, dif_at, new_minifat))
    with SparseWriter(dst) as w:
        w.write(bytes(header))
        w.zeros(ballast * ss)
        for s in out.sectors:
            w.write(s)
        w.write(struct.pack(f"<{len(out.fat)}I", *out.fat))
        for d in difat:
            w.write(d)
