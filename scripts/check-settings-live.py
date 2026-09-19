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

WHAT IT DOES. Launches the exe with `--tab 8` and SW_HIDE, makes the window layered at alpha 0
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
import ctypes
import ctypes.wintypes as wt
import os
import re
import subprocess
import sys
import tempfile
import time
import winreg
from collections import Counter
from pathlib import Path

try:
    from PIL import Image
except ImportError:
    print("[settings-live] SKIP - Pillow not installed (pip install pillow)")
    sys.exit(2)

ap = argparse.ArgumentParser()
ap.add_argument("--exe", required=True, help="SageThumbs2K.exe to drive")
ap.add_argument("--keep", default=None, help="folder to keep the captures in (default: temp, deleted)")
args = ap.parse_args()

CLASS = "SageThumbs2KOptions"
ROOT = r"Software\SageThumbs2K-LiveCheckScratch"
WM_COMMAND, WM_CLOSE, BM_CLICK = 0x111, 0x10, 0xF5

# ---- ids from the source, when we are inside the repo ------------------------------------
IDS = {"ID_NAV_BASE": 1700, "ID_PREVIEW_ENABLED": 1203, "ID_PREVIEW_BLOCKED_EXTS": 1260,
       "ID_LBL_PREVIEW_BLOCKED_EXTS": 1259, "NCAT": 11}
src = Path(__file__).resolve().parent.parent / "src" / "bin" / "app" / "settings_dlg"
for fname in ("ids.rs", "navrail.rs"):
    f = src / fname
    if f.exists():
        text = f.read_text(encoding="utf-8", errors="replace")
        for k in IDS:
            m = re.search(rf"const {k}: (?:i32|usize) = (\d+);", text)
            if m:
                IDS[k] = int(m.group(1))
NAV_BASE, NCAT = IDS["ID_NAV_BASE"], IDS["NCAT"]
ID_SWITCH, ID_FIELD, ID_CAPTION = IDS["ID_PREVIEW_ENABLED"], IDS["ID_PREVIEW_BLOCKED_EXTS"], IDS["ID_LBL_PREVIEW_BLOCKED_EXTS"]
# The File types page holds a zebra-striped list: two tones by design, so rule 1 skips it.
ZEBRA_PAGE = 2

# ---- Win32 -------------------------------------------------------------------------------
u32, gdi = ctypes.windll.user32, ctypes.windll.gdi32
u32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
H = ctypes.c_void_p
# 64-bit handles: without argtypes ctypes squeezes them through a C int and overflows.
for fn, res, argt in (
    (u32.FindWindowW, H, [ctypes.c_wchar_p, ctypes.c_wchar_p]),
    (u32.GetDlgItem, H, [H, ctypes.c_int]),
    (u32.GetDC, H, [H]),
    (u32.ReleaseDC, ctypes.c_int, [H, H]),
    (u32.PrintWindow, wt.BOOL, [H, H, wt.UINT]),
    (u32.GetWindowRect, wt.BOOL, [H, ctypes.c_void_p]),
    (u32.GetWindowThreadProcessId, wt.DWORD, [H, ctypes.c_void_p]),
    (u32.IsWindowVisible, wt.BOOL, [H]),
    (u32.IsWindowEnabled, wt.BOOL, [H]),
    (u32.GetDlgCtrlID, ctypes.c_int, [H]),
    (u32.GetWindowLongPtrW, ctypes.c_ssize_t, [H, ctypes.c_int]),
    (u32.SetWindowLongPtrW, ctypes.c_ssize_t, [H, ctypes.c_int, ctypes.c_ssize_t]),
    (u32.SetLayeredWindowAttributes, wt.BOOL, [H, wt.DWORD, ctypes.c_ubyte, wt.DWORD]),
    (u32.ShowWindow, wt.BOOL, [H, ctypes.c_int]),
    (u32.GetClassNameW, ctypes.c_int, [H, ctypes.c_wchar_p, ctypes.c_int]),
    (u32.SendMessageW, ctypes.c_ssize_t, [H, wt.UINT, wt.WPARAM, wt.LPARAM]),
    (u32.PostMessageW, wt.BOOL, [H, wt.UINT, wt.WPARAM, wt.LPARAM]),
    (u32.EnumChildWindows, wt.BOOL, [H, ctypes.c_void_p, wt.LPARAM]),
    (gdi.CreateCompatibleDC, H, [H]),
    (gdi.CreateCompatibleBitmap, H, [H, ctypes.c_int, ctypes.c_int]),
    (gdi.SelectObject, H, [H, H]),
    (gdi.DeleteObject, wt.BOOL, [H]),
    (gdi.DeleteDC, wt.BOOL, [H]),
    (gdi.GetDIBits, ctypes.c_int, [H, H, wt.UINT, wt.UINT, ctypes.c_void_p, ctypes.c_void_p, wt.UINT]),
):
    fn.restype, fn.argtypes = res, argt


class BMIH(ctypes.Structure):
    _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG), ("biPlanes", wt.WORD),
                ("biBitCount", wt.WORD), ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD),
                ("biXPelsPerMeter", wt.LONG), ("biYPelsPerMeter", wt.LONG), ("biClrUsed", wt.DWORD),
                ("biClrImportant", wt.DWORD)]


def rect(h):
    r = wt.RECT()
    u32.GetWindowRect(h, ctypes.byref(r))
    return r


def capture(hwnd):
    """The window's DWM surface as an RGB image - what is actually on screen, stale or not."""
    r = rect(hwnd)
    w, h = r.right - r.left, r.bottom - r.top
    sdc = u32.GetDC(None)
    mdc = gdi.CreateCompatibleDC(sdc)
    bmp = gdi.CreateCompatibleBitmap(sdc, w, h)
    old = gdi.SelectObject(mdc, bmp)
    ok = u32.PrintWindow(hwnd, mdc, 2)  # PW_RENDERFULLCONTENT
    gdi.SelectObject(mdc, old)
    bi = BMIH(ctypes.sizeof(BMIH), w, -h, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi.GetDIBits(mdc, bmp, 0, h, buf, ctypes.byref(bi), 0)
    gdi.DeleteObject(bmp)
    gdi.DeleteDC(mdc)
    u32.ReleaseDC(None, sdc)
    if not ok:
        raise RuntimeError("PrintWindow failed")
    return Image.frombuffer("RGB", (w, h), buf, "raw", "BGRX", 0, 1).copy()


def pixels(im):
    return list(im.get_flattened_data() if hasattr(im, "get_flattened_data") else im.getdata())


def rival_tones(im, near=8, share=0.08):
    """Every PAIR of big neutral greys within `near` levels of each other (see check-theme-shots)."""
    px = pixels(im.resize((im.width // 2, im.height // 2), Image.NEAREST))
    n = len(px)
    c = Counter(p[0] for p in px if p[0] == p[1] == p[2])
    big = sorted(k for k, v in c.items() if v / n >= share)
    return [f"{a} ({c[a] * 100 // n}%) beside {b} ({c[b] * 100 // n}%)" for a, b in zip(big, big[1:]) if b - a <= near]


def children(hwnd):
    out = []

    @ctypes.WINFUNCTYPE(wt.BOOL, H, wt.LPARAM)
    def cb(h, _):
        buf = ctypes.create_unicode_buffer(64)
        u32.GetClassNameW(h, buf, 64)
        out.append((h, buf.value))
        return True

    u32.EnumChildWindows(hwnd, cb, 0)
    return out


def mode(im, box):
    return Counter(pixels(im.crop(box))).most_common(1)[0][0]


def to_window(hwnd, h):
    """A child's rect in window-image coordinates."""
    wr, r = rect(hwnd), rect(h)
    return r.left - wr.left, r.top - wr.top, r.right - wr.left, r.bottom - wr.top


fails = []


def field_checks(hwnd, im, label):
    """Every visible framed field: interior fill must equal the ring the dialog paints round it."""
    n = 0
    for h, cls in children(hwnd):
        if cls not in ("Edit", "ComboBox") or not u32.IsWindowVisible(h):
            continue
        x0, y0, x1, y1 = to_window(hwnd, h)
        if x1 - x0 < 30 or y1 - y0 < 10 or y1 - y0 > 60:
            continue  # multi-line edits and other odd controls are not framed fields
        mid = (y0 + y1) // 2
        # inside the control, away from its text: top-right corner of an edit, top-left of a combo
        inner = mode(im, (x1 - 8, y0 + 1, x1 - 2, y0 + 4)) if cls == "Edit" else mode(im, (x0 + 2, y0 + 1, x0 + 8, y0 + 4))
        ring = mode(im, (x0 - 3, mid - 2, x0 - 1, mid + 2))  # the dialog's pixels just left of the control
        n += 1
        if inner != ring:
            fails.append(f"{label}: {cls} id={u32.GetDlgCtrlID(h)} enabled={bool(u32.IsWindowEnabled(h))} "
                         f"interior {inner} != frame ring {ring}")
    return n


def run(theme, outdir):
    if u32.FindWindowW(CLASS, None):
        print("[settings-live] SKIP - a Settings window is already open; this check never touches a live one")
        sys.exit(2)
    try:
        winreg.DeleteKey(winreg.HKEY_CURRENT_USER, ROOT)
    except OSError:
        pass
    with winreg.CreateKey(winreg.HKEY_CURRENT_USER, ROOT) as k:
        # An EMPTY settings key is a brand-new user: the welcome window opens first, modal and
        # (under SW_HIDE) invisible, and the Settings window never comes. Mark it seen.
        winreg.SetValueEx(k, "FirstRunShown", 0, winreg.REG_DWORD, 1)
        winreg.SetValueEx(k, "NavDotsSeen", 0, winreg.REG_DWORD, 4095)
    env = dict(os.environ, ST2K_THEME=theme, ST2K_SETTINGS_ROOT=ROOT)
    si = subprocess.STARTUPINFO()
    si.dwFlags = subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = 0  # SW_HIDE: the app's own first ShowWindow is overridden by this
    p = subprocess.Popen([args.exe, "--tab", "8"], env=env, startupinfo=si)
    hwnd = None
    for _ in range(100):
        time.sleep(0.1)
        h = u32.FindWindowW(CLASS, None)
        if h:
            pid = wt.DWORD()
            u32.GetWindowThreadProcessId(h, ctypes.byref(pid))
            if pid.value == p.pid:
                hwnd = h
                break
    if not hwnd:
        p.kill()
        try:
            winreg.DeleteKey(winreg.HKEY_CURRENT_USER, ROOT)
        except OSError:
            pass
        print("[settings-live] SKIP - the Settings window never appeared")
        sys.exit(2)
    try:
        # Invisible but alive: layered alpha 0 keeps a DWM surface for PrintWindow without the
        # window ever being seen, and SW_SHOWNOACTIVATE + TOOLWINDOW steals no focus or taskbar slot.
        u32.SetWindowLongPtrW(hwnd, -20, u32.GetWindowLongPtrW(hwnd, -20) | 0x80000 | 0x80)
        u32.SetLayeredWindowAttributes(hwnd, 0, 0, 2)
        u32.ShowWindow(hwnd, 4)
        time.sleep(1.0)
        for tab in range(NCAT):
            u32.PostMessageW(hwnd, WM_COMMAND, NAV_BASE + tab, 0)
            time.sleep(0.45)
            im = capture(hwnd)
            im.save(os.path.join(outdir, f"live-{theme}-tab{tab}.png"))
            nf = field_checks(hwnd, im, f"{theme} tab{tab}")
            rv = [] if tab == ZEBRA_PAGE else rival_tones(im)
            if rv:
                fails.append(f"{theme} tab{tab}: two background tones: {'; '.join(rv)}")
            print(f"[settings-live] {theme} tab{tab}: fields={nf} tones={'ok' if not rv else rv}")

        # ---- the live toggle on the Quick preview page
        u32.PostMessageW(hwnd, WM_COMMAND, NAV_BASE + 8, 0)
        time.sleep(0.45)
        sw, fld, lbl = (u32.GetDlgItem(hwnd, i) for i in (ID_SWITCH, ID_FIELD, ID_CAPTION))
        if not (sw and fld and lbl):
            fails.append(f"{theme}: Quick preview controls not found (ids {ID_SWITCH}/{ID_FIELD}/{ID_CAPTION})")
            return

        def state(tag):
            im = capture(hwnd)
            im.save(os.path.join(outdir, f"live-{theme}-toggle-{tag}.png"))
            x0, y0, x1, y1 = to_window(hwnd, fld)
            mid = (y0 + y1) // 2
            lab = pixels(im.crop(to_window(hwnd, lbl)))
            ink = min(lab, key=sum) if theme == "light" else max(lab, key=sum)
            return dict(enabled=bool(u32.IsWindowEnabled(fld)),
                        inner=mode(im, (x1 - 8, y0 + 1, x1 - 2, y0 + 4)),
                        ring_left=mode(im, (x0 - 3, mid - 2, x0 - 1, mid + 2)),
                        ring_top=mode(im, (x0 + 20, y0 - 4, x0 + 40, y0 - 2)),
                        caption_ink=ink)

        a = state("0-off")
        u32.SendMessageW(sw, BM_CLICK, 0, 0)
        time.sleep(0.5)
        b = state("1-on")
        u32.SendMessageW(sw, BM_CLICK, 0, 0)  # back to where it was: nothing is saved either way
        time.sleep(0.5)
        c = state("2-off-again")
        for tag, s in (("off", a), ("on", b), ("off again", c)):
            print(f"[settings-live] {theme} toggle {tag}: {s}")
            if not (s["inner"] == s["ring_left"] == s["ring_top"]):
                fails.append(f"{theme} toggle {tag}: field interior and frame ring disagree: {s}")
        if a["enabled"] or not b["enabled"] or c["enabled"]:
            fails.append(f"{theme}: the field's enabled state did not follow the switch")
        if a["inner"] == b["inner"]:
            fails.append(f"{theme}: disabled and enabled field fills are identical ({a['inner']})")
        if a["caption_ink"] == b["caption_ink"]:
            fails.append(f"{theme}: the caption did not dim/undim with the switch ({a['caption_ink']})")
        if a != c:
            fails.append(f"{theme}: ON then OFF did not land back on the first OFF (stale repaint?): {a} vs {c}")
    finally:
        u32.PostMessageW(hwnd, WM_CLOSE, 0, 0)
        try:
            p.wait(4)
        except subprocess.TimeoutExpired:
            p.kill()
        try:
            winreg.DeleteKey(winreg.HKEY_CURRENT_USER, ROOT)
        except OSError:
            pass


outdir = args.keep or tempfile.mkdtemp(prefix="st2k-settings-live-")
os.makedirs(outdir, exist_ok=True)
for theme in ("light", "dark"):
    run(theme, outdir)
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
