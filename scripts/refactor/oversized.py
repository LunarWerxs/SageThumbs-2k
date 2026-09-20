"""List every non-vendor .rs file over 800 lines with its code/test split.

For each file: total lines, the line where the LAST top-level `#[cfg(test)]` block
starts (0 = no test block), and the code-only count. Files whose code-only count is
already under 800 can be brought under the line by moving the test block to a
sibling `tests.rs`; the rest need a real split.
"""
import os
import re
import sys

ROOT = sys.argv[1] if len(sys.argv) > 1 else "."
LIMIT = 800
CFG = re.compile(r"^#\[cfg\(test\)\]\s*$")
MOD = re.compile(r"^(pub(\(crate\))? )?mod \w+\s*\{")

rows = []
for base in ("src", "crates/dll", "tests"):
    for dp, dn, fn in os.walk(os.path.join(ROOT, base)):
        if "vendor" in dp.replace("\\", "/").split("/"):
            continue
        for f in fn:
            if not f.endswith(".rs"):
                continue
            p = os.path.join(dp, f)
            with open(p, encoding="utf-8", errors="replace") as fh:
                lines = fh.read().split("\n")
            total = len(lines) - (1 if lines and lines[-1] == "" else 0)
            if total <= LIMIT:
                continue
            test_start = 0
            for i, ln in enumerate(lines):
                if CFG.match(ln) and i + 1 < len(lines) and MOD.match(lines[i + 1]):
                    test_start = i + 1  # 1-based line of #[cfg(test)]
            code = test_start - 1 if test_start else total
            tests = total - code
            rows.append((total, code, tests, os.path.relpath(p, ROOT).replace("\\", "/")))

rows.sort(reverse=True)
print(f"{'total':>6} {'code':>6} {'tests':>6}  file")
easy = 0
for total, code, tests, path in rows:
    tag = ""
    if code <= LIMIT and tests <= LIMIT:
        tag = "  <- tests out"
        easy += 1
    elif code <= LIMIT:
        tag = "  <- tests out (tests file still >800)"
    print(f"{total:>6} {code:>6} {tests:>6}  {path}{tag}")
print(f"\n{len(rows)} files over {LIMIT}; {easy} fixed by moving tests to a sibling file")
