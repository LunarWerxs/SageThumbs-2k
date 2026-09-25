"""The big-file gate: every format, grown past every size gate, through every surface.

WHY (issue #46, 2026-09-22). A 300 MB Photoshop document stayed on its 160-pixel preview in
Quick preview, and a 5 GB one never sharpened. No check caught it: the corpus's biggest PSD is
600 KB, and nothing had ever handed ANY surface a file past the 256 MiB input ceiling or the
2 GiB full-fidelity ceiling except three hand-picked cases. Behaviour that changes past a size
gate was unmeasured for every other format. Michael: "you need to build better tests, checks,
et cetera. And resolve it before this goes out."

WHAT. For each registered extension, its corpus sample (real.<ext>, else sample.<ext>) is grown
to 300 MiB, 2.2 GiB and 5 GiB by a ballast strategy that leaves its picture alone
(`ballast.py`; sparse, so this costs no disk). Each twin and the normal-size sample then go
through the same surface, and the twin must give the SAME picture:

    cli    `st2k thumbnail <f> --size 256`               normal, 300M, 2.2G
    thumb  the Explorer thumbnail, IStream -> the DLL     normal, 300M
    pane   the preview pane, IStream -> the DLL           normal, 300M
    quick  Quick preview, first stage + sharpen pass      normal, 300M, 2.2G, 5G

A twin FAILS when it renders nothing, a different size, a blurrier or different picture, or
takes far longer than its normal-size sample. 5 GiB is past the default MaxSize (4096 MB), so
cli/thumb/pane are not asked to render it: skipping it there is the user's own setting.

    python scripts/bigfiles/bigfiles.py [--build] [--only psd,psb] [--jobs 8]

Exit 0 when every twin passes or is waived with a reason in big-files.json; 1 otherwise.
Report: tmp/bigfiles/report.md (+ results.json)."""

import argparse
import json
import os
import shutil
import sys
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import BIG, MANIFEST, ROOT, SURFACE_SIZES, TARGET, cargo_target, guard_memory  # noqa: E402
import bigpixels  # noqa: E402
from judging import judge, load  # noqa: E402
from planning import grow, pixel_cases, plan  # noqa: E402
from report import report  # noqa: E402
from surfaces import build, find_test_exe, run_cli, run_dll, run_quick  # noqa: E402


def args():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="store_true")
    ap.add_argument("--only", default="")
    ap.add_argument("--jobs", type=int, default=8)
    # Beside the build, on the same NTFS volume (the twins are sparse files).
    ap.add_argument("--work", default=os.path.join(cargo_target(), "bigfiles"))
    ap.add_argument("--st2k", default=os.path.join(TARGET, "st2k.exe"))
    ap.add_argument("--app", default=os.path.join(TARGET, "SageThumbs2K.exe"))
    ap.add_argument("--axis", choices=["size", "pixels", "all"], default="all")
    # Where report.md and results.json go (a proof run against another build keeps its own).
    ap.add_argument("--report-dir", default=os.path.join(ROOT, "tmp", "bigfiles"))
    # The Explorer/pane driver (tests/big_files.rs) loads the sagethumbs2k.dll one folder above
    # itself, so a copy placed at <dir>/deps/ beside <dir>/sagethumbs2k.dll drives THAT build:
    # how the gate is run against a released DLL to prove it fails where that release failed.
    ap.add_argument("--test-exe", default="")
    return ap.parse_args()


def grow_all(a, cases, only):
    """Grow every case's twins into a fresh work dir; add the pixel axis's cases."""
    shutil.rmtree(a.work, ignore_errors=True)
    os.makedirs(os.path.join(a.work, "out"))
    with ThreadPoolExecutor(a.jobs) as pool:
        grown = list(pool.map(lambda c: grow(c, a.work), cases))
    for case, (twins, errors) in zip(cases, grown):
        case["twins"], case["grow_errors"] = twins, errors
    if a.axis != "size":
        cases += pixel_cases(os.path.join(a.work, "pixels"), only)
    return cases


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


def main():
    a = args()
    test_exe = a.test_exe or (build() if a.build else find_test_exe())
    if not test_exe:
        raise SystemExit("no big_files test executable: run with --build")
    manifest = json.load(open(MANIFEST, encoding="utf-8")) if os.path.isfile(MANIFEST) else {}
    only = set(filter(None, a.only.split(",")))
    cases, waived = plan(a.st2k, only, manifest) if a.axis != "pixels" else ([], {})
    cases = grow_all(a, cases, only)
    waived.update(bigpixels.NOT_GENERATED)
    per_case = run_all(a, cases, test_exe)
    results = [r for case, runs in zip(cases, per_case) for r in verdicts(case, runs)]
    results = confirm_alone(a, cases, results, test_exe)
    fails = sum(r["verdict"] == "FAIL" for r in results)
    report(results, waived, len(cases), a.report_dir)
    print(f"bigfiles: {len(cases)} formats, {fails} failing twin(s), {len(waived)} waived "
          f"-> {os.path.join(a.report_dir, 'report.md')}")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
