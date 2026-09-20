"""Move named top-level items out of an oversized .rs file into a child module file,
parent-hub style: the child starts with `use super::*;`, the hub gains
`mod <child>;` + `use <child>::*;`, and every moved item (and the fields / methods
inside it) that was private becomes `pub(super)` so hub and siblings still see it.

Relies on rustfmt layout: a top-level item starts at column 0 and its closing
brace is a lone `}` at column 0 (or the item ends with `;` on its own last line).
Leading `///`, `//`, `#[...]` lines (multi-line attributes included) travel with the item.

What it cannot know, and clippy -D warnings will tell you: a child that uses nothing from
its parent has an unused `use super::*` (delete it), and a hub that uses nothing from a
child has an unused `use child::*` (delete it; give the tests a `#[cfg(test)] use` if they
named something through it). Macro invocations (`thread_local!`) are not items and stay
behind - move them by hand. A child named like an extern crate (`exif`) shadows that crate
for everything under it. Run rustfmt on hub and child afterwards.

usage: extract_items.py <repo-root> <file.rs> <child> --items a,b,"impl X",... [--doc "..."] [--apply]
       extract_items.py <repo-root> <file.rs> x --list
"""
import os
import re
import sys

import _rs

ITEM = re.compile(
    r"^(?P<vis>pub(\([^)]*\))?\s+)?(?P<q>(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*)(?P<kind>fn|struct|enum|union|trait|type|const|static|impl|mod|macro_rules!)\b(?P<rest>.*)$"
)
IMPL = re.compile(r"^impl(<[^>]*>)?\s+(?:(?P<trait>[\w:<>, ]+?)\s+for\s+)?(?P<ty>[\w:]+)")
LEAD = re.compile(r"^(///|//|#\[|#!\[)")
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
CLOSER = re.compile(r"^[\]\)}]+;?$")
SUPER = re.compile(r"^(\s*)pub\(super\) ")
FIELD = re.compile(r"^    [a-z_][a-z0-9_]*: ")
METHOD = re.compile(r"^    (const |unsafe )?fn ")
MOD_DECL = re.compile(r"^(pub(\([^)]*\))? )?mod \w+;$")
GLOB_USE = re.compile(r"^use \w+::\*;$")


# --- parsing ---------------------------------------------------------------------------


def item_key(ln, m):
    """'fn name', 'struct name', 'impl Ty', 'impl Trait for Ty', 'macro name' - or None."""
    kind = m.group("kind")
    rest = m.group("rest").strip()
    if kind == "impl":
        im = IMPL.match(ln)
        if im is None:
            return None
        trait = im.group("trait")
        return f"impl {trait + ' for ' if trait else ''}{im.group('ty')}"
    if kind == "macro_rules!":
        return "macro " + rest.split("{")[0].strip()
    name = NAME.match(rest)
    return f"{kind} {name.group(0)}" if name else None


def attr_open(lines, idx):
    """Index of the `#[` line opening the multi-line attribute that closes at `idx`, or None."""
    k = idx
    while k > 0 and not lines[k].startswith("#["):
        k -= 1
    return k if lines[k].startswith("#[") else None


def lead_start(lines, i):
    """First line of the doc comments / attributes that belong to the item at `i`."""
    start = i
    while start > 0:
        prev = lines[start - 1]
        if prev.startswith("#!["):
            break
        if LEAD.match(prev):
            start -= 1
            continue
        opened = attr_open(lines, start - 1) if prev in (")]", "]") else None
        if opened is None:
            break
        start = opened
    return start


def scan_to(lines, j, pred):
    while j < len(lines) and not pred(lines[j]):
        j += 1
    return j


def item_end(lines, i):
    """Exclusive end line of the item that starts at `i`."""
    first = lines[i].rstrip()
    if first.endswith(";"):
        return i + 1
    if first.endswith(("{", "(")):
        return scan_to(lines, i + 1, lambda ln: ln == "}" or CLOSER.match(ln) is not None) + 1
    j = scan_to(lines, i, lambda ln: ln.rstrip().endswith(("{", ";")))
    if j < len(lines) and lines[j].rstrip().endswith(";"):
        return j + 1
    return scan_to(lines, j + 1, lambda ln: ln == "}") + 1


def parse_items(lines):
    """[(start incl. leading docs/attrs, end exclusive, key)] for every top-level item."""
    items = []
    i = 0
    while i < len(lines):
        ln = lines[i]
        m = None if ln.startswith(" ") else ITEM.match(ln)
        key = item_key(ln, m) if m else None
        if key is None:
            i += 1
            continue
        end = item_end(lines, i)
        items.append((lead_start(lines, i), end, key))
        i = end
    return items


# --- visibility ------------------------------------------------------------------------


def mark_top(ln, m):
    """A column-0 item line: private -> `pub(super)`. Returns (line, (in_struct, in_impl))."""
    kind = m.group("kind")
    if not m.group("vis") and kind not in ("impl", "macro_rules!"):
        ln = "pub(super) " + ln
    in_struct = kind == "struct" and ln.rstrip().endswith("{")
    # inherent impls only: a trait impl's methods take no visibility qualifier
    in_impl = kind == "impl" and " for " not in ln
    return ln, (in_struct, in_impl)


def make_pub_super(block):
    """Private -> `pub(super)`, fields and inherent methods included. An item that was ALREADY
    `pub(super)` was visible to the hub's parent; one level deeper that is
    `pub(in super::super)`, and the hub re-exports it by name."""
    out = []
    state = (False, False)
    for ln in (SUPER.sub(r"\1pub(in super::super) ", raw) for raw in block):
        top = None if ln.startswith(" ") else ITEM.match(ln)
        if top:
            ln, state = mark_top(ln, top)
        elif (state[0] and FIELD.match(ln)) or (state[1] and METHOD.match(ln)):
            ln = "    pub(super) " + ln[4:]
        out.append(ln)
    return out


def declared_vis(block):
    for ln in block:
        m = ITEM.match(ln)
        if m:
            return (m.group("vis") or "").strip()
    return ""


def reexports(lines, found, child):
    """Items that were `pub` / `pub(crate)` / `pub(super)` keep that reach through the hub: a
    private glob import does not re-export them, so they are named."""
    by_vis = {}
    for s, e, key in found:
        kind, _, name = key.partition(" ")
        vis = declared_vis(lines[s:e])
        if vis and kind not in ("impl", "macro"):
            by_vis.setdefault(vis, []).append(name)
    return [f"{vis} use {child}::{{{', '.join(names)}}};" for vis, names in sorted(by_vis.items())]


# --- the move --------------------------------------------------------------------------


def carve(lines, found):
    """(hub lines that stay, child lines that move, how many lines moved)."""
    keep, moved, cursor, total = [], [], 0, 0
    for s, e, _ in found:
        keep.extend(lines[cursor:s])
        moved.extend(make_pub_super(lines[s:e]))
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


def insert_point(keep):
    at = after_mod_block(keep)
    if at is None:
        at = after_use_block(keep)
    return at or 0


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
    found = [it for it in parsed if it[2] in wanted]
    missing = wanted - {k for _, _, k in found}
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
