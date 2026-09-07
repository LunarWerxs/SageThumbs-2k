#!/usr/bin/env python3
"""Rebuild the Quick preview PDF-viewer screenshot (assets/screenshots/preview-pdf.png).

    python scripts/make-pdf-shot.py [path\\to\\SageThumbs2K.exe]

Why this exists, and why the demo document is GENERATED rather than checked in: the shot this
replaces was taken on 2026-08-22 against a `field-guide.pdf` that lived on one developer's
machine and was never committed. By 2026-09-06 that file was gone, and the screenshot could
not be reproduced at all - by then its toolbar was three buttons behind the app (no theme
toggle, no save-page, no Settings gear) and nothing could refresh it. That is the same way the
hero collage rotted, and it has the same fix: the input is produced by this script, so the
asset is a function of the repo instead of a function of someone's Downloads folder.

Zero third-party dependencies on purpose. `make-collage.py` needs Pillow and is skipped
(loudly) when it is missing; that is tolerable for a composite, but an asset that cannot be
regenerated on a bare clone is exactly the failure this script exists to end. The PDF is
therefore emitted by the small writer below using only the base-14 fonts every PDF reader
already has, the same way make-collage.py generates its own .md/.eml/.stl demo inputs.

The capture goes through the app's own `--shot --window preview` harness: the window is built
OFF-SCREEN and rendered with PrintWindow, so nothing appears on screen, nothing steals focus,
and this is safe to run at any time.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The window is requested at this size through `--size`, which sizes the WINDOW: the client
# area comes back a little smaller (the frame's borders), which is why these are 14x7 larger
# than the 1236x1643 image the committed asset has always been. Matching its predecessor's
# size keeps the README layout unchanged.
SHOT_W, SHOT_H = 1250, 1650

# The Ctrl+F term the shot demonstrates. It must appear EXACTLY ONCE in the document, or the
# find bar reads "1/3" instead of "1/1" and the screenshot stops showing a clean single hit.
# `assert_single_hit` enforces that against the real text below rather than trusting it.
FIND_TERM = "andromeda"

# The page the viewer is scrolled to. Page 3 is where FIND_TERM lives, so the caption's page
# counter and the find bar agree - the point of the shot is that the search JUMPED here.
FIND_PAGE = 3

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


# ---- The demo document -------------------------------------------------------------------
#
# A fictional visitor guide for a fictional observatory: ordinary prose, one table per content
# page, one bullet list. The screenshot is selling the VIEWER (continuous scroll, the page
# thumbnail strip, find-and-jump), so the document has to read like a real multi-page document
# and otherwise get out of the way.

PAGES: list[list[tuple]] = [
    [
        ("kicker", "SECTION ONE"),
        ("h1", "Dark Sky Field Guide"),
        ("rule", None),
        ("body", "The ridge has been dark for as long as anyone here has been counting, and "
                 "the guide you are holding exists to keep it that way. Read it before your "
                 "first visit; most of it is about light, and the rest is about gates."),
        ("body", "The observatory is run by volunteers. Nobody is paid to be here at two in "
                 "the morning, so the rules below are the ones that keep the site usable "
                 "rather than the ones a lawyer would write."),
        ("h2", "Before you arrive"),
        ("bullets", [
            "Check the cloud forecast the afternoon before, not the week before.",
            "Charge everything at home. There is no mains power at the pads.",
            "Bring more layers than you think. The ridge runs cold after midnight.",
            "Tell someone which pad you are on if you are observing alone.",
        ]),
        ("body", "New visitors are welcome on any open night. If it is your first time, park "
                 "in the lower field and walk up; the upper track is reserved for people "
                 "unloading heavy mounts."),
    ],
    [
        ("kicker", "SECTION TWO"),
        ("h1", "Getting here and setting up"),
        ("rule", None),
        ("body", "The site sits at the end of a single track road. The last two miles are "
                 "unlit by design, so approach on sidelights and expect to meet people "
                 "walking in the dark."),
        ("h2", "Pad assignments"),
        ("table", {
            "head": ["Pad", "Surface", "Best for"],
            "rows": [
                ["North one", "Concrete", "Heavy equatorial mounts and permanent piers"],
                ["North two", "Concrete", "Long exposure imaging, shielded from the road"],
                ["East", "Gravel", "Visual observing and quick setups"],
                ["Meadow", "Grass", "Wide field cameras and anyone with a tripod"],
            ],
        }),
        ("body", "Pads are first come, first served, except on public nights when the two "
                 "north pads are held for the outreach scopes until nine."),
    ],
    [
        ("kicker", "SECTION THREE"),
        ("h1", "Autumn highlights"),
        ("rule", None),
        ("body", "Autumn is the season the ridge is known for. The Milky Way still arches "
                 "overhead at dusk, the fog blanks the valley lights, and the first of the "
                 "winter clusters rise before midnight."),
        ("table", {
            "head": ["Object", "Type", "How to find it"],
            "rows": [
                # The single occurrence of FIND_TERM in the whole document.
                ["Andromeda Galaxy", "Galaxy",
                 "Naked eye from the pads; follow the arrow of Cassiopeia's deeper V"],
                ["Double Cluster", "Open clusters",
                 "Binoculars; midway between Perseus and Cassiopeia"],
                ["Pleiades", "Open cluster",
                 "Rises over the east marker after 22:00 in September"],
                ["Fomalhaut", "Star", "The lone bright star skimming the southern rim"],
            ],
        }),
        ("h2", "House rules, the short version"),
        ("bullets", [
            "Red light past the gate, always.",
            "Walk the rim path, not across the meadow: dew soaked grass and tripod legs.",
            "Generators are welcome at the east pads only, and off by midnight.",
            "Leave the gate as you found it. The herd two fields down is real.",
        ]),
        ("body", "The guide's seasonal tables continue on the winter and spring pages, and "
                 "the last page lists the volunteer schedule. Clear skies."),
        ("footer", "Ridgeline Community Observatory  -  Visitor guide, fourth revision"),
    ],
    [
        ("kicker", "SECTION FOUR"),
        ("h1", "Winter, spring, and the volunteer rota"),
        ("rule", None),
        ("body", "Winter is the clearest and the cruelest. Transparency on the ridge in "
                 "January is the best it gets all year, and so is the wind chill, so the "
                 "warm room stays unlocked from November through February."),
        ("h2", "Who to ask"),
        ("bullets", [
            "Gate code and pad bookings: the duty volunteer, by radio on channel four.",
            "Outreach nights and school groups: the education team, in writing, please.",
            "Anything broken, dark, or missing: the site warden, immediately.",
        ]),
        ("body", "The rota is posted inside the warm room door and refreshed at the start of "
                 "each season. If you have been coming for a year and have not yet taken a "
                 "shift, this is the paragraph that is about you."),
        ("footer", "Ridgeline Community Observatory  -  Visitor guide, fourth revision"),
    ],
]


def blocks_text(blocks: list[tuple]) -> str:
    """Every visible string in one page's blocks. `rule` carries no text (payload is None),
    which is why this is a plain loop rather than a conditional expression - the first cut
    was the latter and fed None to the table branch."""
    out: list[str] = []
    for kind, payload in blocks:
        if kind in ("kicker", "h1", "h2", "body", "footer"):
            out.append(payload)
        elif kind == "bullets":
            out.extend(payload)
        elif kind == "table":
            out.extend(payload["head"])
            for row in payload["rows"]:
                out.extend(row)
    return "\n".join(out)


def document_text() -> str:
    """Every visible string in PAGES - what the viewer's find will search."""
    return "\n".join(blocks_text(blocks) for blocks in PAGES)


def assert_single_hit() -> None:
    """FIND_TERM must occur exactly once, on FIND_PAGE - see FIND_TERM's comment."""
    hits = document_text().lower().count(FIND_TERM)
    if hits != 1:
        sys.exit(f"FIND_TERM {FIND_TERM!r} occurs {hits} times in the demo document; the shot "
                 f"needs exactly one so the find bar reads 1/1")
    if FIND_TERM not in blocks_text(PAGES[FIND_PAGE - 1]).lower():
        sys.exit(f"FIND_TERM {FIND_TERM!r} is not on page {FIND_PAGE}; the caption's page "
                 f"counter and the find bar would disagree")


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


def find_exe(argv: list[str]) -> Path:
    """The app EXE: an explicit argument, then the configured target dir, then the install.
    Mirrors make-collage.py so both scripts resolve the binary the same way."""
    candidates: list[Path] = []
    if len(argv) > 1:
        candidates.append(Path(argv[1]))
    try:
        meta = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=True,
            ).stdout
        )
        candidates.append(Path(meta["target_directory"]) / "release" / "SageThumbs2K.exe")
    except Exception:
        pass
    candidates.append(ROOT / "target" / "release" / "SageThumbs2K.exe")
    candidates.append(Path(r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe"))
    for c in candidates:
        if c.is_file():
            return c
    sys.exit("SageThumbs2K.exe not found. Build it first, or pass its path as an argument.")


def main() -> None:
    assert_single_hit()
    exe = find_exe(sys.argv)
    print(f"pdf shot: exe={exe}")

    out_png = ROOT / "assets" / "screenshots" / "preview-pdf.png"
    out_png.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="st2k_pdfshot_") as tmp:
        pdf = Path(tmp) / "field-guide.pdf"
        build_pdf(pdf)
        print(f"  demo document = {pdf.name} ({pdf.stat().st_size:,} bytes, "
              f"{len(PAGES)} pages)")

        if out_png.exists():
            out_png.unlink()
        args = [
            str(exe), "--shot", str(out_png), "--window", "preview",
            "--file", str(pdf),
            "--pdf-page", str(FIND_PAGE),
            "--find", FIND_TERM,
            "--size", f"{SHOT_W}x{SHOT_H}",
            "--wait-ms", "600",
        ]
        res = subprocess.run(args, capture_output=True, text=True)
        if res.returncode != 0 or not out_png.is_file():
            sys.exit(f"capture failed (exit {res.returncode}): {res.stderr.strip()}")

    print(f"  {out_png.name}  ({out_png.stat().st_size:,} bytes)")

    # Mirror ONLY if the site already carries this asset. make-shots.ps1 copies its outputs
    # into site\img unconditionally, but every one of those is referenced by site/index.html;
    # this one is README-only, so an unconditional copy would leave an orphan nothing links.
    mirror = ROOT / "site" / "img" / out_png.name
    if mirror.is_file():
        mirror.write_bytes(out_png.read_bytes())
        print(f"  -> mirrored to site/img/{out_png.name}")
    else:
        print("  (site/img has no copy of this asset - README only, nothing to mirror)")


if __name__ == "__main__":
    main()
