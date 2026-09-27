"""Laying one demo page out top-down, tables included (see make-pdf-shot.py)."""

from __future__ import annotations

import sys

from pdf_shot_text import (ACCENT, INK, MARGIN_X, MUTED, PAGE_W, RULE, TOP_Y, WHITE, ZEBRA, Content,
                           wrap)


def layout_page(blocks: list[tuple]) -> Content:
    """Lay one page's blocks out top-down. Nothing here paginates: PAGES is authored so each
    page's blocks fit, and `build_pdf` rejects a page whose cursor ran past BOTTOM_Y rather
    than silently drawing off the bottom edge, where nothing would ever see it - a PNG has no
    tests, so an overfull page would just quietly ship in the screenshot."""
    c = Content()
    col = PAGE_W - 2 * MARGIN_X
    y = TOP_Y

    for kind, payload in blocks:
        if kind == "kicker":
            c.text(MARGIN_X, y, payload, "F2", 9.0, ACCENT, spacing=1.4)
            y -= 26
        elif kind == "h1":
            c.text(MARGIN_X, y, payload, "F2", 23.0, ACCENT)
            y -= 20
        elif kind == "rule":
            c.line(MARGIN_X, y, PAGE_W - MARGIN_X, y, RULE)
            y -= 26
        elif kind == "h2":
            y -= 12
            c.text(MARGIN_X, y, payload, "F2", 15.0, ACCENT)
            y -= 12
            c.line(MARGIN_X, y, PAGE_W - MARGIN_X, y, RULE)
            y -= 22
        elif kind == "body":
            for ln in wrap(payload, "F1", 11.0, col):
                c.text(MARGIN_X, y, ln, "F1", 11.0, INK)
                y -= 16
            y -= 10
        elif kind == "footer":
            y -= 8
            c.text(MARGIN_X, y, payload, "F1", 9.0, MUTED)
            y -= 16
        elif kind == "bullets":
            for item in payload:
                lines = wrap(item, "F1", 11.0, col - 22)
                c.text(MARGIN_X + 8, y, "\u2022", "F1", 11.0, MUTED)
                for i, ln in enumerate(lines):
                    c.text(MARGIN_X + 22, y, ln, "F1", 11.0, INK)
                    y -= 16
                y -= 4
            y -= 8
        elif kind == "table":
            y = layout_table(c, payload, y, col)
        else:
            sys.exit(f"unknown block kind {kind!r}")

    c.y_end = y
    return c


def layout_table(c: Content, table: dict, y: float, col: float) -> float:
    """A header band, zebra rows, and a hairline under each row. Column widths are fixed
    fractions of the text column; the last one takes the remainder."""
    widths = [col * 0.26, col * 0.20, col * 0.54]
    xs = [MARGIN_X]
    for w in widths[:-1]:
        xs.append(xs[-1] + w)

    head_h = 26.0
    c.rect(MARGIN_X, y - head_h + 8, col, head_h, ACCENT)
    for x, w, label in zip(xs, widths, table["head"]):
        c.text(x + 10, y - 9, label, "F2", 10.5, WHITE)
    y -= head_h + 4

    for idx, row in enumerate(table["rows"]):
        wrapped = [wrap(cell, "F1", 10.0, w - 20) for cell, w in zip(row, widths)]
        rows_h = max(len(lines) for lines in wrapped) * 15.0 + 12.0
        if idx % 2 == 0:
            c.rect(MARGIN_X, y - rows_h + 11, col, rows_h, ZEBRA)
        for x, lines in zip(xs, wrapped):
            ty = y
            for ln in lines:
                c.text(x + 10, ty, ln, "F1", 10.0, INK)
                ty -= 15
        y -= rows_h
        c.line(MARGIN_X, y + 9, PAGE_W - MARGIN_X, y + 9, RULE, 0.5)

    return y - 16
