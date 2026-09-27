"""Page geometry, base-14 text metrics and one page's content stream (see make-pdf-shot.py)."""

from __future__ import annotations


PAGE_W, PAGE_H = 612.0, 792.0
MARGIN_X = 72.0
TOP_Y = 720.0
BOTTOM_Y = 84.0

INK = (0.10, 0.10, 0.11)
MUTED = (0.42, 0.44, 0.47)
ACCENT = (0.04, 0.29, 0.20)
RULE = (0.80, 0.82, 0.84)
ZEBRA = (0.945, 0.960, 0.952)
WHITE = (1.0, 1.0, 1.0)

# ---- Base-14 metrics ---------------------------------------------------------------------
#
# Adobe's published Helvetica / Helvetica-Bold widths for ASCII 32..126, in 1/1000 em. Needed
# only so paragraphs wrap where a reader would wrap them; an approximation here shows up as
# ragged lines and overfull rows in a screenshot whose whole job is to look like a document.

_HELV = (
    "278 278 355 556 556 889 667 191 333 333 389 584 278 333 278 278 556 556 556 556 556 "
    "556 556 556 556 556 278 278 584 584 584 556 1015 667 667 722 722 667 611 778 722 278 "
    "500 667 556 833 722 778 667 778 722 667 611 722 667 944 667 667 611 278 278 278 469 "
    "556 333 556 556 500 556 556 278 556 556 222 222 500 222 833 556 556 556 556 333 500 "
    "278 556 500 722 500 500 500 334 260 334 584"
)
_HELV_BOLD = (
    "278 333 474 556 556 889 722 238 333 333 389 584 278 333 278 278 556 556 556 556 556 "
    "556 556 556 556 556 333 333 584 584 584 611 975 722 722 722 722 667 611 778 722 278 "
    "556 722 611 833 722 778 667 778 722 667 611 722 667 944 667 667 611 333 278 333 584 "
    "556 333 556 611 556 611 556 333 611 611 278 278 556 278 889 611 611 611 611 389 556 "
    "333 611 556 778 556 556 500 389 280 389 584"
)

WIDTHS = {
    "F1": [int(w) for w in _HELV.split()],
    "F2": [int(w) for w in _HELV_BOLD.split()],
}


def text_width(s: str, font: str, size: float) -> float:
    """Width of `s` in points. Anything outside ASCII 32..126 is charged as a space."""
    table = WIDTHS[font]
    total = 0
    for ch in s:
        code = ord(ch)
        total += table[code - 32] if 32 <= code <= 126 else table[0]
    return total * size / 1000.0


def wrap(s: str, font: str, size: float, width: float) -> list[str]:
    """Greedy word wrap to `width` points."""
    lines: list[str] = []
    line = ""
    for word in s.split():
        candidate = word if not line else line + " " + word
        if text_width(candidate, font, size) <= width or not line:
            line = candidate
        else:
            lines.append(line)
            line = word
    if line:
        lines.append(line)
    return lines


# ---- A very small PDF writer ---------------------------------------------------------------


def esc(s: str) -> str:
    """Escape a string for a PDF literal `( ... )`, and fold the few non-Latin-1 characters
    used here onto their WinAnsiEncoding byte. U+2022 BULLET is 0x95 in WinAnsi; without this
    it encodes to `?` and every bullet in the shot renders as a question mark (it did)."""
    s = s.replace("•", "\x95").replace("·", "\xb7")
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


class Content:
    """One page's content stream, built as a list of operator lines."""

    def __init__(self) -> None:
        self.ops: list[str] = []
        # Where the layout cursor finished, so `build_pdf` can reject a page that ran past the
        # bottom margin. Set by `layout_page`; None means nothing laid this page out.
        self.y_end: float | None = None

    def rect(self, x: float, y: float, w: float, h: float, color: tuple) -> None:
        r, g, b = color
        self.ops.append(f"{r:.3f} {g:.3f} {b:.3f} rg {x:.2f} {y:.2f} {w:.2f} {h:.2f} re f")

    def line(self, x1: float, y1: float, x2: float, y2: float, color: tuple,
             width: float = 0.75) -> None:
        r, g, b = color
        self.ops.append(
            f"{r:.3f} {g:.3f} {b:.3f} RG {width:.2f} w "
            f"{x1:.2f} {y1:.2f} m {x2:.2f} {y2:.2f} l S"
        )

    def text(self, x: float, y: float, s: str, font: str, size: float, color: tuple,
             spacing: float = 0.0) -> None:
        r, g, b = color
        # `Tc` is PERSISTENT text state, not per-BT: it survives ET and applies to every later
        # text run in the stream. Emitting it only when non-zero meant the kicker's 1.4pt
        # tracking leaked into the whole rest of the page, so every line rendered wider than
        # `text_width` measured and ran off the right margin. Always state it.
        self.ops.append(
            f"BT /{font} {size:.2f} Tf {spacing:.2f} Tc {r:.3f} {g:.3f} {b:.3f} rg "
            f"1 0 0 1 {x:.2f} {y:.2f} Tm ({esc(s)}) Tj ET"
        )

    def render(self) -> bytes:
        return ("\n".join(self.ops) + "\n").encode("latin-1", "replace")
