"""Move the LAST `#[cfg(test)] mod x { ... }` block of an oversized .rs file into a sibling
file (`<stem>/x.rs`, or `x.rs` beside a mod.rs), leaving `#[cfg(test)]\nmod x;` behind.
Semantics are identical: the module path stays `parent::x`, so `use super::*` and
`super::super::y` resolve exactly as before. Run it again to take the next block.

Refuses when anything but blank lines or already-extracted test mod declarations follows
the block, or when the target file already exists.

usage: split_tests.py <repo-root> [--apply] file [file ...]
"""
import os
import re
import sys

import _rs

CFG = re.compile(r"^#\[cfg\(test\)\]\s*$")
MOD = re.compile(r"^(pub(\(crate\))? )?mod (\w+)\s*\{\s*$")
TRAIL = re.compile(r"^(#\[cfg\(test\)\]|#\[path = \"[^\"]+\"\]|(pub(\(crate\))? )?mod \w+;|)\s*$")


def last_test_block(lines):
    """Index of the `#[cfg(test)]` line that opens the last inline test module, or None."""
    start = None
    for i in range(len(lines) - 1):
        if CFG.match(lines[i]) and MOD.match(lines[i + 1]):
            start = i
    return start


def plan(root, rel):
    """(plan dict, None) or (None, reason)."""
    path = os.path.join(root, rel)
    lines, nl = _rs.read_lines(path)
    start = last_test_block(lines)
    if start is None:
        return None, "no trailing #[cfg(test)] mod block"
    decl = MOD.match(lines[start + 1])
    end = _rs.closing_brace(lines, start + 1)
    if end is None:
        return None, "no column-0 '}' closes the block"
    trailing = lines[end + 1:]
    stray = next((t for t in trailing if not TRAIL.match(t)), None)
    if stray is not None:
        return None, f"non-trivial line after the block: {stray!r}"
    target, beside = _rs.child_target(path, decl.group(3))
    if os.path.exists(target):
        return None, f"target exists: {target}"
    body = _rs.as_test_file(_rs.moved_body(_rs.dedent(lines[start + 2:end]), beside))
    kept_tail = [t for t in trailing if t.strip() != ""]
    hub = lines[:start] + ["#[cfg(test)]", f"{decl.group(1) or ''}mod {decl.group(3)};"] + kept_tail + [""]
    return {"path": path, "target": target, "hub": hub, "body": body + [""], "nl": nl, "code": start}, None


def main():
    root = sys.argv[1]
    apply = "--apply" in sys.argv
    for rel in [a for a in sys.argv[2:] if a != "--apply"]:
        p, err = plan(root, rel)
        if err:
            print(f"SKIP  {rel}: {err}")
            continue
        verb = "APPLY" if apply else "PLAN "
        print(f"{verb} {rel}: {p['code']} code lines stay, {len(p['body']) - 3} test lines -> {os.path.relpath(p['target'], root)}")
        if apply:
            _rs.write_lines(p["target"], p["body"], p["nl"])
            _rs.write_lines(p["path"], p["hub"], p["nl"])


if __name__ == "__main__":
    main()
