"""The Settings window's class, messages and control ids check-settings-live.py drives."""

import re
from pathlib import Path


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
