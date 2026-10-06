"""Grower for a PDF whose size is its page count: a scanned book (see ballast.py).

Issue #59: Windows' PDF engine visits every page object while it opens a document, and in a
scanned book each page object sits a page's scan after the last. 3.6.0 spent a megabyte of read
allowance per visit and gave up near page 190, so no scanned book past the input ceiling ever
had a thumbnail, while `pdf-body` (one gap before the index) passed. This grower keeps the
sample whole and adds an incremental update: the sample's page tree first, so page one is the
sample's page one, then blank pages, each after a scan-sized stream."""

import re
import zlib

from sparse import SparseWriter

# Pages the twin has: 3.6.0 gave up near 190.
BOOK_PAGES = 300

# A scan's bytes. Not zeros: a run of NUL inside a stream makes Windows' engine read the whole
# file (measured 2026-10-06: the same 300-page twin pulled the entire 192 MiB read budget with
# NUL scans, and rendered with 0xFF ones), and no compressed scan is a NUL run. So this grower
# writes real bytes, and is PHYSICAL (ballast.py).
_SCAN = b"\xff" * (1 << 20)

_REF = rb"(\d+)\s+\d+\s+R"


def _value(text, key):
    """The raw value of `/key` in a dictionary's text: a reference, a nested dictionary, an
    array or one token."""
    m = re.search(rb"/" + key + rb"(?![A-Za-z0-9])\s*", text)
    if not m:
        return None
    rest = text[m.end():]
    ref = re.match(_REF, rest)
    if ref:
        return ref.group(0)
    pairs = {b"<<": (b"<<", b">>"), b"[": (b"[", b"]")}
    opener = next((o for o in pairs if rest.startswith(o)), None)
    if opener is None:
        return re.match(rb"/?[^\s/<>\[\]()]+", rest).group(0)
    open_, close = pairs[opener]
    depth, i = 0, 0
    while i < len(rest):
        if rest.startswith(open_, i):
            depth, i = depth + 1, i + len(open_)
        elif rest.startswith(close, i):
            depth, i = depth - 1, i + len(close)
            if depth == 0:
                return rest[:i]
        else:
            i += 1
    raise ValueError(f"unbalanced /{key.decode()}")


def _unpredict(raw, columns):
    """PNG-predicted rows (/Predictor 10..15) back to plain bytes."""
    out, prev = bytearray(), bytearray(columns)
    for at in range(0, len(raw), columns + 1):
        kind, row = raw[at], bytearray(raw[at + 1:at + 1 + columns])
        for i in range(len(row)):
            left = row[i - 1] if i else 0
            up, corner = prev[i], prev[i - 1] if i else 0
            if kind == 1:
                row[i] = (row[i] + left) & 0xFF
            elif kind == 2:
                row[i] = (row[i] + up) & 0xFF
            elif kind == 3:
                row[i] = (row[i] + (left + up) // 2) & 0xFF
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                row[i] = (row[i] + (left if pa <= pb and pa <= pc else up if pb <= pc else corner)) & 0xFF
        out += row
        prev = row
    return bytes(out)


class Pdf:
    """Just enough of a PDF to find its page tree: its cross-reference sections (tables or
    streams, newest first), its objects (direct or in object streams) and its trailer."""

    def __init__(self, data):
        self.data, self.where, self.trailer = data, {}, None
        at = int(re.search(rb"startxref\s+(\d+)", data[data.rfind(b"startxref"):]).group(1))
        self.startxref, seen = at, set()
        while at is not None and at not in seen:
            seen.add(at)
            trailer = self._table(at) if data.startswith(b"xref", at) else self._xref_stream(at)
            self.trailer = self.trailer or trailer
            prev = re.search(rb"/Prev\s+(\d+)", trailer)
            at = int(prev.group(1)) if prev else None

    def _table(self, at):
        end = self.data.index(b"trailer", at)
        words, i = self.data[at + 4:end].split(), 0
        while i < len(words):
            first, count = int(words[i]), int(words[i + 1])
            for k in range(count):
                off, flag = int(words[i + 2 + 3 * k]), words[i + 4 + 3 * k]
                self.where.setdefault(first + k, (1, off) if flag == b"n" else (0, 0))
            i += 2 + 3 * count
        return self.data[end:self.data.index(b"startxref", end)]

    def _xref_stream(self, at):
        head, body = self._stream_at(at)
        w = [int(x) for x in re.search(rb"/W\s*\[\s*(\d+)\s+(\d+)\s+(\d+)\s*\]", head).groups()]
        index = re.search(rb"/Index\s*\[([\d\s]+)\]", head)
        nums = [int(x) for x in index.group(1).split()] if index else [0, int(_value(head, b"Size"))]
        pos = 0
        for first, count in zip(nums[::2], nums[1::2]):
            for k in range(count):
                cut = [pos, pos + w[0], pos + w[0] + w[1], pos + sum(w)]
                kind, a = (int.from_bytes(body[cut[j]:cut[j + 1]], "big") for j in range(2))
                self.where.setdefault(first + k, (kind if w[0] else 1, a))
                pos += sum(w)
        return head

    def _stream_at(self, at):
        """An indirect stream object's dictionary and decoded bytes."""
        m = re.compile(rb"\d+\s+\d+\s+obj\s*").match(self.data, at)
        start = self.data.index(b"stream", m.end())
        head = self.data[m.end():start]
        body_at = start + 6 + (2 if self.data.startswith(b"\r\n", start + 6) else 1)
        length = _value(head, b"Length")
        n = int(length) if length and length.isdigit() else self.data.index(b"endstream", body_at) - body_at
        body = self.data[body_at:body_at + n]
        if b"/FlateDecode" in (_value(head, b"Filter") or b""):
            body = zlib.decompress(body)
        parms = _value(head, b"DecodeParms") or b""
        predictor = _value(parms, b"Predictor")
        if predictor and int(predictor) >= 10:
            body = _unpredict(body, int(_value(parms, b"Columns") or 1))
        return head, body

    def obj(self, num):
        """Object `num`'s text, between `obj` and `endobj`."""
        kind, a = self.where[num]
        if kind == 1:
            m = re.compile(rb"\d+\s+\d+\s+obj").match(self.data, a)
            return self.data[m.end():self.data.index(b"endobj", m.end())].strip()
        head, body = self._stream_at(self.where[a][1])
        count, first = int(_value(head, b"N")), int(_value(head, b"First"))
        index = [int(x) for x in body[:first].split()][:2 * count]
        offsets = dict(zip(index[::2], index[1::2]))
        starts = sorted(offsets.values()) + [len(body) - first]
        at = offsets[num]
        return body[first + at:first + starts[starts.index(at) + 1]].strip()


def pdf_pages(src, dst, size):
    """The sample, then an incremental update: blank pages to BOOK_PAGES, each after its
    ballast, a page tree holding the sample's tree first, and a cross-reference stream (eight-byte
    offsets, so past 4 GiB too) whose /Prev is the sample's own index."""
    data = open(src, "rb").read()
    pdf = Pdf(data)
    root = int(re.match(_REF, _value(pdf.trailer, b"Root")).group(1))
    catalog = pdf.obj(root)
    pages = int(re.match(_REF, _value(catalog, b"Pages")).group(1))
    tree = pdf.obj(pages)
    have = int(_value(tree, b"Count"))
    extra = max(1, BOOK_PAGES - have)
    top = int(_value(pdf.trailer, b"Size"))
    node, xref_num = top, top + 1 + 2 * extra
    gap = max(0, (size - len(data) - 4096 - 220 * extra - 12 * (2 * extra + 4)) // extra)
    offsets = {}
    with SparseWriter(dst) as w:
        w.write(data + b"\n")

        def emit(num, text):
            offsets[num] = w.tell()
            w.write(b"%d 0 obj\n" % num + text + b"\nendobj\n")

        kids = [b"%d 0 R" % pages]
        for i in range(extra):
            scan, page = top + 1 + 2 * i, top + 2 + 2 * i
            offsets[scan] = w.tell()
            w.write(b"%d 0 obj\n<< /Length %d >>\nstream\n" % (scan, gap))
            for at in range(0, gap, len(_SCAN)):
                w.write(_SCAN[:gap - at])
            w.write(b"\nendstream\nendobj\n")
            emit(page, b"<< /Type /Page /Parent %d 0 R /MediaBox [0 0 612 792] /Resources << >> >>" % node)
            kids.append(b"%d 0 R" % page)
        emit(pages, re.sub(rb"/Parent\s+" + _REF, b"", tree, count=1).replace(
            b"<<", b"<< /Parent %d 0 R" % node, 1))
        emit(node, b"<< /Type /Pages /Kids [%s] /Count %d >>" % (b" ".join(kids), have + extra))
        emit(root, re.sub(rb"/Pages\s*" + _REF, b"/Pages %d 0 R" % node, catalog, count=1))
        offsets[xref_num] = w.tell()
        nums = sorted(offsets)
        rows = b"".join(b"\x01" + offsets[n].to_bytes(8, "big") + b"\x00\x00" for n in nums)
        index = b" ".join(b"%d 1" % n for n in nums)
        w.write(b"%d 0 obj\n<< /Type /XRef /Size %d /Root %d 0 R /Prev %d /W [1 8 2] /Index [%s] "
                b"/Length %d >>\nstream\n" % (xref_num, xref_num + 1, root, pdf.startxref, index, len(rows))
                + rows + b"\nendstream\nendobj\nstartxref\n%d\n%%%%EOF\n" % offsets[xref_num])
