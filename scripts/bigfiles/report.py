"""The gate's report: tmp/bigfiles/report.md and results.json."""

import json
import os

from common import BIG, SIZES, SURFACE_SIZES


def report(results, waived, n_cases, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, "results.json"), "w", encoding="utf-8") as f:
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
    with open(os.path.join(out_dir, "report.md"), "w", encoding="utf-8") as f:
        f.writelines(lines)
