"""Assembling the laid-out demo pages into one PDF file (see make-pdf-shot.py)."""

from __future__ import annotations

import sys
from pathlib import Path

from pdf_shot_document import PAGES
from pdf_shot_layout import layout_page
from pdf_shot_text import BOTTOM_Y, PAGE_H, PAGE_W


def build_pdf(path: Path) -> None:
    """Assemble the PDF. Objects are numbered 1..N and the xref offsets are computed from the
    real byte positions as the file is built, so this stays valid however the content grows."""
    contents = []
    for page_no, blocks in enumerate(PAGES, start=1):
        c = layout_page(blocks)
        if c.y_end is not None and c.y_end < BOTTOM_Y:
            sys.exit(
                f"page {page_no} overflows: layout finished at y={c.y_end:.0f}, below the "
                f"{BOTTOM_Y:.0f}pt bottom margin. Shorten a block or move it to a new page - "
                f"this script does not paginate."
            )
        contents.append(c)

    n_pages = len(PAGES)
    # 1 catalog, 2 pages, 3..(2+n) page objects, then n content streams, then 2 fonts.
    first_content = 3 + n_pages
    font_regular = first_content + n_pages
    font_bold = font_regular + 1
    total = font_bold

    objects: dict[int, bytes] = {}
    kids = " ".join(f"{3 + i} 0 R" for i in range(n_pages))
    objects[1] = b"<< /Type /Catalog /Pages 2 0 R >>"
    objects[2] = (
        f"<< /Type /Pages /Kids [{kids}] /Count {n_pages} >>".encode("latin-1")
    )
    for i in range(n_pages):
        objects[3 + i] = (
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.0f} {PAGE_H:.0f}] "
            f"/Resources << /Font << /F1 {font_regular} 0 R /F2 {font_bold} 0 R >> >> "
            f"/Contents {first_content + i} 0 R >>"
        ).encode("latin-1")
    for i, c in enumerate(contents):
        body = c.render()
        objects[first_content + i] = (
            f"<< /Length {len(body)} >>\nstream\n".encode("latin-1") + body + b"endstream"
        )
    objects[font_regular] = (
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
    )
    objects[font_bold] = (
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold "
        b"/Encoding /WinAnsiEncoding >>"
    )

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in range(1, total + 1):
        offsets[num] = len(out)
        out += f"{num} 0 obj\n".encode("latin-1") + objects[num] + b"\nendobj\n"

    xref_at = len(out)
    out += f"xref\n0 {total + 1}\n".encode("latin-1")
    out += b"0000000000 65535 f \n"
    for num in range(1, total + 1):
        out += f"{offsets[num]:010d} 00000 n \n".encode("latin-1")
    out += (
        f"trailer\n<< /Size {total + 1} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n"
    ).encode("latin-1")

    path.write_bytes(bytes(out))
