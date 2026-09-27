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

Needs `fonttools`: `pip install -r scripts/requirements-dev.txt`. The generated font is
COMMITTED, so a normal build and CI never need this script or the network.
"""

from __future__ import annotations

import argparse
import os
import sys
import urllib.request
from pathlib import Path

from icon_font_outlines import build_instance, normalize_optical_sizes, rename_face
from icon_font_spec import (FACE_NAME, GLYPHS, NORM_CENTER, NORM_MAX_SCALE, NORM_MIN_SCALE, NORM_TARGET,
                            PIN_FILLED_OUT, PIN_MATERIAL_NAME)
from icon_font_upstream import UPSTREAM, UPSTREAM_SHA256, load_codepoints, verify_sha256

REPO = Path(__file__).resolve().parent.parent
OUT_TTF = REPO / "assets" / "icons" / "SageThumbs2K-Icons.ttf"
OUT_LICENSE = REPO / "assets" / "icons" / "LICENSE-Material-Symbols.txt"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", help="local copy of the upstream variable TTF")
    args = ap.parse_args()

    try:
        from fontTools.merge import Merger
        from fontTools.ttLib import TTFont
    except ImportError:
        print("needs fonttools:  pip install -r scripts/requirements-dev.txt", file=sys.stderr)
        return 2

    src = Path(args.src) if args.src else REPO / ".icon-font-src.ttf"
    if not src.exists():
        print(f"downloading Material Symbols -> {src}")
        with urllib.request.urlopen(UPSTREAM) as r:  # nosec B310 - fixed https URL
            data = r.read()
        verify_sha256(data, UPSTREAM_SHA256, "variable font")
        src.write_bytes(data)

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
        # AFTER the merge, so the two pin instances are normalized together (see SCALE_GROUPS)
        # rather than each against its own part-font.
        norm = normalize_optical_sizes(merged)
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

    # The normalization is a silent geometry change to a committed binary asset, so print what
    # it did: which glyphs moved, by how much, and where the longest side landed. A row that
    # sits AT a clamp is the one to look at if the toolbar ever reads uneven again.
    print(f"\noptical normalization  target={NORM_TARGET}  "
          f"clamp=[{NORM_MIN_SCALE}, {NORM_MAX_SCALE}]  centre={NORM_CENTER}")
    for cp, name, s, longest in norm:
        flag = ""
        if abs(s - NORM_MAX_SCALE) < 1e-9:
            flag = "  <- at MAX clamp (stayed smaller on purpose)"
        elif abs(s - NORM_MIN_SCALE) < 1e-9:
            flag = "  <- at MIN clamp"
        print(f"  U+{cp:04X} {name:<12} x{s:.3f} -> {longest:>4}{flag}")

    OUT_LICENSE.write_text(LICENSE_TEXT, encoding="utf-8")
    size = os.path.getsize(OUT_TTF)
    print(f"\nwrote {OUT_TTF.relative_to(REPO)}  {size:,} bytes  {len(cmap)} glyphs")
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
