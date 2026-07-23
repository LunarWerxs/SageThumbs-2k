#!/usr/bin/env python3
"""Regenerate the tiny SOLID .7z fixtures used by src/container/sevenz.rs tests.

These guard the solid-archive cover scan (src/container/sevenz.rs: `solid_covers`),
which picks covers by PHYSICAL order and bounds decode with a peek budget so
clicking a huge project .7z can't spike Explorer's CPU/disk. Both archives are
SOLID (one folder, >1 substream). Content is a raw marker per entry: extract_seek_n
returns stored bytes verbatim and never decodes, so the payload need not be a valid
image; keep the marker strings in sync with the assertions in sevenz.rs.

  solid_order.7z   Physical order [m.png, a.png]. "a.png" sorts first by NAME (the
                   old pick); the solid rule takes the physically-first image m.png.
  solid_buried.7z  The only image sits behind ~12 MiB of non-image data (past the
                   8 MiB budget), so the scan must decline WITHOUT decoding the
                   prefix -- the reach cost is predicted from the header.

No "many blocks" fixture ships for the SOLID_MAX_BLOCKS guard: py7zr packs a solid
archive into ONE block (that is what "solid" means), so it can't produce the
thousands-of-blocks header that guard defends against. The sevenz.rs tests exercise
that guard by lowering the cap on these tiny fixtures instead (see
solid_block_guard_declines_when_over_cap).

Requires: py7zr  (pip install py7zr). Run from anywhere:
  python tests/fixtures/sevenz/make_fixtures.py
The committed .7z files are the output of this script; regenerate + recommit if you
change the markers or layout.
"""
import os
import py7zr

DST = os.path.dirname(os.path.abspath(__file__))

with py7zr.SevenZipFile(os.path.join(DST, "solid_order.7z"), "w") as z:
    z.writestr(b"PHYSICALLY-FIRST-IMAGE", "m.png")
    z.writestr(b"name-sorts-first-but-second-physically", "a.png")

with py7zr.SevenZipFile(os.path.join(DST, "solid_buried.7z"), "w") as z:
    z.writestr(b"\x00" * (12 * 1024 * 1024), "0_big.bin")
    z.writestr(b"late-image-bytes", "late.png")

print("wrote solid_order.7z + solid_buried.7z to", DST)
