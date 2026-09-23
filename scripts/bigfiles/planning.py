"""Which files the gate grows, how, and the grown twins (see bigfiles.py)."""

import json
import os
import subprocess

import ballast
import bigpixels
from common import BIG, CORPUS, ROOT, SIZES, guard_memory, max_size


def formats(st2k):
    """{ext: category} for every registered extension."""
    out = subprocess.run([st2k, "formats", "--json"], capture_output=True, check=True).stdout
    return {f["ext"]: f["category"] for f in json.loads(out)}


def sample_for(ext):
    for name in (f"real.{ext}", f"sample.{ext}"):
        if os.path.isfile(os.path.join(CORPUS, name)):
            return name
    return None


def sniff_strategy(path, tail_ok):
    head = open(path, "rb").read(16)
    if head.startswith(b"8BPS"):
        return "psd-layers"
    if head.startswith(b"PK\x03\x04"):
        return "zip-last"
    if head[4:8] == b"ftyp":
        return "isobmff-free"
    data_end = open(path, "rb").read()[-200:]
    if b"APETAGEX" in data_end:
        return "before-tail-tag"
    return "tail" if tail_ok else None


def plan(st2k, only, manifest):
    tail = {}
    tail_json = os.path.join(ROOT, "tmp", "bigfiles", "tail.json")
    if os.path.isfile(tail_json):
        tail = json.load(open(tail_json, encoding="utf-8"))
    cases, waived = [], {}
    for ext, category in sorted(formats(st2k).items()):
        if only and ext not in only:
            continue
        entry = manifest.get(ext, {})
        if "waive" in entry:
            waived[ext] = entry["waive"]
            continue
        names = entry.get("samples") or [sample_for(ext)]
        if names == [None]:
            waived[ext] = "no corpus sample"
            continue
        for i, name in enumerate(names):
            path = os.path.join(CORPUS, name)
            strategy = entry.get("strategy") or sniff_strategy(path, tail.get(name, "tail") == "tail")
            if strategy is None:
                waived[ext] = "UNDECIDED: appended bytes change its picture and no strategy is set"
                continue
            case_id = ext if i == 0 else f"{ext}~{os.path.splitext(name)[0]}"
            cases.append({"ext": case_id, "sample": name, "path": path, "strategy": strategy,
                          "category": category, "waive_sizes": entry.get("waive_sizes", {})})
    return cases, waived


def grow(case, work):
    """The case's twins, by size label: {label: path} (a failed grow is recorded, not raised)."""
    guard_memory()
    twins, errors = {}, {}
    labels = ["300M"] if case["strategy"] in ballast.PHYSICAL else list(SIZES)
    labels = [label for label in labels if SIZES[label] <= max_size(case["ext"])]
    for label in labels:
        ext = os.path.splitext(case["sample"])[1]
        dst = os.path.join(work, f"{case['ext']}.{label}{ext}")
        try:
            ballast.STRATEGIES[case["strategy"]](case["path"], dst, SIZES[label])
            twins[label] = dst
        except Exception as e:  # noqa: BLE001 - the report says what broke
            errors[label] = f"{type(e).__name__}: {e}"
    return twins, errors


def pixel_cases(work, only):
    """The pixel axis's cases: each genuinely big picture, its reference as the "normal" file."""
    cases = []
    for case_id, big, ref in bigpixels.make(work):
        if only and case_id.split("~")[0] not in only and case_id not in only:
            continue
        cases.append({"ext": case_id, "sample": os.path.basename(big), "path": ref,
                      "strategy": "pixels", "category": "Image", "twins": {BIG: big},
                      "grow_errors": {}})
    return cases
