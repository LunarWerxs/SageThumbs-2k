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
from common import MANIFEST, ROOT, TARGET, cargo_target  # noqa: E402
import bigpixels  # noqa: E402
from runs import confirm_alone, run_all, verdicts  # noqa: E402
from planning import grow, pixel_cases, plan  # noqa: E402
from report import report  # noqa: E402
from surfaces import build, find_test_exe  # noqa: E402


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
