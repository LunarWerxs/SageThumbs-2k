"""The ONE way a support script resolves the built app EXE.

`make-collage.py` and `make-pdf-shot.py` each carried the same four-candidate ladder - an
explicit argument, `cargo metadata`'s `target_directory/release`, `./target/release`, then the
Program Files install - and the two copies had already drifted: one printed nothing when
`cargo metadata` failed, the other warned. That silence is the expensive branch. This repo pins
an ABSOLUTE custom target directory in `.cargo/config.toml`, so `cargo metadata` is the only
candidate that finds a freshly built EXE; when it fails quietly the ladder falls through to the
INSTALLED copy and the script cheerfully screenshots the previous release, which looks exactly
like a change that did nothing.

Stdlib only, and a sibling import, so every caller keeps its "regenerates from a bare clone
with no third-party dependency" guarantee (`sys.path[0]` is the running script's own directory,
which is `scripts/`).
"""
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INSTALLED = Path(r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe")


def find_exe(argv, label="st2k"):
    """The app EXE: an explicit `argv[1]`, then the configured target dir, then the install.

    Exits with a message when nothing resolves; `label` names the calling script in the
    `cargo metadata` warning so a log line says which asset is about to be built wrong.
    """
    candidates = []
    if len(argv) > 1:
        candidates.append(Path(argv[1]))
    try:
        meta = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=True,
            ).stdout
        )
        candidates.append(Path(meta["target_directory"]) / "release" / "SageThumbs2K.exe")
    except Exception as e:
        # Best effort, but never silent: see the module docstring for what a quiet fall-through
        # to the installed EXE costs.
        print(f"[{label}] cargo metadata unavailable ({e}); trying the default paths")
    candidates.append(ROOT / "target" / "release" / "SageThumbs2K.exe")
    candidates.append(INSTALLED)
    for c in candidates:
        if c.is_file():
            return c
    sys.exit("SageThumbs2K.exe not found - build it first, or pass its path as argument 1")
