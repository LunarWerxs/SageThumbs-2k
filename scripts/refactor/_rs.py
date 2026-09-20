"""Shared pieces of the refactor instruments: file I/O that keeps a file's own line endings,
where a child module's file lives, and the text fix-ups a moved block needs."""
import io
import os
import re
import sys

ROOT_FILES = ("mod.rs", "lib.rs", "main.rs")
INCLUDE = re.compile(r'(include_(?:bytes|str)!\(\s*")(?![/\\]|[A-Za-z]:)')
TEST_ATTR = "#![cfg(test)]"


def read_lines(path):
    """(lines, newline) - split on the file's own line ending so a rewrite keeps it."""
    with io.open(path, encoding="utf-8", newline="") as fh:
        raw = fh.read()
    nl = "\r\n" if "\r\n" in raw else "\n"
    return raw.split(nl), nl


def write_lines(path, lines, nl):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with io.open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(nl.join(lines))


def is_named_crate_root(path):
    """A `tests/x.rs` or `src/bin/x.rs`: a crate root whose children would land BESIDE it and be
    taken by Cargo for crates of their own, so they need `#[path = "x/child.rs"]` instead."""
    parts = os.path.normpath(path).replace("\\", "/").split("/")
    return len(parts) >= 2 and (parts[-2] == "tests" or (len(parts) >= 3 and parts[-3:-1] == ["src", "bin"]))


def child_target(path, name, beside=False):
    """(path of child module `name`, whether it sits BESIDE the parent file).

    A `mod.rs` or a crate root owns its directory, so its children sit beside it; any other
    `foo.rs` keeps its children in `foo/` (a named crate root too, declared with `#[path]`)."""
    base, fname = os.path.split(path)
    beside = beside or fname in ROOT_FILES
    if beside:
        return os.path.join(base, name + ".rs"), True
    return os.path.join(base, fname[:-3], name + ".rs"), False


def deepen_includes(lines):
    """`include_bytes!("../x")` is relative to the FILE: one directory deeper, one more `../`."""
    return [INCLUDE.sub(lambda m: m.group(1) + "../", ln) for ln in lines]


SUPER = re.compile(r"(?<![\w:])super::(?!super\))")


def deepen_supers(lines):
    """An extracted item sits one module deeper: a `super::x` path in it needs one more `super::`
    (`pub(super)` and `pub(in super::super)` are visibilities, not paths, and stay)."""
    return [ln if ln.lstrip().startswith("//") else SUPER.sub("super::super::", ln) for ln in lines]


def dedent(lines):
    """Drop one 4-space level; blank lines stay blank, anything shallower is left alone."""
    out = []
    for ln in lines:
        if ln.startswith("    "):
            out.append(ln[4:])
        else:
            out.append("" if ln.strip() == "" else ln)
    return out


def closing_brace(lines, start):
    """Index of the first column-0 `}` after `start` (rustfmt indents every nested one)."""
    for idx in range(start + 1, len(lines)):
        if lines[idx] == "}":
            return idx
    return None


def as_test_file(body):
    """A test-only module says so inside the file too: the parent's `#[cfg(test)]` is invisible
    from there, and a per-file reader (the Architect, a reviewer) needs the tell."""
    return [TEST_ATTR, ""] + body


def moved_body(body, beside):
    return body if beside else deepen_includes(body)


def die(msg):
    print(msg)
    sys.exit(1)


def flag_value(args, flag, default=""):
    return args[args.index(flag) + 1] if flag in args else default
