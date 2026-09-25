#!/usr/bin/env python3
"""Fold the hard line breaks out of a published GitHub release body, in place.

    python scripts/unwrap-release-notes.py --list                # what would change, nothing written
    python scripts/unwrap-release-notes.py v3.2.0 [v3.1.1 ...]   # rewrite those releases
    python scripts/unwrap-release-notes.py --all --apply         # rewrite every release that needs it

WHY. `docs/CHANGELOG.md` is hard-wrapped at ~95 columns so it reads in an editor, and
`Format-ReleaseNotesBody` used to carry those line breaks into the release body verbatim. GitHub
renders a release body with `breaks: true`, so every one of them becomes a real `<br>`: on a
desktop that only looks a bit odd, but on a phone the 95-column wrap lands inside an already
narrow column and each bullet comes out as a ragged staircase (owner report, 2026-09-20, reading
v3.2.0's notes on a phone). The exporter is fixed going forward; this is the one-off for the
releases already published.

This is a PURE FORMATTING pass and deliberately cannot change a single word: it only joins a
bullet's continuation lines onto the bullet, with one space. Fenced code blocks, HTML blocks,
headings, rules and blank lines are passed through untouched, and the result is verified to
contain every non-empty source line's text before anything is uploaded - a release body is
public, so "no content changed" has to be checked rather than assumed.
"""
import argparse
import json
import re
import subprocess
import sys

REPO = "LunarWerxs/SageThumbs-2k"
BULLET = re.compile(r"^\s*[-*+][ ]")
CONTINUATION = re.compile(r"^\s+\S")
FENCE = re.compile(r"^\s*```")


def unwrap(body):
    """Join each bullet's continuation lines onto the bullet. Everything else is passed through."""
    out, pending, fenced = [], None, False
    for line in body.replace("\r\n", "\n").split("\n"):
        if FENCE.match(line):
            if pending is not None:
                out.append(pending)
                pending = None
            fenced = not fenced
            out.append(line)
            continue
        if fenced:
            out.append(line)
            continue
        if pending is not None and CONTINUATION.match(line) and not BULLET.match(line):
            pending = pending + " " + line.strip()
            continue
        if pending is not None:
            out.append(pending)
            pending = None
        if BULLET.match(line):
            pending = line.rstrip()
            continue
        out.append(line)
    if pending is not None:
        out.append(pending)
    return "\n".join(out)


def every_line_survived(before, after):
    """Every non-empty source line's text must still be present. A release body is public."""
    missing = [l.strip() for l in before.split("\n") if l.strip() and l.strip() not in after]
    return missing


def gh(args):
    r = subprocess.run(["gh"] + args, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if r.returncode:
        raise SystemExit("gh " + " ".join(args) + " failed: " + (r.stderr or r.stdout)[-300:])
    return r.stdout


def tags(explicit, want_all):
    if explicit:
        return explicit
    rows = json.loads(gh(["release", "list", "--repo", REPO, "--limit", "100", "--json", "tagName"]))
    return [r["tagName"] for r in rows] if want_all else [rows[0]["tagName"]]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tags", nargs="*")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--list", action="store_true", help="report only, write nothing")
    ap.add_argument("--apply", action="store_true")
    a = ap.parse_args()
    apply = a.apply or (bool(a.tags) and not a.list)

    changed = 0
    for tag in tags(a.tags, a.all or a.list):
        # Compare against the NORMALISED body: `unwrap` emits "\n", and a CRLF release body with
        # nothing to fold would otherwise look changed and buy a pointless public edit.
        body = json.loads(gh(["release", "view", tag, "--repo", REPO, "--json", "body"]))["body"].replace("\r\n", "\n")
        new = unwrap(body)
        if new == body:
            print(f"  ok     {tag} - already one line per bullet")
            continue
        missing = every_line_survived(body, new)
        if missing:
            print(f"  SKIP   {tag} - {len(missing)} line(s) would not survive: {missing[0][:60]!r}")
            continue
        changed += 1
        before_lines, after_lines = body.count("\n") + 1, new.count("\n") + 1
        if not apply:
            print(f"  would  {tag} - {before_lines} lines -> {after_lines}")
            continue
        subprocess.run(
            ["gh", "release", "edit", tag, "--repo", REPO, "--notes-file", "-"],
            input=new, text=True, encoding="utf-8", check=True, capture_output=True,
        )
        print(f"  EDITED {tag} - {before_lines} lines -> {after_lines}")
    print(f"{changed} release(s) {'rewritten' if apply else 'would change'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
