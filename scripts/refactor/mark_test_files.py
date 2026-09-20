"""Every test-only file module should say `#![cfg(test)]` at its own head: a file declared as
`#[cfg(test)] mod x;`, and every child of a file that already carries the inner attribute.

The parent's attribute is invisible from inside the file, so a per-file reader (the Architect's
clone and complexity classifiers, a reviewer opening the file cold) cannot tell test code from
production code without it. Semantically a no-op: the module is already compiled only for tests.

With --apply it repeats until nothing changes, because marking a file makes its children
eligible. Without it, only the first layer is listed.

usage: mark_test_files.py <repo-root> [--apply]
"""
import os
import re
import subprocess
import sys

import _rs

CFG = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
PATH = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]\s*$')
MOD = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
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
    mm = MOD.match(lines[j].strip()) if j < len(lines) else None
    return (mm.group(1), explicit) if mm else None


def test_only_declarations(lines):
    """(name, explicit path) of each file-module declaration that is test-only: one under a
    `#[cfg(test)]`, or ANY column-0 `mod x;` when the file itself is `#![cfg(test)]`."""
    whole = bool(HAS.search("\n".join(lines)))
    for i, ln in enumerate(lines):
        if CFG.match(ln):
            decl = declaration_after(lines, i)
            if decl:
                yield decl
        elif whole and MOD.match(ln):
            yield MOD.match(ln).group(1), None


def test_module_files(root, rel):
    """Paths (repo-relative) of every test-only file module `rel` declares."""
    lines, _ = _rs.read_lines(os.path.join(root, rel))
    base = children_dir(rel)
    for name, explicit in test_only_declarations(lines):
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


def one_pass(root, ours, apply):
    return sum(mark(root, t, apply) for rel in ours for t in set(test_module_files(root, rel)))


def main():
    root = sys.argv[1]
    apply = "--apply" in sys.argv
    # tracked AND untracked: the files a split just wrote are the ones that need the marker
    tracked = subprocess.run(["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"], cwd=root, capture_output=True, text=True).stdout.split("\n")
    ours = [r for r in tracked if r and "/vendor/" not in r and not r.startswith("vendor/")]
    total = changed = one_pass(root, ours, apply)
    while apply and changed:
        changed = one_pass(root, ours, apply)
        total += changed
    print(f"{total} file(s) {'marked' if apply else 'would be marked'}")


if __name__ == "__main__":
    main()
