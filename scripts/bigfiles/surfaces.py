"""Running a file through each surface: st2k, Quick preview, and the DLL's Explorer
thumbnail and preview pane (tests/big_files.rs)."""

import json
import os
import subprocess
import time

from common import ROOT, TARGET


def run_cli(st2k, path, out):
    t = time.monotonic()
    try:
        r = subprocess.run([st2k, "thumbnail", path, out, "--size", "256"],
                           capture_output=True, timeout=180)
        ok = r.returncode == 0 and os.path.isfile(out)
    except subprocess.TimeoutExpired:
        ok = False
    return (out if ok else None), int((time.monotonic() - t) * 1000), None


def run_quick(app, path, out):
    t = time.monotonic()
    try:
        subprocess.run([app, "--probe-preview", path, out], capture_output=True, timeout=240)
    except subprocess.TimeoutExpired:
        pass
    wall = int((time.monotonic() - t) * 1000)
    try:
        w, h, ms = (int(x) for x in open(out + ".tsv").read().split())
    except (OSError, ValueError):
        return None, wall, None
    return (out if w and os.path.isfile(out) else None), ms, (w, h)


def run_dll(test_exe, rows, out_dir, shards):
    """Explorer thumbnail + preview pane for every (id, path), in `shards` parallel processes."""
    os.makedirs(out_dir, exist_ok=True)
    procs = []
    for i in range(shards):
        mine = rows[i::shards]
        if not mine:
            continue
        shard = os.path.join(out_dir, f"shard{i}")
        os.makedirs(shard, exist_ok=True)
        plan_path = os.path.join(shard, "plan.tsv")
        with open(plan_path, "w", encoding="utf-8") as f:
            f.writelines(f"{rid}\t{p}\n" for rid, p in mine)
        env = dict(os.environ, BIGFILES_PLAN=plan_path, BIGFILES_OUT=shard)
        procs.append((shard, subprocess.Popen(
            [test_exe, "--ignored", "--exact", "big_files_through_the_shell_surfaces",
             "--test-threads=1"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)))
    results = {}
    for shard, p in procs:
        p.wait()
        tsv = os.path.join(shard, "results.tsv")
        for line in open(tsv, encoding="utf-8") if os.path.isfile(tsv) else []:
            rid, tok, tw, th, tms, pok, pms = line.rstrip("\n").split("\t")
            results[rid] = {
                "thumb": (os.path.join(shard, f"{rid}.thumb.png") if tok == "1" else None,
                          int(tms), (int(tw), int(th))),
                "pane": (os.path.join(shard, f"{rid}.pane.png") if pok == "1" else None,
                         int(pms), None),
            }
    return results


def build():
    subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
    out = subprocess.run(["cargo", "test", "--release", "--test", "big_files", "--no-run",
                          "--message-format=json"], cwd=ROOT, check=True,
                         capture_output=True, text=True).stdout
    for line in out.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") == "compiler-artifact" and msg.get("executable") \
                and msg.get("target", {}).get("name") == "big_files":
            return msg["executable"]
    raise SystemExit("could not find the big_files test executable")


def find_test_exe():
    deps = os.path.join(TARGET, "deps")
    exes = [os.path.join(deps, n) for n in os.listdir(deps)
            if n.startswith("big_files-") and n.endswith(".exe")]
    return max(exes, key=os.path.getmtime) if exes else None
