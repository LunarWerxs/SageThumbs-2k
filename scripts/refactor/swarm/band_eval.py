"""Re-scan and compare the warn band for the files a swarm touched.

Usage: python band_eval.py [--before tmp/cx.json] [--after tmp/cx-after.json] [--files a.rs,b.rs]
Without --files, uses every modified .rs in `git status`. Prints per file: functions that left
the band, that stayed, that are NEW in the band, and the total band count before/after.
"""
import argparse
import json
import os
import subprocess

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))


def run(cmd):
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--before", default="tmp/cx.json")
    ap.add_argument("--after", default="tmp/cx-after.json")
    ap.add_argument("--files", default="")
    ap.add_argument("--no-scan", action="store_true")
    a = ap.parse_args()

    if not a.no_scan:
        out = run(["python", "scripts/complexity-scan.py", "--root", ".", "--json", "--warnings"])
        with open(os.path.join(ROOT, a.after), "w", encoding="utf-8") as fh:
            fh.write(out)
    before = json.load(open(os.path.join(ROOT, a.before), encoding="utf-8"))
    after = json.load(open(os.path.join(ROOT, a.after), encoding="utf-8"))

    if a.files:
        files = [f for f in a.files.split(",") if f]
    else:
        files = [l[3:].strip() for l in run(["git", "status", "--short"]).split("\n") if l[:2].strip() in ("M", "A", "AM", "MM") and l.strip().endswith(".rs")]

    key = lambda x: (x["file"], x["function"], x["metric"])
    b = {key(x): x["score"] for x in before}
    af = {key(x): x["score"] for x in after}
    left = stayed = new = 0
    for f in files:
        bk = {k: v for k, v in b.items() if k[0] == f}
        ak = {k: v for k, v in af.items() if k[0] == f}
        rows = []
        for k, v in sorted(bk.items()):
            if k in ak:
                rows.append(f"    STAYED {k[1]} {k[2]} {v}->{ak[k]}")
                stayed += 1
            else:
                rows.append(f"    left   {k[1]} {k[2]} {v}")
                left += 1
        for k, v in sorted(ak.items()):
            if k not in bk:
                rows.append(f"    NEW    {k[1]} {k[2]} {v}")
                new += 1
        print(f"{f}: before {len(bk)} after {len(ak)}")
        for r in rows:
            print(r)
    print(f"touched files {len(files)}: left {left}, stayed {stayed}, new {new}; band total {len(before)} -> {len(after)}")


if __name__ == "__main__":
    main()
