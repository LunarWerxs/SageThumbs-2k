"""Turn an inline `mod NAME { ... }` block (any visibility, anywhere in the file) into a file
module: the body goes to `<stem>/NAME.rs` (or `NAME.rs` beside a mod.rs / crate root), the
declaration becomes `mod NAME;` with its visibility, and the doc comments / attributes above
it stay where they are.

usage: inline_mod_out.py <repo-root> <file.rs> NAME [--apply] [--root-file]
  --root-file: the file is a crate root (a `src/bin/x.rs`, say), so children sit beside it
"""
import os
import re
import sys

import _rs

MOD = re.compile(r"^(?P<vis>pub(\([^)]*\))?\s+)?mod (?P<name>\w+)\s*\{\s*$")


def find_block(lines, name):
    """(start index, visibility prefix) of `mod name {`, or (None, '')."""
    for i, ln in enumerate(lines):
        m = MOD.match(ln)
        if m and m.group("name") == name:
            return i, m.group("vis") or ""
    return None, ""


def is_test_only(lines, start):
    return any(ln.strip() == "#[cfg(test)]" for ln in lines[max(0, start - 3):start])


def main():
    root, rel, name = sys.argv[1], sys.argv[2], sys.argv[3]
    apply = "--apply" in sys.argv
    path = os.path.join(root, rel)
    lines, nl = _rs.read_lines(path)
    start, vis = find_block(lines, name)
    if start is None:
        _rs.die(f"no inline `mod {name} {{` in {rel}")
    end = _rs.closing_brace(lines, start)
    if end is None:
        _rs.die("no column-0 closing brace")
    target, beside = _rs.child_target(path, name, beside="--root-file" in sys.argv)
    body = _rs.moved_body(_rs.dedent(lines[start + 1:end]), beside)
    if is_test_only(lines, start):
        body = _rs.as_test_file(body)
    hub = lines[:start] + [f"{vis}mod {name};"] + lines[end + 1:]
    verb = "APPLY" if apply else "PLAN "
    print(f"{verb} {rel}: mod {name} lines {start + 1}-{end + 1} -> {os.path.relpath(target, root)}; file {len(lines)} -> {len(hub)}")
    if not apply:
        return
    if os.path.exists(target):
        _rs.die(f"target exists: {target}")
    while body and body[-1] == "":
        body.pop()
    _rs.write_lines(target, body + [""], nl)
    _rs.write_lines(path, hub, nl)


if __name__ == "__main__":
    main()
