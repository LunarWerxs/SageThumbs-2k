"""The refactor gate as ONE command, so the model is not the loop.

Measured 2026-09-20 over this repo's 13 biggest sessions: 30,000 model turns at ~5.6 s each
(47 h), 42 h of tool waits, and every clippy -> settle -> clippy round trip cost two turns and a
warm clippy run. This script runs the whole round trip itself:

  gate.py clippy            clippy (workspace, all targets) + the app bin WITH html-preview (the
                            release build's feature set), settling the hub's unused re-exports
                            with fix_unused_imports.py between runs, up to three rounds. Exit 0
                            when both builds are clean; the remaining errors otherwise.
  gate.py commit <plan.json>   one commit per hub: [[hub, child-or-dir, ...], ...] with the
                            standard "<hub>: <what> (N lines -> M)" message ("what" = plan[hub]
                            when the plan is {hub: [what, paths...]}).
  gate.py consistency       CI's ten consistency scripts, the way CI runs them (a non-zero
                            LASTEXITCODE fails the step even when every assertion passed).
  gate.py all <plan.json>   clippy, then consistency, then commit - the pre-push shape.

Everything heavy still belongs under fairjob on the shared box; this script is what fairjob runs.

usage: gate.py <clippy|commit|consistency|all> [plan.json]
"""
import json
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CLIPPY = [
    ["cargo", "clippy", "--workspace", "--all-targets", "--message-format", "short", "--", "-D", "warnings"],
    ["cargo", "clippy", "-p", "sagethumbs2k", "--bin", "SageThumbs2K", "--features", "html-preview", "--message-format", "short", "--", "-D", "warnings"],
]
CONSISTENCY = [
    "check-consistency", "test-release-size", "test-release-pipeline", "test-installer-lint", "test-msix-integrity",
    "test-architecture-release-contract", "test-dev-architecture", "test-magick-dependency-freshness",
    "check-vendored-exr", "check-email-rule",
]
ERR = re.compile(r"^(?:src|crates|tests)[^ ]*: (?:error|warning)", re.M)


def run(cmd, log=None):
    p = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace")
    out = p.stdout + p.stderr
    if log:
        with open(log, "a", encoding="utf-8") as fh:
            fh.write(out)
    return p.returncode, out


def clippy(rounds=3):
    log = os.path.join(ROOT, "tmp", "gate-clippy.log")
    os.makedirs(os.path.dirname(log), exist_ok=True)
    for r in range(1, rounds + 1):
        open(log, "w").close()
        codes = [run(c, log)[0] for c in CLIPPY]
        text = open(log, encoding="utf-8").read()
        errors = sorted(set(ERR.findall(text)))
        unused = [l for l in text.split("\n") if "unused import" in l]
        print(f"clippy round {r}: exit {codes}, {len(errors)} distinct error lines, {len(unused)} unused-import lines")
        if all(c == 0 for c in codes):
            return 0
        if not unused:
            break
        subprocess.run([sys.executable, os.path.join(ROOT, "scripts", "refactor", "fix_unused_imports.py"), ROOT, log, "--apply"], cwd=ROOT)
        subprocess.run("git diff --name-only -- '*.rs' | xargs rustfmt --edition 2021", cwd=ROOT, shell=True, capture_output=True)
    for l in sorted(set(re.findall(r"^(?:src|crates|tests)[^\n]*", text, re.M)))[:40]:
        print("  " + l[:200])
    return 1


def lines(path, rev=None):
    if rev:
        return subprocess.run(["git", "show", f"{rev}:{path}"], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout.count("\n")
    return open(os.path.join(ROOT, path), encoding="utf-8", errors="replace").read().count("\n")


def commit(plan_path):
    plan = json.load(open(plan_path, encoding="utf-8"))
    entries = plan.items() if isinstance(plan, dict) else ((p[0], p[1:]) for p in plan)
    trailer = "\n\nCo-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
    for hub, rest in entries:
        what, paths = (rest[0], rest[1:]) if isinstance(plan, dict) else ("split into children", rest)
        paths = [hub] + list(paths)
        subprocess.run(["git", "add", "--"] + paths, cwd=ROOT, check=True, capture_output=True)
        msg = f"{hub}: {what} ({lines(hub, 'HEAD')} lines -> {lines(hub)}){trailer}"
        r = subprocess.run(["git", "commit", "-q", "-m", msg, "--"] + paths, cwd=ROOT, capture_output=True, text=True)
        print(("ok   " if not r.returncode else "FAIL ") + hub + ("" if not r.returncode else " " + r.stderr[-200:]))
    return 0


def consistency():
    bad = 0
    for s in CONSISTENCY:
        cmd = ["pwsh", "-NoProfile", "-Command", f"./scripts/{s}.ps1 *> $null; if (Test-Path variable:\\LASTEXITCODE) {{ exit $LASTEXITCODE }}"]
        code = subprocess.run(cmd, cwd=ROOT).returncode
        if code:
            bad += 1
            print(f"FAIL {s} (exit {code}) - run it by hand for the detail")
    print(f"consistency: {len(CONSISTENCY) - bad}/{len(CONSISTENCY)} clean")
    return 1 if bad else 0


def main():
    what = sys.argv[1] if len(sys.argv) > 1 else "clippy"
    if what == "clippy":
        return clippy()
    if what == "commit":
        return commit(sys.argv[2])
    if what == "consistency":
        return consistency()
    if what == "all":
        return clippy() or consistency() or commit(sys.argv[2])
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
