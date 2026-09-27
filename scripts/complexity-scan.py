#!/usr/bin/env python3
r"""
complexity-scan.py - vendored per-function Rust complexity scanner (stdlib only).

WHY THIS EXISTS. check-complexity.ps1's reference engine is Odin's probe.py, which shells
out to the PRIVATE `Lunarwerx/odin` sibling checkout's Architect (a bun/JS tool). A GitHub
runner can never clone that private repo, so CI had NO way to run the complexity gate at
all (docs/DEVELOPMENT_GOTCHAS.md 2.2) - the backlog regrew from 0 to 19 findings in a week
with every visible gate green. This script is the tracked, dependency-free fallback:
check-complexity.ps1 prefers Odin's probe.py when it is present (that stays the reference
implementation - see its own header) and falls back to THIS script otherwise, which is
exactly the shape a GitHub runner is in.

SCOPE: RUST ONLY. Odin's Architect measures every `.rs/.py/.mjs/...` file in the repo
through its own per-language engine; porting all of those (a real TypeScript-AST walk for
JS, a separate Python lexer, ...) is out of this script's budget. This repo is a Rust shell
extension - the handful of `.py`/`.mjs` build-support scripts are not gated here. If one of
those ever crosses the gate, only Odin's own local hook would catch it; see the header of
check-complexity.ps1 for how that machine-local gap is covered.

CALIBRATION IS THE POINT, NOT THE SCANNER SHAPE. This is a faithful line-by-line port of
`connections-arkitect`'s LEGACY (non-AST) Rust complexity path - the one `.rs` files
actually go through in the reference engine, since Rust is not one of the extensions its
TypeScript-AST parser can read (agnostic/engines/code-quality/code-metrics-engine.mjs).
Every rule below cites the exact function it mirrors so a future drift in the reference can
be diffed rule-by-rule instead of re-derived from scratch:

  - Comments, string/char literals and raw strings are masked to blanks BEFORE anything
    else runs (rust-lexical-scan.mjs's `maskRustLiteralsAndComments` / `walkRustSpans`),
    length- and newline-preserving so every offset stays valid. A lifetime (`&'a str`,
    `Chars<'_>`) is NOT a char literal and is deliberately left as code - the reference
    only masks a lifetime's apostrophe to protect its OWN naive quote-aware brace matcher
    (code-metrics-engine.mjs's `maskRustLifetimes`); this port never treats `'` as a quote
    opener at all (see `_matching_brace_end`/`_matching_angle_end` below), so a bare
    lifetime apostrophe is already harmless and needs no separate masking pass.
  - Functions are found via the SAME single regex Rust gets routed through
    (`FUNC_PATTERNS`'s `fn-func` id, gated by `LEGACY_PATTERN_IDS_BY_LANGUAGE`), with the
    SAME header-parse quirk: a non-generic function's parameter list is closed at the
    FIRST `)`, not a balanced one (`parseHeaderTail`) - `fn f(cb: impl Fn(i32) -> i32) {`
    stops "params" at `Fn(i32)`'s own paren. This does not corrupt the body span in
    practice (the very next real `{` is still found by a plain forward scan) so it is kept
    exactly as reference, not "fixed" into a different-and-therefore-disagreeing scanner.
  - A name is excluded from tracking if it matches `RESERVED_NOT_FUNCTION` - a JavaScript
    keyword list, ported byte-for-byte. It is JS-shaped, not Rust-shaped, and that is the
    point of matching it exactly: `default`, `from`, `as`, `in`, `of`, `with` are ALL in
    it, so `fn default()` (every `impl Default`) and `fn from()` (every `impl From`) are
    genuinely invisible to the reference scanner too - reproducing that blind spot is
    required for agreement, not a bug to route around.
  - Nested `fn` items are NOT scoped out of their enclosing function's own complexity (the
    reference's AST path does this via a skip-set; the LEGACY path Rust uses never had
    that fix - see code-metrics-engine.mjs's own header, "kept verbatim as the *Legacy
    functions"). A named function nested inside another gets its own entry AND its
    branches still count toward the enclosing function's score.
  - Every `?` that is not part of `?.` counts as a decision point in BOTH metrics
    (`CC_TOKENS`'s shared `/\?\?|\?(?!\.)/` for cyclomatic; the lone-`?` branch of
    `legacyCognitiveComplexity` for cognitive, since Rust never sets Dart's
    `hasNullCoalescingOperator` gate) - this is the try-operator rule CLAUDE.md and
    docs/DEVELOPMENT_GOTCHAS.md call out by name. A `match` is scored per ARM for
    cyclomatic (McCabe: N arms = N-1 decision points, `languageExtraCyclomatic`'s
    `arms - matches` count) but only ONCE, nesting-weighted, for cognitive
    (`legacyCognitiveComplexity`'s `word === "match"` branch) - the two metrics disagree
    here on purpose, exactly as they do for a JS/TS `switch`.
  - `loop { }` is NOT scored as a decision point in either metric - neither CC_TOKENS nor
    the cognitive word list has ever known the word "loop". Not a fix opportunity here:
    the reference doesn't count it, so a scanner that does would disagree with the tool
    the pre-push hook actually uses.
  - `&&`/`||` "run" collapsing (one score per RUN of the same operator, not one per
    occurrence) is ported exactly as legacyCognitiveComplexity implements it, INCLUDING
    the fact that it never actually collapses anything in real source: `lastBoolean` is
    reset by every intervening non-operator character (an operand, a space), so `a && b
    && c` scores 2, not 1. This looks like a bug in the reference; it is still what the
    reference measures, so it is ported unchanged rather than "corrected" into disagreement.
  - `stripCommentsAndStrings`'s trailing `.replace(/#[^\n]*/g, ...)` - a Python-comment
    rule applied unconditionally to every language - blanks a Rust attribute
    (`#[derive(...)]`, `#[cfg(test)]`) from the `#` to end-of-line before EITHER metric is
    computed. Ported as `_hash_blank` below, applied per-function after extraction, same
    as the reference applies it per-function inside `legacyCyclomaticComplexity` /
    `legacyCognitiveComplexity`.
  - File discovery mirrors `bin/arkitect.mjs`'s `--corpus` (foreign-tree) scan rules: dot-
    prefixed directories AND files are pruned outright (`entry.name.startsWith(".")`,
    Bun.Glob's `dot: false`); on top of that, every DIRECTORY- or FILE-shaped, non-glob,
    non-negated `.gitignore` line that resolves to something that actually exists on disk
    is excluded too (`readIgnoreDirs`), literally (root-relative path match, NOT a
    recursive glob - `test/` excludes only `<root>/test`, never `src/test/`, "the safe
    direction for a gate" per the reference's own comment). `crates/vendor/**` is NOT
    gitignored, so vendored third-party Rust IS in scope - matching the reference exactly,
    however surprising that is on first read.

GATE: an ERROR at score >= 30 for EITHER metric (cyclomatic or cognitive); a WARN at >= 15.
Mirrors `code-metrics-report.mjs`'s `runFunctionMetricCheck` (`threshold ?? 15`,
`severity = score >= threshold * 2 ? "error" : "warning"`).

USAGE
    python scripts/complexity-scan.py --root <path> [--json] [--warnings]
    python scripts/complexity-scan.py --self-test

EXIT CODES
    0  scan completed (see stdout/JSON for findings) - or --self-test passed
    1  --self-test found a fixture that did NOT score as expected
    2  --root does not exist, or no .rs files could be read at all
"""
import argparse
import json
import os
import sys

from complexity_scan_files import scan_repo
from complexity_scan_metrics import GATE, WARN
from complexity_scan_selftest import _self_test


# ============================================================================
# Driver
# ============================================================================


def _print_text(rows, threshold, warn_only_note):
    over = [r for r in rows if r[0] >= threshold]
    over.sort(key=lambda r: -r[0])
    if not over:
        print(f"  none in this scope. {warn_only_note}")
        return
    functions = {(r[1], r[2], r[3]) for r in over}
    print(f"  {len(functions)} unique function(s) behind {len(over)} finding(s).")
    print()
    print(f"  {'SCORE':>6}  {'METRIC':<6} FUNCTION / LOCATION")
    for score, file_, line, fn, metric in over:
        print(f"  {score:>6}  {metric:<6} {fn}  {file_}:{line}")


def _run_scan(args):
    root = os.path.abspath(args.root)
    if not os.path.isdir(root):
        print(f"complexity-scan: --root {args.root!r} is not a directory", file=sys.stderr)
        return 2
    rows, files, unreadable = scan_repo(root)
    if not files:
        print("complexity-scan: no .rs files found - nothing was measured.", file=sys.stderr)
        return 2
    threshold = WARN if args.warnings else GATE
    kept = [r for r in rows if r[0] >= threshold]
    if args.json:
        print(json.dumps([{"score": s, "file": f, "line": ln, "function": fn, "metric": m} for s, f, ln, fn, m in kept], indent=1))
    else:
        print(f"complexity-scan: {len(files)} .rs file(s) scanned via the vendored fallback (root {root})")
        if unreadable:
            print(f"  {len(unreadable)} file(s) could not be read: {', '.join(unreadable[:10])}")
        note = f"(repo total {len(rows)} findings, gate {GATE}, warn {WARN})"
        _print_text(rows, threshold, note)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", default=".", help="repo root to scan (default: cwd)")
    parser.add_argument("--json", action="store_true", help="machine-readable [{score,file,line,function,metric}]")
    parser.add_argument("--warnings", action="store_true", help="also include the 15..29 warn band, not just >=30 errors")
    parser.add_argument("--self-test", action="store_true", help="prove the scanner against known fixtures and exit")
    args = parser.parse_args()

    if args.self_test:
        return _self_test()
    return _run_scan(args)


if __name__ == "__main__":
    sys.exit(main())
