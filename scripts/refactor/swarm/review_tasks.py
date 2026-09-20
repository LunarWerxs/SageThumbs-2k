"""Build the read-only review-swarm task file over a set of commits (CLAUDE.md 2.3 recipe).

Usage: python review_tasks.py <commit>... [--out PATH] [--per-file-over N]
One task per commit, or one per FILE when the commit touches more than N files (default 6);
the `git show` diff inline; verdict/findings schema; tools read.
"""
import argparse
import json
import os
import subprocess

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))

SCHEMA = {
    "type": "object",
    "properties": {
        "verdict": {"type": "string", "enum": ["ok", "concern"]},
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "line": {"type": "integer"},
                    "severity": {"type": "string", "enum": ["bug", "doc", "nit"]},
                    "issue": {"type": "string"},
                    "evidence": {"type": "string"},
                },
                "required": ["file", "line", "severity", "issue", "evidence"],
            },
        },
        "summary": {"type": "string"},
    },
    "required": ["verdict", "findings", "summary"],
}

HEAD_REFACTOR = """You are reviewing ONE refactoring diff in a Rust Windows shell extension (SageThumbs 2K). The refactor's contract: behaviour must be IDENTICAL before and after (a helper extracted, a duplicate collapsed, a function split). Read the diff below; open the touched files with read_file ONLY if the diff alone cannot answer a question (budget: at most 4 file reads).
Report ONLY real problems: a behaviour change (different result, different order of side effects, a guard dropped or moved, an early return lost, a bounds check weakened), a visibility or naming mistake that would not compile, a doc comment that now lies about the code it sits on, or a new helper that is wrong for one of its callers. Check these specifically, because a split in this same batch got one of them wrong and a test caught it: (1) a helper that returns a bool or an Option to stand in for a `break`, `continue` or `return` in the original loop - does the CALLER read it with the same sense the original had (break on the condition that used to break, continue on the one that used to continue)? Trace one iteration where the original broke and one where it continued. (2) A `?` that moved into a helper now exits only the helper: does the caller still abort the way the original did? (3) A `break` that became `return false` inside a helper that is now called from inside a loop: the loop must still stop. Do NOT report style, naming taste, or "consider" suggestions. Every finding must cite file and line and say concretely what input produces what different result. If you find nothing, say verdict ok with an empty findings list."""

HEAD_TESTS = """You are reviewing ONE diff that ADDS UNIT TESTS to a Rust Windows shell extension (SageThumbs 2K), sometimes with a small pure helper extracted so the tests can reach the logic. Read the diff below; open the touched files with read_file ONLY if the diff alone cannot answer a question (budget: at most 4 file reads).
Report ONLY real problems: an extraction that changed behaviour (different result, different order of side effects, a guard dropped, an early return lost); a test that does not test what its name says, asserts a tautology, or would pass against a broken implementation; a test that touches the real registry, the clipboard, a window, or a file outside the temp dir; a test that depends on the machine (locale, screen size, an installed program); a doc comment that now lies. Do NOT report style, naming taste, or "consider" suggestions. Every finding must cite file and line. If you find nothing, say verdict ok with an empty findings list."""


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout


TEST_PREFIXES = ("churn hotspot:", "build.rs:", "gen-site.mjs:", "compare-renders.py:", "test-script-tests.ps1:")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("commits", nargs="*")
    ap.add_argument("--range", default="", help="git range, e.g. 9f194ca..HEAD (kind picked per commit by subject)")
    ap.add_argument("--out", default=os.path.join(ROOT, "tmp/review-tasks.json"))
    ap.add_argument("--per-file-over", type=int, default=6)
    ap.add_argument("--kind", choices=["refactor", "tests", "auto"], default="auto")
    a = ap.parse_args()
    commits = list(a.commits)
    if a.range:
        commits += [s for s in git("log", "--format=%H", "--reverse", a.range).split("\n") if s.strip()]

    tasks = []
    for sha in commits:
        subject = git("log", "-1", "--format=%s", sha).strip()
        kind = a.kind if a.kind != "auto" else ("tests" if subject.startswith(TEST_PREFIXES) else "refactor")
        if subject.startswith("complexity: re-seed"):
            continue
        head = HEAD_TESTS if kind == "tests" else HEAD_REFACTOR
        short = git("rev-parse", "--short", sha).strip()
        files = [f for f in git("show", "--name-only", "--format=", sha).split("\n") if f.strip()]
        if len(files) > a.per_file_over:
            for f in files:
                diff = git("show", "--format=", sha, "--", f)
                tasks.append({"id": f"{short}-{f.replace('/', '_')}", "prompt": f"{head}\n\nCommit: {subject}\nFile: {f}\n\n```diff\n{diff}\n```"})
        else:
            diff = git("show", "--format=", sha)
            tasks.append({"id": f"{short}", "prompt": f"{head}\n\nCommit: {subject}\nFiles: {', '.join(files)}\n\n```diff\n{diff}\n```"})

    job = {"defaults": {"cwd": ROOT, "tools": "read", "schema": SCHEMA, "max_turns": 12, "timeout_s": 300}, "tasks": tasks}
    with open(a.out, "w", encoding="utf-8") as fh:
        json.dump(job, fh, indent=1)
    total = sum(len(t["prompt"]) for t in tasks)
    print("tasks", len(tasks), "chars", total, "->", a.out)


if __name__ == "__main__":
    main()
