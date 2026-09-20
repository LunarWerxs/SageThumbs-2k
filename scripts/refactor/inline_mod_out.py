"""Turn an inline `mod NAME { ... }` block (any visibility, anywhere in the file) into a
file module: the body goes to `<stem>/NAME.rs` (or `NAME.rs` beside a mod.rs / crate
root), the declaration line becomes `mod NAME;` with its visibility, and the doc
comments / attributes above it stay where they are.

usage: inline_mod_out.py <repo-root> <file.rs> NAME [--apply] [--root-file]
  --root-file: the file is a crate root (lib.rs/main.rs/bin root), so children sit beside it
"""
import os
import re
import sys

MOD = re.compile(r"^(?P<vis>pub(\([^)]*\))?\s+)?mod (?P<name>\w+)\s*\{\s*$")


def main():
    root, rel, name = sys.argv[1], sys.argv[2], sys.argv[3]
    apply = "--apply" in sys.argv
    root_file = "--root-file" in sys.argv
    path = os.path.join(root, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        raw = fh.read()
    nl = "\r\n" if "\r\n" in raw else "\n"
    lines = raw.split(nl)
    start = None
    for i, ln in enumerate(lines):
        m = MOD.match(ln)
        if m and m.group("name") == name:
            start = i
            vis = m.group("vis") or ""
            break
    if start is None:
        print(f"no inline `mod {name} {{` in {rel}")
        sys.exit(1)
    end = start + 1
    while end < len(lines) and lines[end] != "}":
        end += 1
    if end >= len(lines):
        print("no column-0 closing brace")
        sys.exit(1)
    body = []
    for ln in lines[start + 1:end]:
        if ln.startswith("    "):
            body.append(ln[4:])
        elif ln.strip() == "":
            body.append("")
        else:
            body.append(ln)
    base, fname = os.path.split(path)
    if fname in ("mod.rs", "lib.rs", "main.rs") or root_file:
        target = os.path.join(base, f"{name}.rs")
    else:
        target = os.path.join(base, fname[:-3], f"{name}.rs")
        # `include_bytes!("../x")` is relative to the FILE: one directory deeper, one more `../`
        inc = re.compile(r'(include_(?:bytes|str)!\(\s*")(?![/\\]|[A-Za-z]:)')
        body = [inc.sub(lambda m: m.group(1) + "../", ln) for ln in body]
    new_lines = lines[:start] + [f"{vis}mod {name};"] + lines[end + 1:]
    print(f"{'APPLY' if apply else 'PLAN '} {rel}: mod {name} lines {start + 1}-{end + 1} ({len(body)} body lines) -> {os.path.relpath(target, root)}; file {len(lines)} -> {len(new_lines)}")
    if apply:
        if os.path.exists(target):
            print("target exists:", target)
            sys.exit(1)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w", encoding="utf-8", newline="") as fh:
            fh.write(nl.join(body).rstrip(nl) + nl)
        with open(path, "w", encoding="utf-8", newline="") as fh:
            fh.write(nl.join(new_lines))


if __name__ == "__main__":
    main()
