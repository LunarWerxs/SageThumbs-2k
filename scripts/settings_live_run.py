"""One theme's pass over every page and the live toggle of the real Settings window (see
check-settings-live.py)."""

import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import time
import winreg

from settings_live_ids import (BM_CLICK, CLASS, ID_CAPTION, ID_FIELD, ID_SWITCH, NAV_BASE, NCAT, ROOT,
                               WM_CLOSE, WM_COMMAND, ZEBRA_PAGE)
from settings_live_win32 import capture, fails, field_checks, mode, pixels, rival_tones, to_window, u32


def run(exe, theme, outdir):
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
    p = subprocess.Popen([exe, "--tab", "8"], env=env, startupinfo=si)
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
