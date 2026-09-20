"""Turn swarm results + the modified tree into per-directory commits with pathspecs.

Usage: python commit_plan.py --kind band --out tmp/band-full-out.json [--out tmp/band-pilot-out.json]
                             [--exclude a.rs,b.rs] [--apply]
       python commit_plan.py --kind churn --out tmp/churn-out.json [--apply]
Dry run prints the plan; --apply runs `git commit -q -m <msg> -- <paths>` per group.
"""
import argparse
import json
import os
import subprocess

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))


def git(*args):
    p = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace")
    return p.returncode, p.stdout


def task_id(path):
    return path.replace("/", "_").replace(".rs", "")


def modified_files():
    _, out = git("status", "--short")
    files = []
    for l in out.split("\n"):
        if len(l) < 4:
            continue
        code, path = l[:2], l[3:].strip()
        if code.strip() in ("M", "A", "AM", "MM", "??") and path:
            files.append(path)
    return files


def load_results(paths):
    res = {}
    for p in paths:
        try:
            d = json.load(open(os.path.join(ROOT, p), encoding="utf-8"))
        except OSError:
            continue
        r = d.get("results") or {}
        if isinstance(r, list):
            r = {x["id"]: x for x in r}
        res.update(r)
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kind", choices=["band", "churn"], required=True)
    ap.add_argument("--out", action="append", default=[])
    ap.add_argument("--exclude", default="")
    ap.add_argument("--only", default="")
    ap.add_argument("--apply", action="store_true")
    a = ap.parse_args()
    excluded = set(x for x in a.exclude.split(",") if x)
    only = set(x for x in a.only.split(",") if x)
    results = load_results(a.out)
    files = [f for f in modified_files() if f not in excluded and (not only or f in only)]

    groups = {}
    unmapped = []
    for f in files:
        if task_id(f) not in results:
            unmapped.append(f)
            continue
        key = os.path.dirname(f) if a.kind == "band" else f
        groups.setdefault(key, []).append(f)
    if unmapped:
        print("UNMAPPED (no task result; left out):", " ".join(unmapped))

    plan = []
    for key in sorted(groups):
        paths = groups[key]
        lines = []
        names = []
        n_left = 0
        for f in paths:
            r = results.get(task_id(f)) or {}
            data = r.get("data") or {}
            if a.kind == "band":
                for fn in data.get("functions", []):
                    if fn.get("after", 99) < 15 or fn.get("after") == -1:
                        n_left += 1
                        names.append(fn["name"])
                    lines.append(f"- {f}: {fn['name']} {fn['metric']} {fn['before']} -> {fn['after']}: {fn.get('action', '')}")
                if data.get("helpers_added"):
                    lines.append(f"  helpers: {', '.join(data['helpers_added'])}")
            else:
                tests = data.get("tests_added", [])
                names = tests
                lines.append(f"- {f}: {len(tests)} tests: {', '.join(tests)}")
                if data.get("extracted"):
                    lines.append(f"  extracted for testability: {', '.join(data['extracted'])}")
        if a.kind == "band":
            subject = f"complexity band ({key}): {n_left} function(s) below 15 - {', '.join(names)[:150]}"
        else:
            subject = f"churn hotspot: unit tests for {key} ({len(names)} tests)"
        body = "\n".join(lines)
        plan.append((subject, body, paths))

    for subject, body, paths in plan:
        print(subject)
        print("  paths:", " ".join(paths))
        for l in body.split("\n")[:6]:
            print("  ", l[:160])
    print(f"{len(plan)} commits over {len(files)} files")
    if a.apply:
        for subject, body, paths in plan:
            rc, out = git("commit", "-q", "-m", subject + "\n\n" + body, "--", *paths)
            print("commit", "ok" if rc == 0 else f"FAILED {rc}", subject[:80], out.strip()[:200])


if __name__ == "__main__":
    main()
