"""Every surface's run of every case, and the verdict rows judged from them (see bigfiles.py)."""

import argparse
import os
from concurrent.futures import ThreadPoolExecutor

from common import BIG, SURFACE_SIZES, guard_memory
from judging import judge, load
from surfaces import run_cli, run_dll, run_quick


def cli_and_quick(a, case, out):
    """One case's normal file and twins through `st2k thumbnail` and the Quick preview probe."""
    guard_memory()
    runs = {}
    ext = case["ext"]
    for label, path in [("normal", case["path"])] + list(case["twins"].items()):
        if label == "normal" or label in SURFACE_SIZES["cli"] or label == BIG:
            runs[("cli", label)] = run_cli(a.st2k, path, os.path.join(out, f"{ext}.{label}.cli.png"))
        # Quick preview PLAYS a video (Media Foundation); the probe covers what it decodes.
        if case["category"] != "Video" and (label in ("normal", BIG) or label in SURFACE_SIZES["quick"]):
            runs[("quick", label)] = run_quick(a.app, path, os.path.join(out, f"{ext}.{label}.quick.png"))
    return runs


def shell_rows(cases):
    """(id, path) of every file the Explorer thumbnail and preview pane take."""
    rows = []
    for case in cases:
        rows.append((f"{case['ext']}.normal", case["path"]))
        rows.extend((f"{case['ext']}.{label}", case["twins"][label])
                    for label in ("300M", BIG) if label in case["twins"])
    return rows


def run_all(a, cases, test_exe):
    """Every surface's run of every case: [{(surface, label): (png, ms, dims)}], case order."""
    out = os.path.join(a.work, "out")
    with ThreadPoolExecutor(a.jobs) as pool:
        per_case = list(pool.map(lambda c: cli_and_quick(a, c, out), cases))
    dll = run_dll(test_exe, shell_rows(cases), os.path.join(out, "dll"), a.jobs)
    for case, runs in zip(cases, per_case):
        for label in ("normal", "300M", BIG):
            rid = f"{case['ext']}.{label}"
            for surface in ("thumb", "pane"):
                if rid in dll:
                    runs[(surface, label)] = dll[rid][surface]
    return per_case


def row(case, surface, size, verdict, why):
    return {"ext": case["ext"], "sample": case["sample"], "strategy": case["strategy"],
            "surface": surface, "size": size, "verdict": verdict, "why": why}


def shell_blind_spot(case, surface, runs):
    """An Explorer surface that draws nothing for the NORMAL file while st2k draws it is a bug
    at any size, which the size comparison would only call a SKIP: that is how every WMA
    shipped without its cover in Explorer."""
    normal, cli_normal = runs.get((surface, "normal")), runs.get(("cli", "normal"))
    if surface not in ("thumb", "pane") or case["strategy"] == "pixels" or not (normal and cli_normal):
        return None
    if load(normal[0]) is None and load(cli_normal[0]) is not None:
        return row(case, surface, "normal", "FAIL", "the normal-size file draws nothing here, though st2k draws it")
    return None


def blank_normal(case, runs):
    """The normal-size file through st2k drawing one flat colour is no picture at all, and
    every size comparison against it would pass: that is how a Photoshop file drawing an empty
    tile everywhere read as clean."""
    img = load(runs.get(("cli", "normal"), (None,))[0])
    if case["strategy"] == "pixels" or img is None or img.max() != img.min():
        return None
    return row(case, "cli", "normal", "FAIL", "the normal-size file draws a blank picture")


def size_verdicts(case, surface, labels, runs):
    """Each twin against the normal file on one surface; a waived size's FAIL is a SKIP with
    its reason, never a silent pass."""
    reference = case["strategy"] == "pixels"
    normal = runs.get((surface, "normal"))
    rows = []
    for label in [BIG] if reference else labels:
        big = runs.get((surface, label))
        if normal is None or big is None:
            continue
        verdict, why = judge(surface, normal, big, reference, same_frame=case["strategy"] != "repeat")
        waiver = case.get("waive_sizes", {}).get(label)
        if waiver and verdict == "FAIL":
            verdict, why = "SKIP", f"waived at {label}: {waiver}"
        rows.append(row(case, surface, label, verdict, why))
    return rows


def verdicts(case, runs):
    """Every result row for one case: grow failures, blind spots, then each size."""
    rows = [{"ext": case["ext"], "surface": "grow", "size": label, "verdict": "FAIL", "why": err}
            for label, err in case["grow_errors"].items()]
    blank = blank_normal(case, runs)
    if blank:
        rows.append(blank)
    for surface, labels in SURFACE_SIZES.items():
        blind = shell_blind_spot(case, surface, runs)
        if blind:
            rows.append(blind)
        rows += size_verdicts(case, surface, labels, runs)
    return rows


def confirm_alone(a, cases, results, test_exe):
    """Re-run every case with a FAIL on its own (one job) and re-judge it. A decode budget can
    run out when the gate itself runs six heavy decodes at once (a 300 MB workbook's Quick
    preview and an Ogg video's pane did, 2026-09-23, and passed alone): such a row becomes a
    PASS that SAYS it failed under the gate's load, and only a failure that reproduces alone
    stays a FAIL."""
    failing = {r["ext"] for r in results if r["verdict"] == "FAIL" and r["surface"] != "grow"}
    again = [c for c in cases if c["ext"] in failing]
    if not again:
        return results
    solo = argparse.Namespace(**{**vars(a), "jobs": 1})
    rerun = {c["ext"]: rows for c, rows in zip(again, (verdicts(c, r) for c, r in zip(again, run_all(solo, again, test_exe))))}
    out = []
    for r in results:
        if r["verdict"] == "FAIL" and r["ext"] in rerun:
            twin = next((x for x in rerun[r["ext"]] if x["surface"] == r["surface"] and x["size"] == r["size"]), None)
            if twin is None or twin["verdict"] == "PASS":
                r = {**r, "verdict": "PASS", "why": f"passed alone; under the gate's parallel load: {r['why']}"}
        out.append(r)
    return out
