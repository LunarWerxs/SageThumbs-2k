"""Parse the Architect's code-duplication report into clone groups and bucket them.
usage: clones.py <report.md> [--json out.json]
Prints: total findings, intra-file findings (every site in one file), the files they touch."""
import json
import re
import sys
from collections import defaultdict

SITE = re.compile(r"((?:src|crates|tests)/[\w/\.\-]+\.rs):(\d+)-(\d+)")
HEAD = re.compile(r"^- (\d+)\+ tokens[;,] (\d+) (?:sites?|merged windows)")

rep = open(sys.argv[1], encoding="utf-8").read().split("\n")
groups = []
seen = set()
for ln in rep:
    sites = SITE.findall(ln)
    if len(sites) < 2:
        continue
    key = tuple(sorted(set(sites)))
    if key in seen:
        continue
    seen.add(key)
    groups.append([(f, int(a), int(b)) for f, a, b in key])

intra = [g for g in groups if len({f for f, _, _ in g}) == 1]
cross = [g for g in groups if len({f for f, _, _ in g}) > 1]
by_file = defaultdict(list)
for g in intra:
    by_file[g[0][0]].append(g)
print(f"groups: {len(groups)}  intra-file: {len(intra)}  cross-file: {len(cross)}  files with intra-file clones: {len(by_file)}")
rows = sorted(by_file.items(), key=lambda kv: -sum(len(g) for g in kv[1]))
for f, gs in rows[:60]:
    print(f"{sum(len(g) for g in gs):3d} sites {len(gs):2d} groups  {f}")
if "--json" in sys.argv:
    out = sys.argv[sys.argv.index("--json") + 1]
    json.dump({f: gs for f, gs in rows}, open(out, "w", encoding="utf-8"), indent=1)
    print("wrote", out)
