"""Growers that add bytes a format's readers skip: after its end, in a comment, in a
padding box or section, or its content repeated (see ballast.py)."""

import struct
import zipfile

from sparse import CHUNK, SparseWriter


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
