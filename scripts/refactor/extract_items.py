"""Move named top-level items out of an oversized .rs file into a child module file,
parent-hub style: the child starts with `use super::*;`, the hub gains
`mod <child>;` + `use <child>::*;`, and every moved item (and the fields / methods
inside it) that was private becomes `pub(super)` so hub and siblings still see it.
A `super::x` path in a moved item gains one more `super::`, because the item sits one
module deeper (a hub that glob-imports its own parent hides this; one that does not, breaks).

Relies on rustfmt layout: a top-level item starts at column 0 and its closing
brace is a lone `}` at column 0 (or the item ends with `;` on its own last line).
Leading `///`, `//`, `#[...]` lines (multi-line attributes included) travel with the item.

What it cannot know, and clippy -D warnings will tell you: a child that uses nothing from
its parent has an unused `use super::*` (delete it), and a hub that uses nothing from a
child has an unused `use child::*` (`fix_unused_imports.py <clippy.log>` settles those: test-only
ones get `#[cfg(test)]`, dead ones go). A child named like a SIBLING module or an extern crate
(`update`, `ole`, `exif`) shadows it for the whole hub - pick another name. A TUPLE struct's
fields are not widened (`struct NamedTemp(PathBuf)` keeps its private `.0`); a sibling that
reads one needs `pub(super)` on the field by hand. Macro invocations (`thread_local!`) are not items and stay
behind - move them by hand. A child named like an extern crate (`exif`) shadows that crate
for everything under it. Run rustfmt on hub and child afterwards.

usage: extract_items.py <repo-root> <file.rs> <child> --items a,b,"impl X",... [--doc "..."] [--apply]
       extract_items.py <repo-root> <file.rs> x --list
"""
import os
import re
import sys

import _rs
from _items import make_pub_super, parse_items, reexports

MOD_DECL = re.compile(r"^(pub(\([^)]*\))? )?mod \w+;$")
GLOB_USE = re.compile(r"^use \w+::\*;$")


# --- the move --------------------------------------------------------------------------


def carve(lines, found):
    """(hub lines that stay, child lines that move, how many lines moved)."""
    keep, moved, cursor, total = [], [], 0, 0
    for s, e, key in found:
        keep.extend(lines[cursor:s])
        body = make_pub_super(lines[s:e])
        # an inline `mod x { use super::*; }` travels with its parent; its own paths stay right
        moved.extend(body if key.startswith("mod ") else _rs.deepen_supers(body))
        moved.append("")
        total += e - s
        # the blank line after the item goes with it
        cursor = e + 1 if e < len(lines) and lines[e] == "" else e
    keep.extend(lines[cursor:])
    return keep, moved, total


def after_mod_block(keep):
    at = None
    for idx, ln in enumerate(keep):
        if idx > 0 and keep[idx - 1].startswith("#[cfg(test)]"):
            continue  # test-only module declarations sit at the tail; never insert there
        if MOD_DECL.match(ln) or GLOB_USE.match(ln):
            at = idx + 1
    return at


def after_use_block(keep):
    at = None
    for idx, ln in enumerate(keep):
        if ln.startswith(("use ", "pub use ")):
            at = idx + 1
        elif at is not None and ln == "":
            break
    return at


def after_inner_docs(keep):
    """A file with no `mod`/`use` block yet: past its `//!` docs and `#![...]` attributes."""
    at = 0
    while at < len(keep) and keep[at].startswith(("//!", "#![")):
        at += 1
    return at


def insert_point(keep):
    at = after_mod_block(keep)
    if at is None:
        at = after_use_block(keep)
    return at or after_inner_docs(keep)


def list_items(parsed):
    for s, e, k in parsed:
        print(f"{s + 1:5d}-{e:5d} {e - s:4d}  {k}")


def main():
    root, rel, child = sys.argv[1], sys.argv[2], sys.argv[3]
    args = sys.argv[4:]
    path = os.path.join(root, rel)
    lines, nl = _rs.read_lines(path)
    parsed = parse_items(lines)
    if "--list" in args:
        return list_items(parsed)
    wanted = {s.strip() for s in _rs.flag_value(args, "--items").split(",") if s.strip()}
    # a bare name (`load_values`) matches every item that ends in it, whatever its kind
    found = [it for it in parsed if it[2] in wanted or it[2].split(" ", 1)[-1] in wanted]
    missing = wanted - {k for _, _, k in found} - {k.split(" ", 1)[-1] for _, _, k in found}
    if missing:
        _rs.die("MISSING: " + ", ".join(sorted(missing)))
    keep, moved, total = carve(lines, found)
    at = insert_point(keep)
    keep[at:at] = [f"mod {child};", f"use {child}::*;"] + reexports(lines, found, child)
    target, beside = _rs.child_target(path, child)
    doc = _rs.flag_value(args, "--doc")
    body = ([f"//! {doc}", ""] if doc else []) + ["use super::*;", ""] + _rs.moved_body(moved, beside)
    while body and body[-1] == "":
        body.pop()
    apply = "--apply" in args
    print(f"{'APPLY' if apply else 'PLAN '} {rel}: {len(found)} items, {total} lines -> {os.path.relpath(target, root)}; hub {len(lines)} -> {len(keep)} lines")
    if not apply:
        return None
    if os.path.exists(target):
        _rs.die(f"target exists: {target}")
    _rs.write_lines(target, body + [""], nl)
    _rs.write_lines(path, keep, nl)
    return None


if __name__ == "__main__":
    main()
