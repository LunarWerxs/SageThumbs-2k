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
Report: tmp/bigfiles/report.md (+ results.json).
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import numpy as np
from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ballast  # noqa: E402
import bigpixels  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
CORPUS = os.path.abspath(os.path.join(ROOT, "..", "test-corpus"))


def cargo_target():
    """The workspace's cargo target directory, from `cargo metadata` - never a path typed here."""
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ROOT,
                         capture_output=True, text=True, check=True).stdout
    return json.loads(out)["target_directory"]


TARGET = os.path.join(cargo_target(), "release")
MANIFEST = os.path.join(HERE, "big-files.json")
MiB = 1 << 20
SIZES = {"300M": 300 * MiB, "2.2G": 2253 * MiB, "5G": 5120 * MiB}
SURFACE_SIZES = {"cli": ["300M", "2.2G"], "thumb": ["300M"], "pane": ["300M"],
                 "quick": ["300M", "2.2G", "5G"]}
# The pixel axis: one genuinely big file per case, on every surface, against its reference.
BIG = "big"
# The largest file a format can LEGITIMATELY be. A twin past it is not a real file of that
# format (a classic TIFF or a PSD with gigabytes of junk after it), so failing on it would be
# a false alarm. Most formats address their data with 32-bit offsets: 4 GiB. PSD's own limit
# is 2 GB, which is why PSB exists. These carry 64-bit offsets and really do reach 5 GB.
FOUR_GIB = (4 << 30) - 1
# A version-3 compound file (512-byte sectors: legacy Office, SolidWorks, Publisher, Visio)
# is limited to 2 GB by its own specification; 3ds Max writes version 4.
TWO_GIB = (2 << 30) - 1
MAX_SIZE = {"psd": TWO_GIB, **{e: TWO_GIB for e in (
    "doc", "dot", "xls", "xlt", "ppt", "pot", "pps", "pub", "vsd", "sldprt", "sldasm", "slddrw")}}
OVER_4_GIB = {
    "psb", "exr", "xcf", "blend", "fits", "fts", "fit", "mp4", "m4v", "mov", "mkv", "webm", "3gp",
    "3g2", "zip", "cbz", "7z", "cb7", "kra", "ora", "epub", "iso",
}


# ---------------------------------------------------------------- which files, which ballast

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


def max_size(ext):
    base = ext.split("~")[0]
    return MAX_SIZE.get(base, 1 << 50 if base in OVER_4_GIB else FOUR_GIB)


# This runs on a shared box: never let the gate itself become the thing that starves it.
MEMORY_CEILING = 3 << 30


def guard_memory():
    try:
        import psutil
    except ImportError:
        return
    used = psutil.Process().memory_info().private
    if used > MEMORY_CEILING:
        raise SystemExit(f"bigfiles: stopped, the gate itself holds {used >> 20} MB")


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


# ---------------------------------------------------------------- the surfaces

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


# ---------------------------------------------------------------- the judgement

def load(path):
    if not path or not os.path.isfile(path):
        return None
    try:
        return np.asarray(Image.open(path).convert("RGB"), dtype=np.float32)
    except OSError:
        return None


def sharpness(a):
    g = a.mean(axis=2)
    return float(np.abs(np.diff(g, axis=0)).mean() + np.abs(np.diff(g, axis=1)).mean())


def budget(surface, normal_ms):
    floor = {"cli": 10000, "thumb": 10000, "pane": 15000, "quick": 15000}[surface]
    return max(floor, 3 * normal_ms + 3000)


def judge_reference(a, b):
    """A big picture against its reference, each fitted by its own surface: the same shape, and
    the same picture once both are brought to the smaller size."""
    (ah, aw), (bh, bw) = a.shape[:2], b.shape[:2]
    if abs(aw / ah - bw / bh) > 0.05 * (aw / ah):
        return "FAIL", f"a {bw}x{bh} picture where the reference is {aw}x{ah}"
    w, h = min(aw, bw), min(ah, bh)
    fit = lambda x: np.asarray(Image.fromarray(x.astype(np.uint8)).resize((w, h), Image.BOX), np.float32)
    a, b = fit(a), fit(b)
    diff = float(np.abs(a - b).mean())
    sa, sb = sharpness(a), sharpness(b)
    if diff > 12 or (sa > 1 and sb / sa < 0.7):
        return "FAIL", f"not the reference picture (mean diff {diff:.1f}, sharpness {sb:.1f} vs {sa:.1f})"
    return "PASS", ""


def judge(surface, normal, big, reference=False, same_frame=True):
    """normal/big = (png path or None, ms, dims or None). ('PASS'|'FAIL'|'SKIP', why). With
    `reference`, `normal` is the reference picture of a genuinely big file (the pixel axis).
    Without `same_frame` (a video grown by repeating its content), the big twin is a longer
    video and may rightly show another frame of it: its size and sharpness are still judged,
    its pixels are not compared."""
    n_png, n_ms, n_dims = normal
    b_png, b_ms, b_dims = big
    a, b = load(n_png), load(b_png)
    if a is None:
        return "SKIP", "the normal-size sample renders nothing here either"
    if b is None:
        return "FAIL", "nothing rendered"
    if reference:
        verdict, why = judge_reference(a, b)
        if verdict == "FAIL":
            return verdict, why
        if b_ms > budget(surface, n_ms) * 4:
            return "FAIL", f"too slow: {b_ms} ms against {n_ms} ms for the reference"
        return "PASS", f"{b_ms} ms"
    if n_dims and b_dims and n_dims != b_dims:
        return "FAIL", f"a {b_dims[0]}x{b_dims[1]} picture where the normal file gets {n_dims[0]}x{n_dims[1]}"
    if a.shape != b.shape:
        return "FAIL", f"a {b.shape[1]}x{b.shape[0]} picture where the normal file gets {a.shape[1]}x{a.shape[0]}"
    diff = float(np.abs(a - b).mean()) if same_frame else 0.0
    sa, sb = sharpness(a), sharpness(b)
    if diff > 6 or (sa > 1 and sb / sa < 0.85):
        return "FAIL", f"a different or blurrier picture (mean diff {diff:.1f}, sharpness {sb:.1f} vs {sa:.1f})"
    if b_ms > budget(surface, n_ms):
        return "FAIL", f"too slow: {b_ms} ms against {n_ms} ms at normal size"
    return "PASS", f"{b_ms} ms"


# ---------------------------------------------------------------- driver

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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build", action="store_true")
    ap.add_argument("--only", default="")
    ap.add_argument("--jobs", type=int, default=8)
    # Beside the build, on the same NTFS volume (the twins are sparse files).
    ap.add_argument("--work", default=os.path.join(cargo_target(), "bigfiles"))
    ap.add_argument("--st2k", default=os.path.join(TARGET, "st2k.exe"))
    ap.add_argument("--app", default=os.path.join(TARGET, "SageThumbs2K.exe"))
    ap.add_argument("--axis", choices=["size", "pixels", "all"], default="all")
    a = ap.parse_args()

    test_exe = build() if a.build else find_test_exe()
    if not test_exe:
        raise SystemExit("no big_files test executable: run with --build")
    manifest = json.load(open(MANIFEST, encoding="utf-8")) if os.path.isfile(MANIFEST) else {}
    only = set(filter(None, a.only.split(",")))
    cases, waived = plan(a.st2k, only, manifest) if a.axis != "pixels" else ([], {})

    shutil.rmtree(a.work, ignore_errors=True)
    os.makedirs(a.work)
    out = os.path.join(a.work, "out")
    os.makedirs(out)
    with ThreadPoolExecutor(a.jobs) as pool:
        grown = list(pool.map(lambda c: grow(c, a.work), cases))
    for case, (twins, errors) in zip(cases, grown):
        case["twins"], case["grow_errors"] = twins, errors
    if a.axis != "size":
        cases += pixel_cases(os.path.join(a.work, "pixels"), only)

    def cli_and_quick(case):
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

    with ThreadPoolExecutor(a.jobs) as pool:
        per_case = list(pool.map(cli_and_quick, cases))
    rows = []
    for case in cases:
        rows.append((f"{case['ext']}.normal", case["path"]))
        for label in ("300M", BIG):
            if label in case["twins"]:
                rows.append((f"{case['ext']}.{label}", case["twins"][label]))
    dll = run_dll(test_exe, rows, os.path.join(out, "dll"), a.jobs)

    results, fails = [], 0
    for case, runs in zip(cases, per_case):
        ext = case["ext"]
        for rid in (f"{ext}.normal", f"{ext}.300M", f"{ext}.{BIG}"):
            for surface in ("thumb", "pane"):
                if rid in dll:
                    runs[(surface, rid.split(".", 1)[1])] = dll[rid][surface]
        for label, err in case["grow_errors"].items():
            results.append({"ext": ext, "surface": "grow", "size": label, "verdict": "FAIL", "why": err})
            fails += 1
        cli_normal = runs.get(("cli", "normal"))
        for surface, labels in SURFACE_SIZES.items():
            normal = runs.get((surface, "normal"))
            reference = case["strategy"] == "pixels"
            # An Explorer surface that draws nothing for the NORMAL file while st2k draws it is
            # a bug at any size, and the size comparison below would only call it a SKIP: that
            # is how every WMA shipped without its cover in Explorer.
            if (surface in ("thumb", "pane") and not reference and normal and cli_normal
                    and load(normal[0]) is None and load(cli_normal[0]) is not None):
                fails += 1
                results.append({"ext": ext, "sample": case["sample"], "strategy": case["strategy"],
                                "surface": surface, "size": "normal", "verdict": "FAIL",
                                "why": "the normal-size file draws nothing here, though st2k draws it"})
            for label in [BIG] if reference else labels:
                big = runs.get((surface, label))
                if normal is None or big is None:
                    continue
                verdict, why = judge(surface, normal, big, reference,
                                     same_frame=case["strategy"] != "repeat")
                # A size the manifest waives, with its reason: a SKIP, never a silent pass.
                waiver = case.get("waive_sizes", {}).get(label)
                if waiver and verdict == "FAIL":
                    verdict, why = "SKIP", f"waived at {label}: {waiver}"
                fails += verdict == "FAIL"
                results.append({"ext": ext, "sample": case["sample"], "strategy": case["strategy"],
                                "surface": surface, "size": label, "verdict": verdict, "why": why})

    report(results, waived, len(cases))
    print(f"bigfiles: {len(cases)} formats, {fails} failing twin(s), {len(waived)} waived "
          f"-> tmp/bigfiles/report.md")
    return 1 if fails else 0


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


def report(results, waived, n_cases):
    os.makedirs(os.path.join(ROOT, "tmp", "bigfiles"), exist_ok=True)
    with open(os.path.join(ROOT, "tmp", "bigfiles", "results.json"), "w", encoding="utf-8") as f:
        json.dump({"results": results, "waived": waived}, f, indent=1)
    lines = [f"# Big-file gate: {n_cases} formats\n"]
    counts = {}
    for r in results:
        key = (r["surface"], r["size"], r["verdict"])
        counts[key] = counts.get(key, 0) + 1
    lines.append("| surface | size | PASS | FAIL | SKIP |\n|---|---|---|---|---|\n")
    for surface, labels in [(s, ["normal"] + ls + [BIG]) for s, ls in SURFACE_SIZES.items()] + [("grow", list(SIZES))]:
        for label in labels:
            c = [counts.get((surface, label, v), 0) for v in ("PASS", "FAIL", "SKIP")]
            if any(c):
                lines.append(f"| {surface} | {label} | {c[0]} | {c[1]} | {c[2]} |\n")
    lines.append("\n## Failing\n\n")
    for r in results:
        if r["verdict"] == "FAIL":
            lines.append(f"- `{r['ext']}` {r['surface']} {r['size']} ({r.get('strategy', '')}): {r['why']}\n")
    lines.append("\n## Waived\n\n")
    lines.extend(f"- `{ext}`: {why}\n" for ext, why in sorted(waived.items()))
    with open(os.path.join(ROOT, "tmp", "bigfiles", "report.md"), "w", encoding="utf-8") as f:
        f.writelines(lines)


if __name__ == "__main__":
    sys.exit(main())
