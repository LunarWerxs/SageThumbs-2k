"""After a parent-hub split, settle the hub's `use child::...` lines from a clippy log.

`extract_items.py` cannot know which moved names the hub, its siblings or only its TESTS still
use, so it writes `use child::*;` plus a by-name re-export of everything that was visible.
Clippy (`--message-format short`, `--all-targets`) then names the ones nobody uses. For each
flagged `use <child>::...` statement whose `<child>` is a `mod <child>;` of the same file:

  * a glob, or a list whose every name is flagged  -> the statement gets `#[cfg(test)]`
    (the usual case: only the test module reaches the moved items through `use super::*`);
    if it already carries `#[cfg(test)]`, nothing uses it at all and it is deleted
  * a list with some names flagged -> those names move to a `#[cfg(test)]` copy of the line

Run clippy again afterwards: a name that the test build does not use either is flagged a
second time, and the second pass deletes it.

usage: fix_unused_imports.py <repo-root> <clippy.log> [--apply]
"""
import os
import re
import sys
from collections import defaultdict

import _rs

MSG = re.compile(r"^(?P<path>[^\s:][^:]*\.rs):(?P<line>\d+):\d+: (?:error|warning): unused imports?: (?P<names>.+)$")
USE = re.compile(r"^(?P<vis>pub(?:\([^)]*\))? )?use (?P<child>\w+)::(?P<rest>.+);$", re.S)


def flagged(log):
    """{path: {line: {names}}} from the log's unused-import messages."""
    out = defaultdict(lambda: defaultdict(set))
    for ln in open(log, encoding="utf-8", errors="replace"):
        m = MSG.match(ln.strip())
        if m:
            path = m.group("path").replace("\\", "/")
            out[path][int(m.group("line"))].update(re.findall(r"`([^`]+)`", m.group("names")))
    return out


def statement(lines, at):
    """(start, end exclusive) of the `use` statement that covers 0-based line `at`."""
    s = at
    while s > 0 and not re.match(r"^(pub(\([^)]*\))? )?use ", lines[s]):
        s -= 1
    e = s
    while not lines[e].rstrip().endswith(";"):
        e += 1
    return s, e + 1


def settle(lines, s, e, names, children):
    """The replacement lines for one flagged statement, or None to leave it alone."""
    m = USE.match(" ".join(x.strip() for x in lines[s:e]))
    if not m or m.group("child") not in children:
        return None
    vis, child, rest = m.group("vis") or "", m.group("child"), m.group("rest").strip()
    is_test = s > 0 and lines[s - 1].strip() == "#[cfg(test)]"
    listed = [rest] if not rest.startswith("{") else [n.strip() for n in rest.strip("{}").split(",") if n.strip()]
    gone = [n for n in listed if n in names or f"{child}::{n}" in names]
    if not gone:
        return None
    keep = [n for n in listed if n not in gone]

    def line(ns):
        return f"{vis}use {child}::{ns[0] if len(ns) == 1 else '{' + ', '.join(ns) + '}'};"

    out = [line(keep)] if keep else []
    if not is_test:
        out += ["#[cfg(test)]", line(gone)]
    return out


def main():
    root, log = sys.argv[1], sys.argv[2]
    for rel, by_line in sorted(flagged(log).items()):
        path = os.path.join(root, rel)
        lines, nl = _rs.read_lines(path)
        children = {m.group(1) for ln in lines for m in [re.match(r"^(?:pub(?:\([^)]*\))? )?mod (\w+);", ln)] if m}
        spans = {}
        for at, names in by_line.items():
            s, e = statement(lines, at - 1)
            spans.setdefault((s, e), set()).update(names)
        changed = 0
        for (s, e), names in sorted(spans.items(), reverse=True):
            new = settle(lines, s, e, names, children)
            if new is None:
                continue
            drop_cfg = not new and s > 0 and lines[s - 1].strip() == "#[cfg(test)]"
            lines[s - 1 if drop_cfg else s:e] = new
            changed += 1
        if changed:
            print(("FIX   " if "--apply" in sys.argv else "WOULD ") + f"{rel}: {changed} statement(s)")
            if "--apply" in sys.argv:
                _rs.write_lines(path, lines, nl)


if __name__ == "__main__":
    main()
