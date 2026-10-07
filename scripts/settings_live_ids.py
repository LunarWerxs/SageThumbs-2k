"""The Settings window's class, messages and control ids check-settings-live.py drives."""

import re
from pathlib import Path


CLASS = "SageThumbs2KOptions"
ROOT = r"Software\SageThumbs2K-LiveCheckScratch"
WM_COMMAND, WM_CLOSE, BM_CLICK = 0x111, 0x10, 0xF5

# ---- ids from the source, when we are inside the repo ------------------------------------
IDS = {"ID_NAV_BASE": 1700, "ID_PREVIEW_ENABLED": 1203, "ID_PREVIEW_BLOCKED_EXTS": 1260,
       "ID_LBL_PREVIEW_BLOCKED_EXTS": 1259, "NCAT": 12}
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

# ---- page indices by nav key, from navrail.rs's `nav_key` table ---------------------------
# Never a number literal: inserting a page shifts every index after it, and the live toggle
# once went on sampling the page next to Quick preview (2026-10-07, Screenshot files at 6).
PAGES = {"nav_filetypes": 2, "nav_quickpreview": 9}
nav = src / "navrail.rs"
if nav.exists():
    nav_text = nav.read_text(encoding="utf-8", errors="replace")
    for key in PAGES:
        m = re.search(rf'(\d+) => "{key}"', nav_text)
        if m:
            PAGES[key] = int(m.group(1))
# The File types page holds a zebra-striped list: two tones by design, so rule 1 skips it.
ZEBRA_PAGE = PAGES["nav_filetypes"]
PREVIEW_PAGE = PAGES["nav_quickpreview"]
