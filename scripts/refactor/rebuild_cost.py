"""What one small edit costs to rebuild in SageThumbs, by where the edit lands.

Builds a detached worktree at the repo's HEAD in a private target dir, warms it, then for each
area appends one unused private function to that area's module file (a realistic small edit that
shifts no other item) and times the three inner-loop commands: `cargo check --lib` (the fast
type check), `cargo test --lib --no-run` (building the unit tests) and `cargo build --bins` (the
app and CLI, which link the library). Restores the file and re-warms before the next area.
One JSON line per timing on stdout. Run through fairjob.

    python scripts/refactor/rebuild_cost.py <worktree dir> <target dir>

Use a detached worktree (`git worktree add --detach <dir> HEAD`) and a target dir of its own, so
the probe edits and builds never touch a tree or a target another session is using.
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

TREE = Path(sys.argv[1])
TARGET = sys.argv[2]
ENV = dict(os.environ, CARGO_TARGET_DIR=TARGET, CARGO_TERM_COLOR="never")
COMMANDS = {
    "check-lib": ["cargo", "check", "--lib"],
    "test-lib-build": ["cargo", "test", "--lib", "--no-run"],
    "build-bins": ["cargo", "build", "--bins"],
}
AREAS = {
    "decode": ["src/decode.rs", "src/decode/mod.rs"],
    "container": ["src/container.rs", "src/container/mod.rs"],
    "verbs": ["src/verbs.rs", "src/verbs/mod.rs"],
    "settings": ["src/settings.rs", "src/settings/mod.rs"],
    "app (bin only)": ["src/bin/app/main.rs"],
}
PROBE = "\n#[allow(dead_code)]\nfn rebuild_probe_{n}() -> u32 {{ {n} }}\n"


def run(name, label):
    started = time.perf_counter()
    done = subprocess.run(COMMANDS[name], cwd=TREE, env=ENV, capture_output=True, text=True, encoding="utf-8", errors="replace")
    seconds = round(time.perf_counter() - started, 1)
    print(json.dumps({"area": label, "command": name, "seconds": seconds, "exit": done.returncode,
                      "tail": done.stderr.strip().splitlines()[-1:] if done.returncode else []}), flush=True)
    return done.returncode


for name in COMMANDS:
    if run(name, "warm (first build)"):
        sys.exit("warm build failed")
for area, candidates in AREAS.items():
    path = next((TREE / c for c in candidates if (TREE / c).exists()), None)
    if path is None:
        print(json.dumps({"area": area, "skipped": "no module file"}), flush=True)
        continue
    original = path.read_bytes()
    try:
        path.write_bytes(original + PROBE.format(n=len(area)).encode())
        wanted = ["build-bins"] if area.startswith("app") else list(COMMANDS)
        for name in wanted:
            run(name, area)
    finally:
        path.write_bytes(original)
    for name in COMMANDS:
        subprocess.run(COMMANDS[name], cwd=TREE, env=ENV, capture_output=True)
print(json.dumps({"done": True}), flush=True)
