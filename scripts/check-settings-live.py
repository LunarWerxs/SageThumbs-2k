#!/usr/bin/env python3
r"""Drive the REAL Settings window, invisibly, and check what a user would actually see.

WHY THIS EXISTS. Every other UI gate in this repo reads a `--shot` still: a window built
off-screen and rendered once with PrintWindow. That proves the paint code, not the window.
On 2026-09-18 the stills said the light-mode patchwork fix was complete; this script, on its
first run, found the licence-key edit sitting on the Licence page with no frame at all - a
`Row::WideBtn` that `paint_chrome` had never framed - because it looks at EVERY field on EVERY
page of the real window rather than the two pages someone thought to shoot. It is also the only
check that can see a stale pixel: it captures the DWM surface (`PW_RENDERFULLCONTENT`) of a
window that has been through the app's normal launch path and real message loop, so a repaint
that was never asked for shows up as the old colour still on screen.

WHAT IT DOES. Launches the exe with `--tab nav_quickpreview` and SW_HIDE, makes the window layered at alpha 0
(never visible, never activated, no focus stolen), shows it, then per theme (ST2K_THEME=light
and dark, against a scratch ST2K_SETTINGS_ROOT so nothing touches the real settings):

  1. every page (posted through the nav item's own WM_COMMAND): no two big neutral background
     tones within 8 levels of each other - the "scattered color blocks" report;
  2. every page: for every visible Edit / ComboBox that is a framed field, the control's own
     interior fill equals the ring the dialog paints around it (frame and field are one colour,
     enabled or disabled);
  3. the Quick preview page: the master switch is clicked ON, then OFF, through BM_CLICK; the
     dependent field's enabled state, interior, frame ring and caption ink must all follow, and
     the second OFF must land pixel-equal to the first (that is the parent-side ring repaint).

Exit 0 = every check passed. Exit 1 = a check failed (details printed). Exit 2 = could not
run: Pillow missing, a Settings window is already open (we never touch a live one), or the
window never appeared - INCONCLUSIVE, never folded into green.

  python scripts\check-settings-live.py --exe "<path>\SageThumbs2K.exe" [--keep <dir>]

Control ids and the page count are read from the source beside this script when it runs
inside the repo (ids.rs / navrail.rs), so a renumbered id cannot make this pass for the wrong
control; outside the repo the last known values are used.
"""
import argparse
import os
import sys
import tempfile
from pathlib import Path

from settings_live_run import run
from settings_live_win32 import fails


ap = argparse.ArgumentParser()
ap.add_argument("--exe", required=True, help="SageThumbs2K.exe to drive")
ap.add_argument("--keep", default=None, help="folder to keep the captures in (default: temp, deleted)")
args = ap.parse_args()


outdir = args.keep or tempfile.mkdtemp(prefix="st2k-settings-live-")
os.makedirs(outdir, exist_ok=True)
for theme in ("light", "dark"):
    run(args.exe, theme, outdir)
if fails:
    print(f"\n[settings-live] FAIL - {len(fails)} finding(s) on the real window (captures in {outdir}):")
    for f in fails:
        print("  -", f)
    sys.exit(1)
print(f"\n[settings-live] PASS - every page, every field and the live toggle agree in both themes"
      + (f" (captures kept in {outdir})" if args.keep else ""))
if not args.keep:
    for f in Path(outdir).glob("*.png"):
        f.unlink()
    try:
        Path(outdir).rmdir()
    except OSError:
        pass
sys.exit(0)
