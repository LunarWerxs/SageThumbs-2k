"""Move named top-level items out of an oversized .rs file into a child module file,
parent-hub style: the child starts with `use super::*;`, the hub gains
`mod <child>;` + `use <child>::*;`, and every moved item (and the fields / methods
inside it) that was private becomes `pub(super)` so hub and siblings still see it.

Relies on rustfmt layout: a top-level item starts at column 0 and its closing
brace is a lone `}` at column 0 (or the item ends with `;` on its own last line).
Leading `///`, `//`, `#[...]` lines (multi-line attributes included) travel with the item.

What it cannot know, and clippy -D warnings will tell you: a child that uses nothing from
its parent has an unused `use super::*` (delete it), and a hub that uses nothing from a
child has an unused `use child::*` (delete it). Macro invocations (`thread_local!`) are
not items and stay behind - move them by hand. Run rustfmt on hub and child afterwards.

usage: extract_items.py <repo-root> <file.rs> <child> --items a,b,"impl X",... [--doc "..."] [--apply] [--list]
"""
import os
import re
import sys

ITEM = re.compile(
    r"^(?P<vis>pub(\([^)]*\))?\s+)?(?P<q>(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*)(?P<kind>fn|struct|enum|union|trait|type|const|static|impl|mod|macro_rules!)\b(?P<rest>.*)$"
)
IMPL = re.compile(r"^impl(<[^>]*>)?\s+(?:(?P<trait>[\w:<>, ]+?)\s+for\s+)?(?P<ty>[\w:]+)")
LEAD = re.compile(r"^(///|//|#\[|#!\[)")


def parse_items(lines):
    """Yield (start, end_exclusive, key) for every top-level item, start including leading
    doc/attr lines. `key` is 'fn name', 'struct name', 'impl Ty', 'impl Trait for Ty', ..."""
    items = []
    i = 0
    n = len(lines)
    while i < n:
        ln = lines[i]
        m = ITEM.match(ln)
        if not m or ln.startswith(" "):
            i += 1
            continue
        kind = m.group("kind")
        rest = m.group("rest").strip()
        if kind == "impl":
            im = IMPL.match(ln)
            if im is None:
                i += 1
                continue
            key = f"impl {im.group('trait') + ' for ' if im.group('trait') else ''}{im.group('ty')}"
        elif kind == "macro_rules!":
            key = "macro " + rest.split("{")[0].strip()
        else:
            name = re.match(r"[A-Za-z_][A-Za-z0-9_]*", rest)
            if not name:
                i += 1
                continue
            key = f"{kind} {name.group(0)}"
        # leading comments / attributes
        start = i
        while start > 0:
            prev = lines[start - 1]
            if prev.startswith("#!["):
                break
            if LEAD.match(prev):
                start -= 1
            elif prev in (")]", "]"):
                # the tail of a multi-line attribute: walk back to the `#[` line that opens it
                k = start - 1
                while k > 0 and not lines[k].startswith("#["):
                    k -= 1
                if not lines[k].startswith("#["):
                    break
                start = k
            else:
                break
        # end
        stripped = ln.rstrip()
        if stripped.endswith(";"):
            end = i + 1
        elif stripped.endswith("{") or stripped.endswith("("):
            # find the closing at column 0
            j = i + 1
            while j < n and not (lines[j] == "}" or re.match(r"^[\]\)}]+;?$", lines[j])):
                j += 1
            end = j + 1
        else:
            # multi-line signature: scan to the first line ending with `{` or `;` at depth 0
            j = i
            while j < n and not lines[j].rstrip().endswith(("{", ";")):
                j += 1
            if lines[j].rstrip().endswith(";"):
                end = j + 1
            else:
                k = j + 1
                while k < n and lines[k] != "}":
                    k += 1
                end = k + 1
        # swallow one trailing blank line
        items.append((start, end, key))
        i = end
    return items


INCLUDE = re.compile(r'(include_(?:bytes|str)!\(\s*")(?![/\\]|[A-Za-z]:)')


def deepen_includes(text_lines):
    """`include_bytes!("../x")` is relative to the FILE; one directory deeper needs one more `../`."""
    return [INCLUDE.sub(lambda m: m.group(1) + "../", ln) for ln in text_lines]


def make_pub_super(block):
    """Private -> `pub(super)`. An item that was ALREADY `pub(super)` was visible to the hub's
    parent; one level deeper that is `pub(in super::super)` (the hub re-exports it by name)."""
    out = []
    in_struct = False
    in_impl = False
    block = [
        re.sub(r"^(\s*)pub\(super\) ", r"\1pub(in super::super) ", ln) if re.match(r"^\s*pub\(super\) ", ln) else ln
        for ln in block
    ]
    for idx, ln in enumerate(block):
        if idx == 0 or not out or not ln.startswith(" "):
            m = ITEM.match(ln) if not ln.startswith(" ") else None
            if m and not m.group("vis") and m.group("kind") not in ("impl", "macro_rules!"):
                ln = "pub(super) " + ln
            if m:
                in_struct = m.group("kind") == "struct" and ln.rstrip().endswith("{")
                in_impl = m.group("kind") == "impl"
            out.append(ln)
            continue
        if in_struct and re.match(r"^    [a-z_][a-z0-9_]*: ", ln):
            ln = "    pub(super) " + ln[4:]
        elif in_impl and re.match(r"^    (const |unsafe )?fn ", ln):
            ln = "    pub(super) " + ln[4:]
        out.append(ln)
    return out


def main():
    root, rel, child = sys.argv[1], sys.argv[2], sys.argv[3]
    args = sys.argv[4:]
    apply = "--apply" in args
    doc = ""
    items = []
    if "--doc" in args:
        doc = args[args.index("--doc") + 1]
    if "--items" in args:
        items = [s.strip() for s in args[args.index("--items") + 1].split(",") if s.strip()]
    path = os.path.join(root, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        raw = fh.read()
    nl = "\r\n" if "\r\n" in raw else "\n"
    lines = raw.split(nl)
    parsed = parse_items(lines)
    if "--list" in args:
        for s, e, k in parsed:
            print(f"{s + 1:5d}-{e:5d} {e - s:4d}  {k}")
        return
    wanted = set(items)
    found = [(s, e, k) for s, e, k in parsed if k in wanted]
    missing = wanted - {k for _, _, k in found}
    if missing:
        print("MISSING:", ", ".join(sorted(missing)))
        sys.exit(1)
    moved = []
    keep = []
    cursor = 0
    total_moved = 0
    for s, e, k in found:
        keep.extend(lines[cursor:s])
        block = lines[s:e]
        # drop a single trailing blank that follows the item in the hub
        if e < len(lines) and lines[e] == "" and keep and keep[-1] == "":
            pass
        moved.extend(make_pub_super(block))
        moved.append("")
        total_moved += e - s
        cursor = e
        if cursor < len(lines) and lines[cursor] == "":
            cursor += 1  # the blank line after the item goes with it
    keep.extend(lines[cursor:])
    # insert the mod + use lines after the last existing `mod x;` / `use x::*;` block, else after the first blank line following the leading doc/use block
    insert_at = None
    for idx, ln in enumerate(keep):
        if idx > 0 and keep[idx - 1].startswith("#[cfg(test)]"):
            continue  # test-only module declarations sit at the tail; never insert there
        if re.match(r"^(pub(\([^)]*\))? )?mod \w+;$", ln) or re.match(r"^use \w+::\*;$", ln):
            insert_at = idx + 1
    if insert_at is None:
        for idx, ln in enumerate(keep):
            if ln.startswith("use ") or ln.startswith("pub use "):
                insert_at = idx + 1
            elif insert_at is not None and ln == "":
                break
        if insert_at is None:
            insert_at = 0
    # Items that were already `pub` / `pub(crate)` keep that visibility in the child, but a
    # private glob import does not re-export them: name them so `hub::item` still resolves.
    reexport = {}
    for s, e, k in found:
        kind, _, nm = k.partition(" ")
        if kind in ("impl", "macro"):
            continue
        for ln in lines[s:e]:
            m = ITEM.match(ln)
            if m and m.group("vis"):
                # `pub(super)` included: the child now says `pub(in super::super)`, and the
                # hub's parent still reaches the item through this re-export.
                reexport.setdefault(m.group("vis").strip(), []).append(nm)
                break
    decl = [f"mod {child};", f"use {child}::*;"]
    for vis, names in sorted(reexport.items()):
        decl.append(f"{vis} use {child}::{{{', '.join(names)}}};")
    keep = keep[:insert_at] + decl + keep[insert_at:]
    base, fname = os.path.split(path)
    stem = fname[:-3]
    beside = fname in ("mod.rs", "lib.rs", "main.rs")
    target = os.path.join(base, f"{child}.rs") if beside else os.path.join(base, stem, f"{child}.rs")
    if not beside:
        moved = deepen_includes(moved)
    header = ([f"//! {doc}", ""] if doc else []) + ["use super::*;", ""]
    body = nl.join(header + moved).rstrip(nl) + nl
    print(f"{'APPLY' if apply else 'PLAN '} {rel}: {len(found)} items, {total_moved} lines -> {os.path.relpath(target, root)}; hub {len(lines)} -> {len(keep)} lines")
    if apply:
        if os.path.exists(target):
            print("target exists:", target)
            sys.exit(1)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w", encoding="utf-8", newline="") as fh:
            fh.write(body)
        with open(path, "w", encoding="utf-8", newline="") as fh:
            fh.write(nl.join(keep))


if __name__ == "__main__":
    main()
