"""Build the zswarm task file for the complexity warn band: one task per FILE.

Usage: python band_tasks.py [--pilot N] [--out PATH] [--exclude-done PATH]
Reads tmp/cx.json (scripts/complexity-scan.py --json --warnings) in the repo, drops test
code and the leave-alone list, groups by file, writes a zswarm CLI task file.
"""
import argparse
import json
import os
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))

SKIP_FILE_PARTS = ("tests/", "tests.rs", "uia", "nudge_engine.rs", "src/fuzz/", "fuzzseed")
SKIP_FUNCS = {
    ("src/licence_state.rs", "from_json"),
    ("src/decode/jp2/mq.rs", "cleanup_pass"),
    ("src/decode/magick/encode.rs", "wait_for_magick_child"),
    ("src/fuzz/surfaces.rs", "deep_session_over_the_new_parsers"),
    ("src/verbs/actions.rs", "run_action"),
    ("src/mcp.rs", "dispatch_tool"),
}

SCHEMA = {
    "type": "object",
    "properties": {
        "status": {"type": "string", "enum": ["done", "partial", "skipped"]},
        "functions": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "metric": {"type": "string"},
                    "before": {"type": "integer"},
                    "after": {"type": "integer"},
                    "action": {"type": "string"},
                },
                "required": ["name", "metric", "before", "after", "action"],
            },
        },
        "helpers_added": {"type": "array", "items": {"type": "string"}},
        "reason": {"type": "string"},
    },
    "required": ["status", "functions", "helpers_added", "reason"],
}

PROMPT = """You are refactoring ONE Rust file in SageThumbs 2K, a Windows shell extension (the cwd is the repo root; the file is UTF-8 with LF line endings). The repo's complexity scanner flags these functions in `{file}`:
{rows}

Run the scanner with: python scripts/complexity-scan.py --root . --json --warnings
It takes about 6 s and prints a JSON list of every function in the repo scoring 15..29, as objects {{score, file, line, function, metric}} with metric "cog" (cognitive) or "cyc" (cyclomatic). Filter for "file": "{file}".

GOAL: take each listed function BELOW 15 on the metric named, without changing behaviour, by extracting a helper along a NATURAL seam, and without creating any new function in this file that scores 15 or more on either metric.

What the scanner charges (measured on this repo): every `?` is a branch; a `?` inside a loop or inside a closure also pays the nesting penalty, so lift the loop or closure body into a free function at nesting zero (there each `?` costs 1 and the loop costs 2); nested `if let` / `match` under a loop is the other usual seam; two arms or two blocks that spell out the same steps become one helper called twice. A dispatcher's cyclomatic score IS its arm count (a `match` with 20 arms scores 20): leave those and say so.

RULES:
- Behaviour must be IDENTICAL: same results, same order of side effects, same error paths, same strings. Copy bodies verbatim into the helper; pass what it needs by reference (`&`, `&mut`) with explicit types; keep the helper private (`fn`, never `pub`) in the same file, directly below the function it serves, with a one-line `///` doc comment saying what it does. Early `return`s, `break`s and `continue`s inside the moved block must keep their meaning (return a bool or an Option from the helper and act on it in the caller).
- Only split along a seam a reader would NAME (a loop body, a validation block, a per-arm routine, a "build X" block). If the only way under 15 is to cut mid-thought, do NOT split: report that function as skipped with the reason.
- Prefer ONE extraction per listed function, and it must earn its name: never a helper that only wraps a single call or a single `if` (a fn that returns `Some(None)` unless a tag matches is the shape to avoid). If one lift is not enough, lift a BIGGER block (the whole loop body, the whole arm) rather than adding a second thin helper.
- Skip any function that is a `#[test]`, lives under `#[cfg(test)]`, or is a fuzz driver.
- Do not touch any other file. Do not reorder, rename or reformat anything else in this file. Never run cargo, rustc or rustfmt: the author runs clippy and the tests over the whole batch, and concurrent cargo runs collide on the target dir.
- Keep the `use` lines as they are unless the helper needs an item the file does not already import.
- VERIFY after editing: run the scanner once and check (a) none of the listed functions still appears for this file at 15 or more on its metric, and (b) no NEW function from this file appears. If a split made the band worse, undo it (restore the original text) and report skipped. Run the scanner at most twice.

Answer through submit_result with: status (done | partial | skipped), functions (one row per listed function: name, metric, before, after, action - what you did in one line), helpers_added (the new helper names), reason (one line per skipped function, or "" if none)."""


def load_source(path):
    try:
        with open(os.path.join(ROOT, path), encoding="utf-8", errors="replace") as fh:
            return fh.read().split("\n")
    except OSError:
        return []


def test_start_line(lines):
    """1-based line of the file's `#[cfg(test)] mod tests` block, or a huge number."""
    for i, line in enumerate(lines):
        if line.strip() == "#[cfg(test)]":
            j = i + 1
            while j < len(lines) and not lines[j].strip():
                j += 1
            if j < len(lines) and lines[j].lstrip().startswith("mod "):
                return i + 1
    return 10 ** 9


def is_test_fn(lines, line_no):
    lo = max(0, line_no - 8)
    above = lines[lo:line_no - 1]
    return any(l.strip().startswith("#[test]") or l.strip().startswith("#[cfg(test)]") for l in above)


def group_findings(findings, excluded):
    """Bucket the band findings by file, dropping the leave-alone files and functions and test code."""
    by_file = {}
    dropped = {"file": 0, "func": 0, "test": 0}
    cache = {}
    for f in findings:
        path = f["file"]
        if any(p in path for p in SKIP_FILE_PARTS) or path in excluded:
            dropped["file"] += 1
            continue
        if (path, f["function"]) in SKIP_FUNCS:
            dropped["func"] += 1
            continue
        if path not in cache:
            cache[path] = load_source(path)
        lines = cache[path]
        if f["line"] >= test_start_line(lines) or is_test_fn(lines, f["line"]):
            dropped["test"] += 1
            continue
        by_file.setdefault(path, []).append(f)
    return by_file, dropped


def build_tasks(files, by_file):
    """One task per file, its band functions listed in the prompt."""
    tasks = []
    for path in files:
        rows = "\n".join(
            "  - `{}` (line {}): {} {}".format(x["function"], x["line"], "cognitive" if x["metric"] == "cog" else "cyclomatic", x["score"])
            for x in sorted(by_file[path], key=lambda x: x["line"])
        )
        tid = path.replace("/", "_").replace(".rs", "")
        tasks.append({"id": tid, "prompt": PROMPT.format(file=path, rows=rows)})
    return tasks


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pilot", type=int, default=0)
    ap.add_argument("--out", default=os.path.join(ROOT, "tmp/band-tasks.json"))
    ap.add_argument("--exclude", default="", help="comma list of files to leave out (already done)")
    a = ap.parse_args()

    with open(os.path.join(ROOT, "tmp/cx.json"), encoding="utf-8") as fh:
        findings = json.load(fh)
    excluded = set(x for x in a.exclude.split(",") if x)

    by_file, dropped = group_findings(findings, excluded)
    files = sorted(by_file, key=lambda p: (-max(x["score"] for x in by_file[p]), p))
    if a.pilot:
        one = [p for p in files if len(by_file[p]) == 1 and 16 <= by_file[p][0]["score"] <= 24]
        files = one[: a.pilot]
    tasks = build_tasks(files, by_file)

    job = {
        "defaults": {"cwd": ROOT, "tools": "all", "schema": SCHEMA, "max_turns": 24, "timeout_s": 900, "model": "deepseek-flash-or"},
        "tasks": tasks,
    }
    with open(a.out, "w", encoding="utf-8") as fh:
        json.dump(job, fh, indent=1)
    n_fn = sum(len(by_file[p]) for p in files)
    print("tasks", len(tasks), "functions", n_fn, "dropped", dropped, "->", a.out)
    for p in files[:60]:
        print(" ", p, [x["score"] for x in by_file[p]])


if __name__ == "__main__":
    main()
