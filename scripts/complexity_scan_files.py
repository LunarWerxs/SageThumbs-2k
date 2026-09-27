"""Which `.rs` files complexity-scan.py measures, and every finding in them (see its header)."""

import os
import re

from complexity_scan_extract import extract_functions
from complexity_scan_mask import mask_rust_literals_and_comments
from complexity_scan_metrics import _hash_blank, cognitive_complexity, cyclomatic_complexity


# ============================================================================
# File discovery - port of bin/arkitect.mjs's `--corpus` (foreign-tree) scan-exclusion
# rules: dot-prefixed entries pruned outright, plus every DIRECTORY- or FILE-shaped,
# non-glob, non-negated `.gitignore` line that resolves to something real on disk.
# ============================================================================

# `bin/arkitect.mjs` never has to name `crates/vendor` explicitly: `agnostic/src/core/
# files.mjs`'s walkFiles excludes any path with a directory SEGMENT named "vendor" (or
# "target"/"dist"/"tmp"/... - GENERIC_GENERATED_DIR_NAMES) REGARDLESS of .gitignore, root-
# scoped (every segment below the scan root, which for a `--corpus` foreign-tree run is
# always the repo root itself). Measured live: without this, the vendored `djvu-rs`/`exr`/
# `jxl-oxide` trees under crates/vendor/ produced 88 phantom errors this scanner does not
# actually have, because odin's real scan never sees that tree at all - confirmed via
# `python probe.py sagethumbs-2k --json`, zero rows with `crates/vendor` in the path.
_GENERATED_DIR_NAMES = frozenset(
    {
        ".git", ".next", ".nuxt", ".turbo", ".codegraph", ".stryker-tmp", "node_modules", "cdk.out",
        "coverage", "tmp", "vendor", "target", "dist", "build", "out",
    }
)
_GENERATED_DIR_PREFIXES = ("cdk.out.", "cdk.out-", "dist-")


def _is_generated_dir_name(name):
    return name in _GENERATED_DIR_NAMES or any(name.startswith(p) for p in _GENERATED_DIR_PREFIXES)


_GLOB_CHARS = re.compile(r"[*?\[\]]")


def _read_ignore_dirs(root, gitignore_path):
    """Port of bin/arkitect.mjs's readIgnoreDirs (globsAllowed=False, the `.gitignore`
    call shape) - literal root-relative path excludes, never a recursive glob match."""
    try:
        with open(gitignore_path, encoding="utf-8", errors="replace") as fh:
            text = fh.read()
    except OSError:
        return []
    out = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or line.startswith("!"):
            continue
        if _GLOB_CHARS.search(line):
            continue
        rel = line.strip("/")
        if not rel or rel == "." or rel.startswith(".."):
            continue
        rel_native = rel.replace("/", os.sep)
        if os.path.exists(os.path.join(root, rel_native)):
            out.append(rel.replace("\\", "/"))
    return out


def _is_excluded(rel_posix, excluded_prefixes):
    for prefix in excluded_prefixes:
        if rel_posix == prefix or rel_posix.startswith(prefix + "/"):
            return True
    return False


def find_rust_files(root):
    """Every `.rs` file under `root`, dot-pruned and `.gitignore`-excluded exactly as
    `bin/arkitect.mjs` scans a foreign tree via `--corpus` (see this file's header)."""
    excluded = _read_ignore_dirs(root, os.path.join(root, ".gitignore"))
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".") and not _is_generated_dir_name(d)]
        rel_dir = os.path.relpath(dirpath, root)
        rel_dir_posix = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
        if rel_dir_posix and _is_excluded(rel_dir_posix, excluded):
            dirnames[:] = []
            continue
        for name in filenames:
            if name.startswith(".") or not name.endswith(".rs"):
                continue
            rel_posix = name if not rel_dir_posix else f"{rel_dir_posix}/{name}"
            if _is_excluded(rel_posix, excluded):
                continue
            files.append(rel_posix)
    files.sort()
    return files


def scan_file(root, rel_path):
    """Every (score, function, line, metric) finding for one file, worst first within the
    file. `metric` is "cog" or "cyc", matching odin's bands.py convention."""
    abs_path = os.path.join(root, rel_path.replace("/", os.sep))
    try:
        with open(abs_path, encoding="utf-8", errors="replace") as fh:
            source = fh.read()
    except OSError:
        return None
    masked = mask_rust_literals_and_comments(source)
    rows = []
    for fn in extract_functions(masked):
        clean = _hash_blank(fn["body"])
        cyc = cyclomatic_complexity(clean)
        cog = cognitive_complexity(clean)
        rows.append((cog, rel_path, fn["start_line"], fn["name"], "cog"))
        rows.append((cyc, rel_path, fn["start_line"], fn["name"], "cyc"))
    return rows


def scan_repo(root):
    files = find_rust_files(root)
    all_rows = []
    unreadable = []
    for rel_path in files:
        rows = scan_file(root, rel_path)
        if rows is None:
            unreadable.append(rel_path)
            continue
        all_rows.extend(rows)
    return all_rows, files, unreadable
