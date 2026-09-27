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

import subprocess
import sys
import tempfile
from pathlib import Path

from pdf_shot_document import FIND_PAGE, FIND_TERM, PAGES, assert_single_hit
from pdf_shot_file import build_pdf
from st2k_exe import find_exe

ROOT = Path(__file__).resolve().parent.parent

# The window is requested at this size through `--size`, which sizes the WINDOW: the client
# area comes back a little smaller (the frame's borders), which is why these are 14x7 larger
# than the 1236x1643 image the committed asset has always been. Matching its predecessor's
# size keeps the README layout unchanged.
SHOT_W, SHOT_H = 1250, 1650


def main() -> None:
    assert_single_hit()
    exe = find_exe(sys.argv, label="make-pdf-shot")
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
            "--wait-ms", "8500",
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
