"""Move a trailing `#[cfg(test)] mod tests { ... }` block out of an oversized .rs file
into a sibling `<stem>/tests.rs` (or `tests.rs` beside a mod.rs), leaving
`#[cfg(test)]\nmod tests;` behind. Semantics are identical: the module path stays
`parent::tests`, so `use super::*` and `super::super::x` resolve exactly as before.

Refuses a file when the block is not the LAST item in the file, when the closing
brace is not at column 0, or when the target file already exists.

usage: split_tests.py <repo-root> [--apply] file [file ...]
"""
import os
import re
import sys

CFG = re.compile(r"^#\[cfg\(test\)\]\s*$")
MOD = re.compile(r"^(pub(\(crate\))? )?mod (\w+)\s*\{\s*$")


TRAIL = re.compile(r"^(#\[cfg\(test\)\]|#\[path = \"[^\"]+\"\]|(pub(\(crate\))? )?mod \w+;|)\s*$")


def plan(root, rel):
    path = os.path.join(root, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        raw = fh.read()
    nl = "\r\n" if "\r\n" in raw else "\n"
    lines = raw.split(nl)
    start = None
    for i in range(len(lines) - 1):
        if CFG.match(lines[i]) and MOD.match(lines[i + 1]):
            start = i
    if start is None:
        return None, "no trailing #[cfg(test)] mod block"
    name = MOD.match(lines[start + 1]).group(3)
    # closing brace: the first column-0 `}` after the mod line (rustfmt indents nested ones)
    end = start + 2
    while end < len(lines) and lines[end] != "}":
        end += 1
    if end >= len(lines):
        return None, "no column-0 '}' closes the block"
    # everything after it must be blank or an already-extracted test mod declaration
    trailing = lines[end + 1:]
    for t in trailing:
        if not TRAIL.match(t):
            return None, f"non-trivial line after the block: {t!r}"
    body = lines[start + 2:end]
    # dedent by exactly one level (4 spaces) where present; keep blank lines blank
    out = []
    for ln in body:
        if ln.startswith("    "):
            out.append(ln[4:])
        elif ln.strip() == "":
            out.append("")
        else:
            out.append(ln)
    base, fname = os.path.split(path)
    stem = fname[:-3]
    if fname == "mod.rs":
        target = os.path.join(base, f"{name}.rs")
    else:
        target = os.path.join(base, stem, f"{name}.rs")
        # `include_bytes!("../x")` is relative to the FILE: one directory deeper, one more `../`
        inc = re.compile(r'(include_(?:bytes|str)!\(\s*")(?![/\\]|[A-Za-z]:)')
        out = [inc.sub(lambda m: m.group(1) + "../", ln) for ln in out]
    if os.path.exists(target):
        return None, f"target exists: {target}"
    vis = MOD.match(lines[start + 1]).group(1) or ""
    tail = [t for t in trailing if t.strip() != ""]
    head = lines[:start] + [f"#[cfg(test)]", f"{vis}mod {name};"] + tail + [""]
    return (path, target, nl.join(head), nl.join(out) + nl, len(lines[:start]), len(out), nl), None


def main():
    root = sys.argv[1]
    apply = "--apply" in sys.argv
    files = [a for a in sys.argv[2:] if a != "--apply"]
    for rel in files:
        p, err = plan(root, rel)
        if err:
            print(f"SKIP  {rel}: {err}")
            continue
        path, target, head, body, n_head, n_body, nl = p
        print(f"{'APPLY' if apply else 'PLAN '} {rel}: {n_head} code lines stay, {n_body} test lines -> {os.path.relpath(target, root)}")
        if apply:
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w", encoding="utf-8", newline="") as fh:
                fh.write(body)
            with open(path, "w", encoding="utf-8", newline="") as fh:
                fh.write(head + nl)


if __name__ == "__main__":
    main()
