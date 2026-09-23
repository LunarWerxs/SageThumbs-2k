"""Check a proposed crate layering of SageThumbs' library against the code: every reference that
points from a lower layer UP to a higher one (or names an item that lives in lib.rs itself) is a
cut the split has to make first. Prints each violating edge with its file:line sites.

    python scripts/refactor/crate_layers.py . [--sites N] [--app]

`--app` checks the app binary (src/bin/app) against APP_LAYERS instead of the library.

LAYERS below is the split plan, bottom layer first; edit it when the plan changes. Zero
violations means every layer can move into its own crate as it stands.
"""
import re
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(sys.argv[1])
SITES = int(sys.argv[sys.argv.index("--sites") + 1]) if "--sites" in sys.argv else 4
SRC = ROOT / "src"

LAYERS = [
    ("base", "host safety settings formats i18n fsutil dib licence_state unixtime parallel guids hex clipboard "
             "sqlite_prim checkerpx failmemo shellcmd upload_config upload_history testcorpus"),
    ("codecs", "container decode video vstream streamsrc isobmff mp4 mkv flv mpeg12 vcodec vp9 pdf ocr jpegtran "
               "app_image strip fuzz"),
    ("actions", "verbs topdf"),
    ("shell", "thumbprovider previewhandler propstore contextmenu command factory badge register typeoverlay foldermenu "
              "cli doctor mcp prebuild"),
]
# The app binary (src/bin/app, root file main.rs), split the same way: the shared window kit,
# the viewer, the screenshot tool, and the binary itself (everything not named here).
APP_LAYERS = [
    ("appkit", "win dark gdip uia gif_frames http sponsors explorer_selection dialog_hook license "
               "cred_store licence_cert update"),
    ("preview", "preview"),
    ("screenshot", "screenshot ocr_result upload_result upload_history_dlg eyedropper hotkey"),
]
if "--app" in sys.argv:
    SRC = ROOT / "src" / "bin" / "app"
    _main = (SRC / "main.rs").read_text(encoding="utf-8", errors="replace")
    _named = {m for _, mods in APP_LAYERS for m in mods.split()}
    _all = set(re.findall(r"^\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", _main, re.M))
    LAYERS = APP_LAYERS + [("app", " ".join(sorted(_all - _named)))]
LEVEL = {m: i for i, (_, mods) in enumerate(LAYERS) for m in mods.split()}
NAME = [name for name, _ in LAYERS]
# A layer already lifted into crates/<layer> is its own crate: cargo refuses an upward edge
# from it outright (a dependency cycle), so only the modules still in src/ need checking.
LIFTED = {m for name, mods in LAYERS if (ROOT / "crates" / name / "src" / "lib.rs").exists() for m in mods.split()}

lib = (SRC / ("main.rs" if "--app" in sys.argv else "lib.rs")).read_text(encoding="utf-8", errors="replace")
declared = set(re.findall(r"^\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", lib, re.M))
missing = sorted(declared - set(LEVEL))
extra = sorted(set(LEVEL) - declared - LIFTED)


def files_of(m):
    out = [p for p in (SRC / f"{m}.rs", SRC / m / "mod.rs") if p.exists()]
    if (SRC / m).is_dir():
        out += [p for p in (SRC / m).rglob("*.rs") if p not in out]
    return out


def inline_mod_spans(code):
    """(start, end) of every inline `mod name { ... }` body: a `super::` inside one names the
    file's own module, not lib.rs."""
    spans = []
    for hit in re.finditer(r"\bmod\s+\w+\s*\{", code):
        depth, i = 1, hit.end()
        while depth and i < len(code):
            depth += {"{": 1, "}": -1}.get(code[i], 0)
            i += 1
        spans.append((hit.end(), i))
    return spans


# A `#[macro_export]` macro is named `crate::name!` wherever it is defined; its layer is the
# module that defines it.
MACRO_HOME = {}
for m in LEVEL:
    for f in files_of(m):
        for name in re.findall(r"#\[macro_export\]\s*macro_rules!\s*(\w+)",
                               f.read_text(encoding="utf-8", errors="replace")):
            MACRO_HOME[name] = m

PATH = re.compile(r"\b(crate|super)::(\{[^;]*?\}|\w+)(!?)", re.S)
violations = defaultdict(list)
for m, level in LEVEL.items():
    if m in LIFTED:
        continue
    for f in files_of(m):
        text = f.read_text(encoding="utf-8", errors="replace")
        code = re.sub(r"//[^\n]*", lambda x: " " * len(x.group(0)), text)
        nested = inline_mod_spans(code)
        for hit in PATH.finditer(code):
            kind, what = hit.group(1), hit.group(2)
            if kind == "super" and (f.parent != SRC or any(a <= hit.start() < b for a, b in nested)):
                continue
            if what in MACRO_HOME and what not in LEVEL:
                what = MACRO_HOME[what]
            heads = re.findall(r"(?:^|[{,])\s*(\w+)", what) if what.startswith("{") else [what]
            line = code.count("\n", 0, hit.start()) + 1
            for head in heads:
                if head == m or head in ("self", "super"):
                    continue
                if head in LEVEL:
                    if LEVEL[head] > level:
                        violations[(m, head)].append(f"{f.relative_to(ROOT).as_posix()}:{line}")
                elif head not in declared and level < len(LAYERS) - 1:
                    # The top layer stays in the crate that owns lib.rs, so naming lib.rs is fine there.
                    violations[(m, "lib.rs:" + head)].append(f"{f.relative_to(ROOT).as_posix()}:{line}")

if missing or extra:
    print(f"layer table out of date: in lib.rs but unplaced {missing}; placed but not in lib.rs {extra}")
by_layer = defaultdict(int)
for (src, dst), sites in sorted(violations.items(), key=lambda kv: (LEVEL[kv[0][0]], -len(kv[1]))):
    by_layer[NAME[LEVEL[src]]] += len(sites)
    target = dst if dst.startswith("lib.rs:") else f"{dst} ({NAME[LEVEL[dst]]})"
    print(f"{NAME[LEVEL[src]]:8} {src:15} -> {target:34} {len(sites):3}  " + ", ".join(sites[:SITES]))
print("\nupward references to cut, by the layer they start in: " + (", ".join(f"{k} {v}" for k, v in by_layer.items()) or "none"))
