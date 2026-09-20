"""Parse the Architect's code-duplication report into clone groups the swarm can work from.

Groups whose every site sits in test code are dropped first: the report LISTS clone windows
inside `#[cfg(test)]` modules and `#![cfg(test)]` files, but the check's own classifier does
not COUNT them, so a worker sent there moves nothing (wave 1, 2026-09-20: a third of the tasks).

Output: intra-file groups keyed by file, and cross-file groups bucketed into connected sets of
files (a set = every file that shares a group with another in it), smallest sets first.

usage: clones.py <repo-root> <report.md> [--json out.json]
"""
import json
import os
import re
import sys
from collections import defaultdict

SITE = re.compile(r"((?:src|crates|tests)/[\w/\.\-]+\.rs):(\d+)-(\d+)")
TEST_FILE = re.compile(r"^\s*#!\[cfg\(test\)\]", re.M)
TEST_MOD = re.compile(r"^#\[cfg\(test\)\]\s*\n(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", re.M)


def test_ranges(root, rel, cache={}):
    """Line ranges of `rel` that are test code: the whole file, or each inline test module."""
    if rel in cache:
        return cache[rel]
    path = os.path.join(root, rel)
    ranges = []
    if rel.startswith("tests/") or not os.path.isfile(path):
        ranges = [(1, 10**9)]
    else:
        text = open(path, encoding="utf-8", errors="replace").read()
        if TEST_FILE.search(text):
            ranges = [(1, 10**9)]
        else:
            lines = text.split("\n")
            for m in TEST_MOD.finditer(text):
                start = text.count("\n", 0, m.start()) + 1
                depth, end = 0, len(lines)
                for i in range(start, len(lines) + 1):
                    depth += lines[i - 1].count("{") - lines[i - 1].count("}")
                    if depth == 0 and i > start:
                        end = i
                        break
                ranges.append((start, end))
    cache[rel] = ranges
    return ranges


def is_test_site(root, site):
    rel, a, b = site
    return any(lo <= a and b <= hi for lo, hi in test_ranges(root, rel))


def parse_groups(report):
    groups, seen = [], set()
    for ln in open(report, encoding="utf-8").read().split("\n"):
        sites = SITE.findall(ln)
        if len(sites) < 2:
            continue
        key = tuple(sorted(set(sites)))
        if key not in seen:
            seen.add(key)
            groups.append([(f, int(a), int(b)) for f, a, b in key])
    return groups


def components(cross):
    """Connected sets of files over the cross-file groups (union-find)."""
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for g in cross:
        files = sorted({f for f, _, _ in g})
        for f in files[1:]:
            parent[find(f)] = find(files[0])
    sets = defaultdict(list)
    for g in cross:
        sets[find(g[0][0])].append(g)
    out = []
    for gs in sets.values():
        files = sorted({f for g in gs for f, _, _ in g})
        out.append({"files": files, "groups": gs})
    out.sort(key=lambda c: (len(c["files"]), -len(c["groups"])))
    return out


def main():
    root, report = sys.argv[1], sys.argv[2]
    groups = parse_groups(report)
    live = [g for g in groups if not all(is_test_site(root, s) for s in g)]
    intra = [g for g in live if len({f for f, _, _ in g}) == 1]
    cross = [g for g in live if len({f for f, _, _ in g}) > 1]
    by_file = defaultdict(list)
    for g in intra:
        by_file[g[0][0]].append(g)
    comps = components(cross)
    print(f"groups: {len(groups)}  live: {len(live)}  intra-file: {len(intra)} in {len(by_file)} files  cross-file: {len(cross)} in {len(comps)} file sets")
    for f, gs in sorted(by_file.items(), key=lambda kv: -sum(len(g) for g in kv[1]))[:20]:
        print(f"  intra {sum(len(g) for g in gs):3d} sites {len(gs):2d} groups  {f}")
    for c in comps[:20]:
        print(f"  cross {len(c['groups']):3d} groups over {len(c['files'])} files: {', '.join(c['files'])}"[:200])
    if "--json" in sys.argv:
        out = sys.argv[sys.argv.index("--json") + 1]
        json.dump({"intra": dict(by_file), "cross": comps}, open(out, "w", encoding="utf-8"), indent=1)
        print("wrote", out)


if __name__ == "__main__":
    main()
