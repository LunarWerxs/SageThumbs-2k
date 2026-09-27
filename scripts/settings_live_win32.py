"""Win32 bindings, a DWM capture of the real window, and the framed-field check (see
check-settings-live.py)."""

import ctypes
import ctypes.wintypes as wt
import sys
from collections import Counter


try:
    from PIL import Image
except ImportError:
    print("[settings-live] SKIP - Pillow not installed (pip install pillow)")
    sys.exit(2)


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
