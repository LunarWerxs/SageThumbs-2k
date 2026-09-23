"""What one small edit costs to rebuild in SageThumbs, by where the edit lands.

Builds a copy of one commit in a private target dir, warms it, then for each area appends one
unused private function to that area's module file (a realistic small edit that shifts no other
item) and times the inner-loop commands: `cargo check --lib` (the fast type check), `cargo test
--lib --no-run` (building every unit-test binary), `test-own-build` (building only the tests of
the package that owns the area: what you run while working on it) and `cargo build --bins` (the
app and CLI, which link the library). Restores the file and re-warms before the next area.
One JSON line per timing on stdout. Run through fairjob.

    python scripts/refactor/rebuild_cost.py <tree dir> <target dir>
    python scripts/refactor/rebuild_cost.py <tree A> <target A> <tree B> <target B> [--repeat N]

The second form compares two commits FAIRLY: every (area, command) is timed on A, then on B,
N times over, so a load swing on a shared box lands on both sides instead of on whichever tree
happened to run second (a single pass once read 6.6 s vs 14.1 s for an edit the change under
test could not have affected). It ends with one `summary` line per (area, command): the median
of each side.

Use a plain copy of each commit (`git archive <commit> | tar -x -C <dir>`; never a worktree, this
repo works on main only) and a target dir of its own, so the probe edits and builds never touch
a tree or a target another session is using.
"""
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
REPEAT = int(sys.argv[sys.argv.index("--repeat") + 1]) if "--repeat" in sys.argv else 1
if "--repeat" in sys.argv:
    ARGS.remove(str(REPEAT))
SIDES = [(Path(ARGS[i]), ARGS[i + 1]) for i in range(0, len(ARGS), 2)]
LABELS = ["A", "B"][: len(SIDES)] if len(SIDES) > 1 else [""]

COMMANDS = {
    "check-lib": ["cargo", "check", "--lib"],
    "test-lib-build": ["cargo", "test", "--lib", "--no-run"],
    "test-own-build": None,  # per area, see OWNER
    "build-bins": ["cargo", "build", "--bins"],
}
# Each area's module file, in every layout the tree has had: the first that exists is edited, so
# the same script measures a commit from before the crate split and one after it.
AREAS = {
    "decode": ["crates/codecs/src/decode.rs", "src/decode.rs", "src/decode/mod.rs"],
    "container": ["crates/codecs/src/container/mod.rs", "src/container/mod.rs", "src/container.rs"],
    "verbs": ["crates/actions/src/verbs.rs", "src/verbs.rs", "src/verbs/mod.rs"],
    "settings": ["crates/base/src/settings.rs", "src/settings.rs", "src/settings/mod.rs"],
    "app (bin only)": ["src/bin/app/main.rs"],
}
PROBE = "\n#[allow(dead_code)]\nfn rebuild_probe_{n}() -> u32 {{ {n} }}\n"


def owner(tree, path):
    """The package whose tests cover `path`: its crate under crates/, else the core package."""
    rel = path.relative_to(tree).as_posix()
    if rel.startswith("crates/"):
        return "sagethumbs2k-" + rel.split("/")[1]
    return "sagethumbs2k"


def command(name, tree, path):
    if name == "test-own-build":
        return ["cargo", "test", "-p", owner(tree, path), "--lib", "--no-run"]
    return COMMANDS[name]


def run(side, name, label, path=None):
    tree, target = SIDES[side]
    env = dict(os.environ, CARGO_TARGET_DIR=target, CARGO_TERM_COLOR="never")
    started = time.perf_counter()
    done = subprocess.run(command(name, tree, path), cwd=tree, env=env, capture_output=True,
                          text=True, encoding="utf-8", errors="replace")
    seconds = round(time.perf_counter() - started, 1)
    row = {"area": label, "command": name, "seconds": seconds, "exit": done.returncode,
           "tail": done.stderr.strip().splitlines()[-1:] if done.returncode else []}
    if LABELS[side]:
        row["side"] = LABELS[side]
    print(json.dumps(row), flush=True)
    return done.returncode, seconds


def warm(side, path=None):
    tree, target = SIDES[side]
    env = dict(os.environ, CARGO_TARGET_DIR=target, CARGO_TERM_COLOR="never")
    for name in COMMANDS:
        if subprocess.run(command(name, tree, path or tree / "src" / "lib.rs"), cwd=tree, env=env,
                          capture_output=True).returncode:
            sys.exit(f"re-warm failed: {name} on {tree}")


def area_file(side, candidates):
    tree = SIDES[side][0]
    return next((tree / c for c in candidates if (tree / c).exists()), None)


for side in range(len(SIDES)):
    for name in COMMANDS:
        if name != "test-own-build" and run(side, name, "warm (first build)")[0]:
            sys.exit("warm build failed")
times = {}
probe_no = 0
for area, candidates in AREAS.items():
    paths = [area_file(side, candidates) for side in range(len(SIDES))]
    if any(p is None for p in paths):
        print(json.dumps({"area": area, "skipped": "no module file"}), flush=True)
        continue
    wanted = ["build-bins"] if area.startswith("app") else list(COMMANDS)
    # Every timing applies a probe the artifacts have never seen (a new number each time), so
    # each one is "edit, then rebuild" with no re-warm in between; the area's file is restored
    # and the tree re-warmed once at the end, so the NEXT area's timings do not pay for it.
    for rep in range(REPEAT):
        for name in wanted:
            for side, path in enumerate(paths):
                original = path.read_bytes()
                try:
                    probe_no += 1  # never the same probe twice: each timing is a fresh edit
                    path.write_bytes(original + PROBE.format(n=probe_no).encode())
                    code, seconds = run(side, name, area, path)
                    if code == 0:  # a failed build keeps its printed row, never a place in the median
                        times.setdefault((area, name, side), []).append(seconds)
                finally:
                    path.write_bytes(original)
    for side, path in enumerate(paths):
        warm(side, path)
for (area, name, side), secs in sorted(times.items()):
    if side == 0 and len(SIDES) > 1:
        other = times.get((area, name, 1), [])
        print(json.dumps({"summary": area, "command": name, "A_median": statistics.median(secs),
                          "B_median": statistics.median(other) if other else None,
                          "A": secs, "B": other}), flush=True)
print(json.dumps({"done": True}), flush=True)
