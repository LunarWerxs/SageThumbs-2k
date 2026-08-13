#!/usr/bin/env python3
"""Build the bundled toolbar icon font from Material Symbols (Apache-2.0).

WHY THIS EXISTS
---------------
Every toolbar in this app used to draw its glyphs from `Segoe Fluent Icons`, which ships with
Windows 11 and does NOT exist on Windows 10 - and GDI substitutes a missing face SILENTLY, so
Windows 10 users saw rows of empty boxes (issue #21, shipped in 1.11.0, patched in 1.11.1 by
falling back to `Segoe MDL2 Assets`). That patch removed the breakage but not the dependency:
the icons still look like whatever the OS happens to provide, and differ between versions.

Bundling our own font removes the OS from the question entirely. Microsoft's icon fonts CANNOT
be redistributed, so this uses Material Symbols, which is Apache-2.0 and explicitly
redistributable.

SIZE IS THE CONSTRAINT, and it is why this subsets rather than ships a font
------------------------------------------------------------------------
`scripts/packaging/size-budget.json` allows 128 KiB of INSTALLER growth per release, and recent
releases have had ~32 KB of headroom. The upstream variable font is ~10 MB. Subsetting to the
~30 glyphs this app actually draws produces **under 5 KB**, which fits with room to spare.

Re-run whenever a toolbar gains a button:

    python scripts/build-icon-font.py            # downloads upstream, writes the asset
    python scripts/build-icon-font.py --src X.ttf   # or point it at a local copy

Needs `fonttools` (pip install fonttools). The generated font is COMMITTED, so a normal build
and CI never need this script or the network.
"""

from __future__ import annotations

import argparse
import io
import os
import sys
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT_TTF = REPO / "assets" / "icons" / "SageThumbs2K-Icons.ttf"
OUT_LICENSE = REPO / "assets" / "icons" / "LICENSE-Material-Symbols.txt"

UPSTREAM_CODEPOINTS = (
    "https://raw.githubusercontent.com/google/material-design-icons/master/variablefont/"
    "MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.codepoints"
)
UPSTREAM = (
    "https://github.com/google/material-design-icons/raw/master/variablefont/"
    "MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.ttf"
)

# The face is RENAMED rather than left as "Material Symbols Outlined" on purpose: the font is
# loaded privately (AddFontResourceEx + FR_PRIVATE) and a distinct name means a user who has
# their own Material Symbols installed cannot have theirs picked instead of ours, and ours
# cannot leak into other applications' font lists.
FACE_NAME = "SageThumbs2K Icons"

# Instance the variable font at one fixed point. `wght=400` matches the weight the old Segoe
# glyphs drew at, and `opsz=24` is the size these toolbars actually render near.
INSTANCE = {"FILL": 0, "GRAD": 0, "opsz": 24, "wght": 400}

# Every glyph the three toolbars draw, as (upstream Material name, OUR codepoint).
#
# ★ THE CODEPOINTS ARE THE APP'S EXISTING SEGOE ONES, NOT MATERIAL'S. That is the whole trick:
# by placing each Material glyph at the codepoint the Rust side already asks for, NOTHING in
# `preview::paint::btn_glyph`, `preview::transport` or `screenshot::toolbar::button_glyph`
# changes, and the OS-font fallback chain keeps working against the same single table. Remap
# here, never in three Rust files.
#
# Upstream Material codepoints are resolved by NAME at build time from the `.codepoints` file
# that ships beside the font, so a name is the stable identifier and this table never has to
# track an upstream renumbering.
GLYPHS = [
    # caption toolbar (preview/paint.rs)
    ("format_list_bulleted", 0xE8FD),  # Toc, the Markdown outline toggle
    ("image", 0xEB9F),                 # MdImages, load web images
    ("code", 0xE943),                  # Source, view source
    ("chevron_left", 0xE76B),          # PdfPrev
    ("chevron_right", 0xE76C),         # PdfNext
    ("push_pin", 0xE718),              # Pin, unpinned (outline)
    ("content_copy", 0xE8C8),          # Copy, shared with the screenshot editor
    # 0xE8D2 is the "A": the preview's OCR button and the screenshot editor's Text tool BOTH
    # use it today, and in Segoe they are the same glyph, so one entry preserves both exactly.
    ("text_fields", 0xE8D2),
    ("info", 0xE946),                  # Info
    ("upload", 0xE898),                # Upload
    ("open_in_new", 0xE8A7),           # Open
    ("open_in_browser", 0xE7AC),       # OpenWith
    ("close", 0xE711),                 # Close, shared with the screenshot editor
    # video transport (preview/transport.rs)
    ("play_arrow", 0xE768),
    ("pause", 0xE769),
    ("skip_previous", 0xE892),
    ("skip_next", 0xE893),
    ("volume_up", 0xE767),
    ("volume_off", 0xE74F),
    ("repeat", 0xE8EE),
    ("swap_horiz", 0xE8AB),
    # screenshot editor (screenshot/toolbar.rs)
    ("edit", 0xE70F),                  # Pen
    ("ink_highlighter", 0xE7E6),       # Highlight
    ("colorize", 0xEF3C),              # Eyedropper
    ("drag_pan", 0xE7C2),              # Move
    ("undo", 0xE7A7),
    ("redo", 0xE7A6),
    ("save", 0xE74E),
    ("cloud_upload", 0xE753),          # Upload (cloud)
]

# The pinned state needs a FILLED pin, which in a variable Material font is the SAME glyph at
# `FILL=1`. A single static instance cannot hold both, so the filled pin is built from a second
# instance and grafted in - at the app's existing "pinned" codepoint.
PIN_MATERIAL_NAME = "push_pin"
PIN_FILLED_OUT = 0xE840


def load_codepoints(src: Path) -> dict[str, int]:
    """Upstream name -> codepoint, from the `.codepoints` file beside the font."""
    cp = src.with_suffix(".codepoints")
    if not cp.exists():
        with urllib.request.urlopen(UPSTREAM_CODEPOINTS) as r:  # nosec B310 - fixed https URL
            cp.write_bytes(r.read())
    out = {}
    for line in cp.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if len(parts) == 2:
            out[parts[0]] = int(parts[1], 16)
    return out


def build_instance(src: Path, fill: int, unicodes: list[int], remap: dict[int, int] | None = None):
    """One instanced, subset font in memory."""
    from fontTools.subset import Options, Subsetter
    from fontTools.ttLib import TTFont
    from fontTools.varLib import instancer

    font = TTFont(str(src))
    instancer.instantiateVariableFont(font, {**INSTANCE, "FILL": fill}, inplace=True)
    opts = Options()
    opts.layout_features = []      # no ligatures: this app addresses glyphs by codepoint
    opts.name_IDs = ["*"]          # keep names so the face can be renamed below
    opts.notdef_outline = True     # a visible .notdef beats an invisible failure
    sub = Subsetter(options=opts)
    sub.populate(unicodes=unicodes)
    sub.subset(font)
    if remap:
        for table in font["cmap"].tables:
            table.cmap = {remap.get(cp, cp): g for cp, g in table.cmap.items()}
    buf = io.BytesIO()
    font.save(buf)
    buf.seek(0)
    return buf


def rename_face(font, name: str) -> None:
    """Give the merged font its own family/full/PostScript name."""
    ps = name.replace(" ", "")
    for rec in font["name"].names:
        try:
            nid = rec.nameID
        except AttributeError:  # pragma: no cover
            continue
        if nid in (1, 3, 4, 6, 16):
            font["name"].setName(ps if nid == 6 else name, nid, rec.platformID, rec.platEncID, rec.langID)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", help="local copy of the upstream variable TTF")
    args = ap.parse_args()

    try:
        from fontTools.merge import Merger
        from fontTools.ttLib import TTFont
    except ImportError:
        print("needs fonttools:  pip install fonttools", file=sys.stderr)
        return 2

    src = Path(args.src) if args.src else REPO / ".icon-font-src.ttf"
    if not src.exists():
        print(f"downloading Material Symbols -> {src}")
        urllib.request.urlopen  # noqa: B018 - documents the call used below
        with urllib.request.urlopen(UPSTREAM) as r, open(src, "wb") as f:  # nosec B310 - fixed https URL
            f.write(r.read())

    upstream = load_codepoints(src)
    unknown = [n for n, _ in GLYPHS if n not in upstream] + (
        [PIN_MATERIAL_NAME] if PIN_MATERIAL_NAME not in upstream else []
    )
    if unknown:
        print("upstream has no glyph named: " + ", ".join(unknown), file=sys.stderr)
        return 1
    remap = {upstream[n]: ours for n, ours in GLYPHS}
    a = build_instance(src, 0, sorted(remap), remap=remap)
    b = build_instance(
        src, 1, [upstream[PIN_MATERIAL_NAME]],
        remap={upstream[PIN_MATERIAL_NAME]: PIN_FILLED_OUT},
    )

    # Merger takes paths, so stage the two parts next to the output.
    OUT_TTF.parent.mkdir(parents=True, exist_ok=True)
    pa, pb = OUT_TTF.with_suffix(".part-a.ttf"), OUT_TTF.with_suffix(".part-b.ttf")
    pa.write_bytes(a.read())
    pb.write_bytes(b.read())
    try:
        merged = Merger().merge([str(pa), str(pb)])
        rename_face(merged, FACE_NAME)
        merged.save(str(OUT_TTF))
    finally:
        pa.unlink(missing_ok=True)
        pb.unlink(missing_ok=True)

    # Verify before declaring success: a font that silently lost a glyph would show up as one
    # blank button, which is the exact class of failure this whole exercise is about.
    check = TTFont(str(OUT_TTF))
    cmap = check.getBestCmap()
    missing = [f"{n} U+{cp:04X}" for n, cp in GLYPHS if cp not in cmap]
    if PIN_FILLED_OUT not in cmap:
        missing.append(f"pin-filled U+{PIN_FILLED_OUT:04X}")
    if missing:
        print("MISSING GLYPHS: " + ", ".join(missing), file=sys.stderr)
        return 1

    OUT_LICENSE.write_text(LICENSE_TEXT, encoding="utf-8")
    size = os.path.getsize(OUT_TTF)
    print(f"wrote {OUT_TTF.relative_to(REPO)}  {size:,} bytes  {len(cmap)} glyphs")
    print(f"wrote {OUT_LICENSE.relative_to(REPO)}")
    return 0


LICENSE_TEXT = """Material Symbols
https://github.com/google/material-design-icons

Licensed under the Apache License, Version 2.0 (the "License"); you may not use
these files except in compliance with the License. You may obtain a copy of the
License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License.

SageThumbs 2K bundles a SUBSET of this font, containing only the ~30 glyphs its
toolbars draw, instanced at a single weight and renamed to "SageThumbs2K Icons"
so it cannot collide with a separately installed copy. It is generated by
scripts/build-icon-font.py; no glyph outlines were modified.
"""


if __name__ == "__main__":
    raise SystemExit(main())
