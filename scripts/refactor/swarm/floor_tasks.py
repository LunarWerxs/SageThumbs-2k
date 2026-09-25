"""Build the zswarm task file for the Architect FLOOR: the three warning families that are a
judgement call rather than a mechanical split (maintainability-index, oversized-files,
code-duplication-core).

Usage: python floor_tasks.py [--report DIR] [--out PATH] [--family mi,oversized,dup] [--pilot N]

Reads the three per-check markdown reports the Architect writes under
`.arkitect/reports/<run>/` (newest run by default) and emits ONE task per FILE (mi, oversized)
or per CLONE GROUP (dup). Every task is a TRIAGE task: the worker reads the code and answers
`restructure` or `reject`, with the reason a human would write in the ROADMAP appendix. It
never edits: the restructuring pass is a second swarm over the `restructure` set only, so a
cheap worker's judgement is reviewed before any file moves.

WHY A TRIAGE PASS AT ALL. These three families are the ones the warnings campaign deliberately
left standing (docs/todo/TODO.md part 2, 2026-09-20): a data table that reads as a table is not
duplication, a one-flow script is not two jobs, and a format catalogue is not an oversized file.
The metric cannot tell those from the real thing, so each item needs a reason on the record -
restructured, or written into ROADMAP's "Considered and rejected" appendix - and nothing is
left merely unexamined.

LEAVE-ALONE, enforced here so no worker can spend a turn on it:
  - `uia.rs` / `screenshot/overlay/uia.rs` - docs/ISSUES.md issue 8; the shared blocks are UI
    Automation providers and no gate in this repo can prove a change to them. Needs a session
    with a screen reader actually running.
  - `src/bin/app/nudge_engine.rs` - a verbatim port, never edited (CLAUDE.md).
"""
import argparse
import glob
import json
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))

LEAVE_ALONE = ("uia.rs", "nudge_engine.rs")

SCHEMA = {
    "type": "object",
    "properties": {
        "decision": {"type": "string", "enum": ["restructure", "reject"]},
        "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
        "what_it_is": {"type": "string"},
        "reason": {"type": "string"},
        "plan": {"type": "string"},
        "risk": {"type": "string"},
    },
    "required": ["decision", "confidence", "what_it_is", "reason", "plan", "risk"],
}

COMMON = """You are triaging ONE Architect warning in a Rust/Windows shell-extension repo
(SageThumbs 2K). You do NOT edit anything. You read the code and answer one question:

  Would a competent engineer rebuilding this repo TODAY, from nothing, write this artifact in
  this exact shape?

If yes -> `decision: "reject"`, and your `reason` is the sentence that goes into the ROADMAP's
"Considered and rejected" appendix, so the next person does not re-derive it. If no ->
`decision: "restructure"`, and your `plan` is the concrete split or extraction, naming the new
file or function names and what moves where.

WHAT COUNTS AS A REJECT (these are real, not excuses):
  - A literal DATA TABLE that reads as a table (a format catalogue, a locale map, a list of
    magic bytes). Collapsing it into a loop over a const array makes it HARDER to read and to
    diff, not easier.
  - A single flow that is genuinely one job, just a long one (a build script that builds one
    thing in fifteen ordered steps).
  - A file whose single-file shape is the POINT (a vendored, dependency-free script that has to
    be droppable into a bare CI runner as one file).
  - Two sites that look alike to a token scanner but mean different things (an import preamble,
    a `use` block, a window-class registration whose fields differ).
  - An extraction that would need more than ~5 parameters or a struct invented only to carry
    them: that is a worse shape than the duplication.

WHAT COUNTS AS A RESTRUCTURE:
  - A verbatim call sequence repeated in three or more files (COM init/teardown, an HRESULT
    check ladder, an identical error-mapping block).
  - A file that is plainly two jobs bolted together (a parser AND a renderer; a downloader AND
    a report writer), where the seam is obvious and each half has its own callers.
  - A helper that already exists elsewhere in the repo and this site simply does not call.

RULES OF THIS REPO you must respect in any plan:
  - `src/bin/app/nudge_engine.rs` is a verbatim port: never touched.
  - `crates/appkit/src/uia.rs` and `crates/screenshot/src/screenshot/overlay/uia.rs` are ISSUES.md issue 8:
    never touched without a screen reader running.
  - Scripts and tests are read BY NAME by other scripts. Before you propose moving anything out
    of a file, grep `scripts/`, `tests/`, `.github/workflows/` and `docs/` for that file's name
    and say in `risk` what you found.
  - A helper extracted from a Rust function must itself land under the complexity gate (30).

Answer with the schema. `what_it_is` is one sentence saying what the code actually is (the thing
the metric could not see). Keep `reason` under 60 words and write it as prose a human will read
in a changelog-like appendix, not as notes to yourself. Budget: at most 8 tool calls.
"""

MI_PROMPT = COMMON + """
--- THIS ITEM ---
Family: maintainability-index. File: `{path}` scored MI **{score}** (0..100; under 20 is
"moderate", under 10 "difficult"). The metric is Halstead volume + cyclomatic + line count, so
ANY single-file tool past roughly 150 lines scores near zero whatever its quality - the score
alone proves nothing. Read the file and judge whether it is genuinely two or more jobs that
should be a small package (a thin entry point plus modules), or one flow that happens to be
long.

Read `{path}` first. Then decide.
"""

OVERSIZED_PROMPT = COMMON + """
--- THIS ITEM ---
Family: oversized-files. File: `{path}`, **{lines} lines** (the check warns at 800). Length
alone is not a defect: a format catalogue or a table of cases is supposed to be long. Read the
file and judge whether it is two or more jobs that should be split, or one coherent thing.

Read `{path}` first. Then decide.
"""

DUP_PROMPT = COMMON + """
--- THIS ITEM ---
Family: code-duplication-core. A token-based clone detector found the same 50+ tokens at these
sites:

{sites}

Read EVERY site listed above before answering. Judge whether this is real duplication worth a
shared helper, or two pieces of code that merely tokenise alike (an import preamble, a `use`
list, a struct literal whose field VALUES differ, a match arm table). If you propose a helper,
say exactly which file it goes in and what its signature is.
"""


def newest_report_dir():
    dirs = sorted(d for d in glob.glob(os.path.join(ROOT, ".arkitect", "reports", "*")) if os.path.isdir(d))
    return dirs[-1] if dirs else None


def read_check(report_dir, stem):
    hits = sorted(glob.glob(os.path.join(report_dir, "check-" + stem + "-*.md")))
    if not hits:
        return ""
    with open(hits[0], encoding="utf-8", errors="replace") as fh:
        return fh.read()


def leave_alone(path):
    return any(p in path for p in LEAVE_ALONE)


def parse_rows(text, pattern, field):
    """One {"path", field} row per `pattern` match (group 1 the path, group 2 the value),
    minus the leave-alone files."""
    return [{"path": m.group(1), field: m.group(2)}
            for m in re.finditer(pattern, text, re.M) if not leave_alone(m.group(1))]


def parse_mi(text):
    """`- \\`scripts/x.py\\` - MI **0.0**` under the PRODUCTION heading."""
    return parse_rows(text, r"^- `([^`]+)` - MI \*\*([0-9.]+)\*\*", "score")


def parse_oversized(text):
    """`- WARNING path - 1244 lines (>= 800)`."""
    return parse_rows(text, r"^- WARNING (\S+) - (\d+) lines", "lines")


def parse_dup(text):
    """The `### Actionable (N)` inventory: `- 50+ tokens; K sites[; N merged windows]: a, b`."""
    body = text.split("### Actionable")[-1].split("### Dashboard-only")[0]
    out = []
    for m in re.finditer(r"^- 50\+ tokens;[^:]*: (.+)$", body, re.M):
        sites = [s.strip() for s in m.group(1).split(",")]
        if any(leave_alone(s) for s in sites):
            continue
        out.append(sites)
    return out


def build(report_dir, families):
    tasks, skipped = [], []
    if "mi" in families:
        for row in parse_mi(read_check(report_dir, "maintainability-index")):
            tid = "mi_" + re.sub(r"[^a-z0-9]+", "_", row["path"].lower())
            tasks.append({"id": tid, "prompt": MI_PROMPT.format(**row)})
    if "oversized" in families:
        for row in parse_oversized(read_check(report_dir, "oversized-files")):
            tid = "big_" + re.sub(r"[^a-z0-9]+", "_", row["path"].lower())
            tasks.append({"id": tid, "prompt": OVERSIZED_PROMPT.format(**row)})
    if "dup" in families:
        for i, sites in enumerate(parse_dup(read_check(report_dir, "code-duplication-core"))):
            body = "\n".join("  - `" + s + "`" for s in sites)
            tasks.append({"id": "dup_%02d" % i, "prompt": DUP_PROMPT.format(sites=body)})
    return tasks, skipped


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--report", default="")
    ap.add_argument("--out", default=os.path.join(ROOT, "tmp/floor-tasks.json"))
    ap.add_argument("--family", default="mi,oversized,dup")
    ap.add_argument("--pilot", type=int, default=0)
    a = ap.parse_args()

    report_dir = a.report or newest_report_dir()
    if not report_dir or not os.path.isdir(report_dir):
        print("floor_tasks: no Architect report directory found - run arkitect first", file=sys.stderr)
        return 2

    tasks, _ = build(report_dir, set(a.family.split(",")))
    if a.pilot:
        tasks = tasks[: a.pilot]
    job = {
        "defaults": {
            "cwd": ROOT,
            "tools": "read",
            "schema": SCHEMA,
            "max_turns": 16,
            "timeout_s": 900,
            "model": "deepseek-flash-or",
        },
        "tasks": tasks,
    }
    with open(a.out, "w", encoding="utf-8") as fh:
        json.dump(job, fh, indent=1)
    print("report", os.path.basename(report_dir))
    print("tasks", len(tasks), "->", a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
