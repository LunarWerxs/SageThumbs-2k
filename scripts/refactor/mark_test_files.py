"""Every file module declared as `#[cfg(test)] mod x;` should say `#![cfg(test)]` at its own head.

The parent's attribute is invisible from inside the file, so a per-file reader (the Architect's
clone and complexity classifiers, a reviewer opening the file cold) cannot tell test code from
production code without it. Semantically a no-op: the module is already compiled only for tests.

usage: mark_test_files.py <repo-root> [--apply]      (prints what it would change without --apply)
"""
import os
import re
import subprocess
import sys

import _rs

CFG = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
PATH = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]\s*$')
MOD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
HAS = re.compile(r"^\s*#!\[cfg\(test\)\]", re.M)
ROOT_PARENTS = {"bin", "tests", "benches", "examples"}


def children_dir(rel):
    d, f = os.path.split(rel)
    if f in _rs.ROOT_FILES or os.path.basename(d) in ROOT_PARENTS:
        return d
    return os.path.join(d, f[:-3])


def declaration_after(lines, i):
    """(module name, explicit #[path] or None) declared under the `#[cfg(test)]` at `i`, or None."""
    explicit = None
    j = i + 1
    while j < len(lines) and lines[j].strip().startswith("#["):
        pm = PATH.match(lines[j])
        explicit = pm.group(1) if pm else explicit
        j += 1
    mm = MOD.match(lines[j]) if j < len(lines) else None
    return (mm.group(1), explicit) if mm else None


def test_module_files(root, rel):
    """Paths (repo-relative) of every file module `rel` declares under `#[cfg(test)]`."""
    lines, _ = _rs.read_lines(os.path.join(root, rel))
    for i, ln in enumerate(lines):
        decl = declaration_after(lines, i) if CFG.match(ln) else None
        if decl is None:
            continue
        name, explicit = decl
        base = children_dir(rel)
        cands = [os.path.join(os.path.dirname(rel), explicit)] if explicit else [
            os.path.join(base, name + ".rs"),
            os.path.join(base, name, "mod.rs"),
        ]
        target = next((c for c in cands if os.path.isfile(os.path.join(root, c))), None)
        if target:
            yield target


def mark(root, target, apply):
    """True when `target` lacked the attribute (and, with `apply`, now has it)."""
    path = os.path.join(root, target)
    lines, nl = _rs.read_lines(path)
    if HAS.search("\n".join(lines)):
        return False
    print(f"{'MARK ' if apply else 'WOULD'} {target.replace(os.sep, '/')}")
    if apply:
        _rs.write_lines(path, _rs.as_test_file(lines), nl)
    return True


def main():
    root = sys.argv[1]
    apply = "--apply" in sys.argv
    tracked = subprocess.run(["git", "ls-files", "*.rs"], cwd=root, capture_output=True, text=True).stdout.split("\n")
    ours = [r for r in tracked if r and "/vendor/" not in r and not r.startswith("vendor/")]
    changed = sum(mark(root, t, apply) for rel in ours for t in test_module_files(root, rel))
    print(f"{changed} file(s) {'marked' if apply else 'would be marked'}")


if __name__ == "__main__":
    main()
