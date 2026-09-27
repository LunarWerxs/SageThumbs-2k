"""compare-renders.py's two modes: known flattened colours (--expect) and old-vs-new (--old)."""

import concurrent.futures as cf
import os
import sys

from compare_renders_pixels import compare_job, expect_job, load_expected


def print_expect_report(job_count, bad, missing):
    print(f"=== {job_count} files with a known flattened colour, "
          f"{job_count - len(bad)} correct ===")
    for name, w, got, verdict in bad:
        print(f"  {name:<44} want rgb{w}  got {got}  [{verdict}]")
    # A manifest entry with no sample behind it is a silently EMPTY check, which is the
    # failure mode this whole file exists to stop. Say so; do not quietly pass.
    for name in missing:
        print(f"  {name:<44} NOT IN THE CORPUS (re-run build-corpus.ps1)")


def run_expect_mode(a, files):
    want = load_expected(a.expect)
    jobs = [(a.new, f, a.out, a.size, a.timeout, want[os.path.basename(f)], a.rendered)
            for f in files if os.path.basename(f) in want]
    missing = sorted(set(want) - {os.path.basename(f) for f in files})
    bad = []
    with cf.ThreadPoolExecutor(max_workers=a.jobs) as pool:
        for name, w, got, verdict in pool.map(expect_job, jobs):
            if verdict != "ok":
                bad.append((name, w, got, verdict))
    print_expect_report(len(jobs), bad, missing)
    return 1 if (bad or missing) else 0


def classify_pair(name, ra, rb, delta, threshold):
    """Sort one compare_job result into its bucket name, or None to count as 'same'."""
    if ra == "ok" and rb != "ok":
        return "lost", (name, rb)
    if ra != "ok" and rb == "ok":
        return "gained", (name, ra)
    if ra != "ok":
        return "skip", None
    if isinstance(delta, str):
        return "error", (name, delta)
    if delta >= threshold:
        return "changed", (name, delta)
    return "same", None


def run_differential_pool(files, jobs, threshold, worker_count):
    changed, lost, gained, same, errs = [], [], [], 0, []
    buckets = {"lost": lost, "gained": gained, "changed": changed, "error": errs}
    with cf.ThreadPoolExecutor(max_workers=worker_count) as pool:
        for i, (name, ra, rb, delta) in enumerate(pool.map(compare_job, jobs), 1):
            if i % 25 == 0:
                print(f"  ...{i}/{len(files)}", file=sys.stderr, flush=True)
            kind, entry = classify_pair(name, ra, rb, delta, threshold)
            if kind == "same":
                same += 1
            elif kind != "skip":
                buckets[kind].append(entry)
    return changed, lost, gained, same, errs


def print_differential_report(total, same, lost, gained, changed, errs):
    print(f"\n=== {total} samples, {same} pixel-identical ===")
    print(f"\nLOST a thumbnail ({len(lost)}):")
    for n, why in sorted(lost):
        print(f"  {n:<44} new={why}")
    print(f"\nGAINED a thumbnail ({len(gained)}):")
    for n, why in sorted(gained):
        print(f"  {n:<44} old={why}")
    print(f"\nPICTURE CHANGED ({len(changed)}), worst first:")
    for n, d in sorted(changed, key=lambda x: -x[1]):
        print(f"  {n:<44} mean abs delta {d:6.1f}")
    if errs:
        print(f"\nUNREADABLE OUTPUT ({len(errs)}):")
        for n, e in errs:
            print(f"  {n:<44} {e}")


def run_differential_mode(a, files):
    jobs = [(a.old, a.new, f, a.out, a.size, a.timeout) for f in files]
    changed, lost, gained, same, errs = run_differential_pool(files, jobs, a.threshold, a.jobs)
    print_differential_report(len(files), same, lost, gained, changed, errs)
    return 1 if (lost or changed or errs) else 0
