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
import re
import sys

GATE = 30
WARN = 15

# ============================================================================
# Rust lexical masking - port of agnostic/lib/rust-lexical-scan.mjs's walkRustSpans +
# maskRustLiteralsAndComments. Comments and string/char literals blank to spaces
# (delimiters included), newlines kept, every other offset preserved.
# ============================================================================


def _blank(text):
    return "".join(ch if ch == "\n" else " " for ch in text)


def _is_escaped(source, i):
    """An ODD run of backslashes immediately before `i` means `source[i]` is escaped.
    Port of lexical-escape.mjs's isEscaped."""
    backslashes = 0
    k = i - 1
    while k >= 0 and source[k] == "\\":
        backslashes += 1
        k -= 1
    return backslashes % 2 == 1


def _char_literal_length(source, i):
    """Length of a char literal starting at apostrophe `i`, or 0 when this apostrophe is a
    LIFETIME (`'a`, `'_`, `'static`) rather than a char literal. Port of
    rust-lexical-scan.mjs's charLiteralLength."""
    n = len(source)
    nxt = source[i + 1] if i + 1 < n else None
    if nxt == "\\":
        if source[i + 2 : i + 3] == "u" and source[i + 3 : i + 4] == "{":
            close = source.find("}", i + 4)
            if close != -1 and close - i <= 12 and source[close + 1 : close + 2] == "'":
                return close + 2 - i
            return 0
        if source[i + 3 : i + 4] == "'":
            return 4
        return 0
    if nxt is not None and nxt != "'" and source[i + 2 : i + 3] == "'":
        return 3
    return 0


def _raw_string_open(source, at):
    """Raw-string opener at `at`: r"…" r#"…"# r##"…"## - `(hashes, quote_index)` or None.
    Port of rust-lexical-scan.mjs's rawStringOpen."""
    n = len(source)
    if at >= n or source[at] not in ("r", "R"):
        return None
    j = at + 1
    while j < n and source[j] == "#":
        j += 1
    if j >= n or source[j] != '"':
        return None
    return (j - (at + 1), j)


def _rule_line_comment(source, i, ch, nxt):
    """`//` line comment - runs to the next newline, or EOF. One rule of the walk below,
    same decomposition as the reference's own rustSlashSlashComment - see
    mask_rust_literals_and_comments's docstring for why this is split into rules at all."""
    if not (ch == "/" and nxt == "/"):
        return None
    nl = source.find("\n", i)
    return len(source) if nl == -1 else nl


def _rule_block_comment(source, i, ch, nxt):
    """`/* … */` block comment, NESTING-aware (rust nests them; a naive first-closer scan
    leaks the outer comment's tail into the token stream as code). Mirrors
    rust-lexical-scan.mjs's rustBlockComment / scanNestedBlockComment."""
    if not (ch == "/" and nxt == "*"):
        return None
    n = len(source)
    depth = 1
    j = i + 2
    while j < n and depth > 0:
        if source[j] == "/" and source[j + 1 : j + 2] == "*":
            depth += 1
            j += 2
        elif source[j] == "*" and source[j + 1 : j + 2] == "/":
            depth -= 1
            j += 2
        else:
            j += 1
    return j


def _rule_raw_string(source, i, ch, _nxt):
    """Raw string, optionally byte-prefixed: r"…" / r#"…"# / b"…"/br#"…"#. A prefix letter
    glued to a preceding identifier char is that identifier's tail, not a string opener.
    Mirrors rust-lexical-scan.mjs's rustRawString."""
    raw_at = i + 1 if ch in ("b", "B") else i
    raw = _raw_string_open(source, raw_at)
    if raw is None:
        return None
    prev = source[i - 1] if i > 0 else None
    if prev is not None and (prev.isalnum() or prev == "_"):
        return None
    hashes, quote_index = raw
    closer = '"' + ("#" * hashes)
    found = source.find(closer, quote_index + 1)
    return len(source) if found == -1 else found + len(closer)


def _rule_quoted_string(source, i, ch, nxt):
    """`"…"` string / `b"…"` byte string, backslash-escaped. Mirrors rust-lexical-scan.mjs's
    rustQuotedString."""
    is_plain = ch == '"'
    is_byte = ch == "b" and nxt == '"' and not (i > 0 and (source[i - 1].isalnum() or source[i - 1] == "_"))
    if not (is_plain or is_byte):
        return None
    n = len(source)
    j = (i if is_plain else i + 1) + 1
    while j < n:
        if source[j] == '"' and not _is_escaped(source, j):
            return j + 1
        j += 1
    return n


def _rule_char_literal(source, i, ch, _nxt):
    """`'x'` / `'\\n'` / `'\\u{…}'` char literal vs `'a` lifetime. Mirrors
    rust-lexical-scan.mjs's rustCharLiteral - a lifetime apostrophe declines (returns None)
    and is left as plain code, same as the reference."""
    if ch != "'":
        return None
    length = _char_literal_length(source, i)
    return i + length if length else None


# Tried in order at every position - same rule-table shape as rust-lexical-scan.mjs's
# walkRustSpans (line comment, nested block comment, raw string, quoted string, char
# literal). Each rule returns the span's end index when it recognizes and consumes
# something starting at `i`, or None to let the next rule (or the walker's own `i += 1`
# fallthrough) try. Splitting the walk into these small, independently-testable rules -
# rather than one large branching loop - is also what keeps `mask_rust_literals_and_comments`
# itself comfortably under this repo's own complexity gate.
_MASK_RULES = (_rule_line_comment, _rule_block_comment, _rule_raw_string, _rule_quoted_string, _rule_char_literal)


def mask_rust_literals_and_comments(source):
    """Comments AND string/char literals blanked (delimiters included); code passed through
    verbatim. Length- and newline-preserving. Port of rust-lexical-scan.mjs's
    maskRustLiteralsAndComments, driven by the same `_MASK_RULES` order its own
    walkRustSpans uses."""
    n = len(source)
    out = []
    code_start = 0
    i = 0
    while i < n:
        ch = source[i]
        nxt = source[i + 1] if i + 1 < n else ""
        end = None
        for rule in _MASK_RULES:
            end = rule(source, i, ch, nxt)
            if end is not None:
                break
        if end is None:
            i += 1
            continue
        out.append(source[code_start:i])
        out.append(_blank(source[i:end]))
        code_start = i = end

    out.append(source[code_start:n])
    return "".join(out)


# ============================================================================
# Function extraction - port of code-metrics-engine.mjs's extractFunctionsLegacy, gated to
# Rust's ONE pattern (LEGACY_PATTERN_IDS_BY_LANGUAGE['rust'] = {'fn-func'}) plus
# parseHeaderTail(tail='none').
# ============================================================================

# JS keyword list, ported byte-for-byte from code-metrics-engine.mjs's
# RESERVED_NOT_FUNCTION - see this file's own header for why `default`/`from`/`as`/`in`/
# `of`/`with` being in a JS list, and therefore excluding real Rust trait-impl functions,
# is a REQUIRED blind spot to reproduce, not a bug.
RESERVED_NOT_FUNCTION = frozenset(
    {
        "if", "else", "for", "while", "switch", "case", "catch", "do", "return", "typeof",
        "new", "delete", "throw", "void", "await", "yield", "in", "of", "instanceof",
        "with", "import", "export", "default", "from", "as", "function", "class", "const",
        "let", "var", "this", "super",
    }
)

# `(?:fn|func)` matches the shared FUNC_PATTERNS id; Rust only ever spells this "fn", but
# the pattern is ported whole rather than narrowed, since narrowing it is itself a
# behavior change relative to the reference.
_FN_PATTERN = re.compile(r"(?:^|\s)(?:fn|func)\s+(?:\([^)]*\)\s+)?([A-Za-z_][A-Za-z0-9_]*)(?=\s*[<(])")

_MAX_TYPE_PARAM_CLAUSE_CHARS = 500
_MAX_HEADER_TO_BRACE_GAP = 200


def _skip_ws(text, i):
    n = len(text)
    while i < n and text[i] in " \t\r\n\f\v":
        i += 1
    return i


def _matching_angle_end(text, open_index):
    """Balanced walk over a `<...>` type-parameter clause. Port of
    code-metrics-engine.mjs's matchingAngleEnd, MINUS its quote-awareness: no real quote
    character survives past `mask_rust_literals_and_comments` except a bare lifetime
    apostrophe, and a plain bracket-depth counter is already immune to that (it simply
    never looks at `'` at all), so quote-stepping would be dead code here."""
    n = len(text)
    depth = 0
    bracket_depth = 0
    limit = min(n, open_index + _MAX_TYPE_PARAM_CLAUSE_CHARS)
    i = open_index
    while i < limit:
        ch = text[i]
        if ch in ("{", "(", "["):
            bracket_depth += 1
        elif ch in ("}", ")", "]"):
            bracket_depth -= 1
        elif ch == ";" and bracket_depth == 0:
            return -1
        elif ch == "<":
            depth += 1
        elif ch == ">":
            if i > 0 and text[i - 1] == "=":  # the `=>` of a function type, not a closer
                i += 1
                continue
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _matching_balanced(text, open_index, open_ch, close_ch):
    """Naive balanced depth counter - no quote/comment awareness needed, since the whole
    file has already been through `mask_rust_literals_and_comments` before this ever
    runs (mirrors matchingDelimiterEnd's CONTRACT, not its quote-stepping machinery,
    which is unreachable once no real quote/comment characters remain)."""
    n = len(text)
    depth = 0
    i = open_index
    while i < n:
        ch = text[i]
        if ch == open_ch:
            depth += 1
        elif ch == close_ch:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _parse_header_tail(text, from_idx):
    """Header end index just past the parameter list's `)`, or None. Port of
    code-metrics-engine.mjs's parseHeaderTail(tail='none') - INCLUDING its non-generic
    naive-first-`)` quirk (see this file's header for why that is kept, not fixed)."""
    i = _skip_ws(text, from_idx)
    n = len(text)
    generic = False
    if i < n and text[i] == "<":
        close = _matching_angle_end(text, i)
        if close < 0:
            return None
        generic = True
        i = _skip_ws(text, close + 1)
    if i >= n or text[i] != "(":
        return None
    params_start = i + 1
    if generic:
        params_end = _matching_balanced(text, i, "(", ")")
    else:
        params_end = text.find(")", params_start)
    if params_end < 0:
        return None
    return params_end + 1


def extract_functions(masked_text):
    """Every trackable `fn` in `masked_text` (already comment/string-masked), each with its
    own body span - INCLUDING a function nested inside another (see this file's header:
    the legacy path never scopes nested functions out, so both get their own entry AND the
    outer's score still includes the inner's branches)."""
    functions = []
    pos = 0
    n = len(masked_text)
    while True:
        m = _FN_PATTERN.search(masked_text, pos)
        if not m:
            break
        name = m.group(1)
        if not name or name in RESERVED_NOT_FUNCTION:
            pos = m.end()
            continue
        header_end = _parse_header_tail(masked_text, m.end())
        if header_end is None:
            pos = m.end()
            continue
        open_brace = masked_text.find("{", header_end)
        if open_brace < 0 or open_brace - header_end > _MAX_HEADER_TO_BRACE_GAP:
            pos = m.end()
            continue
        close = _matching_balanced(masked_text, open_brace, "{", "}")
        if close < 0:
            pos = m.end()
            continue
        body_start, body_end = open_brace, close + 1
        # `m.start()` sits on the ONE separator char `(?:^|\s)` may have consumed (or at
        # true start-of-file when it matched `^`) - skip it, matching the reference's own
        # `headerStart` extraction (code-metrics-engine.mjs's extractFunctionsLegacy).
        header_start = 0 if m.start() == 0 else m.start() + 1
        functions.append(
            {
                "name": name,
                "start_line": masked_text.count("\n", 0, header_start) + 1,
                "end_line": masked_text.count("\n", 0, body_end) + 1,
                "body": masked_text[body_start:body_end],
            }
        )
        pos = header_end
    functions.sort(key=lambda f: f["start_line"])
    return functions


# ============================================================================
# Per-function complexity - port of code-metrics-engine.mjs's legacyCyclomaticComplexity /
# legacyCognitiveComplexity / languageExtraCyclomatic, Rust branches only.
# ============================================================================

# CC_TOKENS, ported verbatim (order is irrelevant: each pattern's count is independent and
# summed - these are NOT a single interacting tokenizer pass).
_CC_IF = re.compile(r"\belse\s+if\b|\bif\b")
_CC_FOR = re.compile(r"\bfor\b")
_CC_WHILE = re.compile(r"\bwhile\b")
_CC_CASE = re.compile(r"\bcase\b")
_CC_CATCH = re.compile(r"\bcatch\b")
_CC_AND = re.compile(r"&&")
_CC_OR = re.compile(r"\|\|")
_CC_TERNARY = re.compile(r"\?\?|\?(?!\.)")
_ARROW = re.compile(r"=>")
_MATCH_KEYWORD = re.compile(r"\bmatch\b")

_COGNITIVE_KEYWORDS_NESTED = ("if", "for", "while", "case", "catch")


def _hash_blank(body):
    """Blank from any `#` to end-of-line - port of stripCommentsAndStrings's trailing
    `.replace(/#[^\\n]*/g, ...)`, applied unconditionally regardless of language. On
    already-masked Rust text the only surviving `#` is a real attribute
    (`#[derive(...)]`, `#![no_std]`), so this blanks attribute lines before either metric
    is computed - reproduced exactly, not "fixed", per this file's header."""
    lines = body.split("\n")
    out = []
    for line in lines:
        idx = line.find("#")
        if idx == -1:
            out.append(line)
        else:
            out.append(line[:idx] + (" " * (len(line) - idx)))
    return "\n".join(out)


def cyclomatic_complexity(clean):
    """McCabe CC of an already hash-blanked, Rust-masked function body. Port of
    legacyCyclomaticComplexity + languageExtraCyclomatic's Rust branch."""
    cc = 1
    for pattern in (_CC_IF, _CC_FOR, _CC_WHILE, _CC_CASE, _CC_CATCH, _CC_AND, _CC_OR, _CC_TERNARY):
        cc += len(pattern.findall(clean))
    arms = len(_ARROW.findall(clean))
    matches = len(_MATCH_KEYWORD.findall(clean))
    cc += max(0, arms - matches)
    return cc


def cognitive_complexity(clean):
    """SonarSource-style cognitive complexity of an already hash-blanked, Rust-masked
    function body. Port of legacyCognitiveComplexity's Rust branches (word == "match"
    scores like an if/for/while; `hasNullCoalescingOperator` is Dart-only and never true
    here, so every bare `?` - including Rust's try operator - scores via the lone-`?`
    branch, and `&&`/`||` "run" collapsing is ported with its real-world no-op behavior
    intact - see this file's header)."""
    score = 0
    depth = 0
    i = 0
    n = len(clean)
    last_boolean = None
    while i < n:
        ch = clean[i]
        nxt = clean[i + 1] if i + 1 < n else ""
        if ch == "{":
            depth += 1
            i += 1
            continue
        if ch == "}":
            if depth > 0:
                depth -= 1
            i += 1
            continue
        if (ch == "&" and nxt == "&") or (ch == "|" and nxt == "|"):
            op = ch + nxt
            if op != last_boolean:
                score += 1
                last_boolean = op
            i += 2
            continue
        last_boolean = None
        if ch == "?" and nxt != ".":
            score += 1 + max(0, depth - 1)
            i += 1
            continue
        if ("A" <= ch <= "Z") or ("a" <= ch <= "z") or ch == "_":
            j = i
            while j < n and (("A" <= clean[j] <= "Z") or ("a" <= clean[j] <= "z") or ("0" <= clean[j] <= "9") or clean[j] == "_"):
                j += 1
            word = clean[i:j]
            if word in _COGNITIVE_KEYWORDS_NESTED:
                score += 1 + max(0, depth - 1)
            elif word == "else":
                score += 1
            elif word == "match":
                score += 1 + max(0, depth - 1)
            i = j
            continue
        i += 1
    return score


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


# ============================================================================
# Driver
# ============================================================================


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


# ============================================================================
# --self-test: prove the scanner against two fixtures that MUST score on opposite sides of
# the gate - same convention as check-render-sanity.ps1 / check-prebuild-coverage.ps1's
# -ProveItFails, so a scanner that silently stopped parsing (masking broke, extraction
# found zero functions, ...) is caught rather than read as "clean".
# ============================================================================

_TRIVIAL_FN = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"

# Byte-identical to check-complexity.ps1's -ProveItFails fixture - verified there against a
# REAL odin run (2026-09-05): cognitive 174, cyclomatic 30. Reusing the exact same source
# cross-validates this port against that real measurement instead of a number this script
# invented for itself.
_DEEP_FN = """fn deeply_nested(a: i32, b: i32, c: i32, d: i32, e: i32) -> i32 {
    let mut total = 0;
    if a > 0 {
        if b > 0 {
            if c > 0 {
                if d > 0 {
                    if e > 0 {
                        for i in 0..a {
                            if i % 2 == 0 {
                                if i % 3 == 0 {
                                    if i % 5 == 0 {
                                        while total < b {
                                            match i {
                                                0 => total += 1,
                                                1 => total += 2,
                                                2 => total += 3,
                                                3 => total += 4,
                                                4 => total += 5,
                                                5 => total += 6,
                                                6 => total += 7,
                                                7 => total += 8,
                                                8 => total += 9,
                                                9 => total += 10,
                                                _ => {
                                                    if total > 100 {
                                                        if total > 200 {
                                                            if total > 300 {
                                                                if total > 400 {
                                                                    break;
                                                                } else { total += 1; }
                                                            } else { total += 2; }
                                                        } else { total += 3; }
                                                    } else { total += 4; }
                                                }
                                            }
                                        }
                                    } else if e < -5 { total -= 1; } else { total -= 2; }
                                } else if d < -5 { total -= 3; } else { total -= 4; }
                            } else if c < -5 { total -= 5; } else { total -= 6; }
                        }
                    } else if b < -5 { total -= 7; } else { total -= 8; }
                } else if a < -5 { total -= 9; } else { total -= 10; }
            } else { total -= 11; }
        } else { total -= 12; }
    } else { total -= 13; }
    total
}
"""

# A minimal fixture proving the try-operator ("?") rule this whole gate exists to
# document: FIVE fallible calls chained with `?` inside one flat match arm dispatcher have
# no nesting at all, yet still cross the gate on `?` alone (CLAUDE.md / DEVELOPMENT_
# GOTCHAS.md "it scores every `?` as a branch").
_TRY_OPERATOR_FN = "fn dispatch(x: u8) -> Result<(), E> {\n" + "\n".join(
    f"    if x == {i} {{ step{i}()?; }}" for i in range(1, 32)
) + "\n    Ok(())\n}\n"


def _self_test():
    failures = 0

    def check(label, fixture, expect_over_gate):
        masked = mask_rust_literals_and_comments(fixture)
        fns = extract_functions(masked)
        if len(fns) != 1:
            print(f"  FAIL  {label}: expected exactly 1 function extracted, got {len(fns)}")
            return False
        clean = _hash_blank(fns[0]["body"])
        cyc = cyclomatic_complexity(clean)
        cog = cognitive_complexity(clean)
        over = cyc >= GATE or cog >= GATE
        ok = over == expect_over_gate
        status = "PASS" if ok else "FAIL"
        print(f"  {status}  {label}: cyclomatic={cyc} cognitive={cog} (expected over-gate={expect_over_gate})")
        return ok

    if not check("trivial function stays clean", _TRIVIAL_FN, False):
        failures += 1
    if not check("deeply nested function crosses the gate", _DEEP_FN, True):
        failures += 1
    if not check("chained try-operator crosses the gate on '?' alone", _TRY_OPERATOR_FN, True):
        failures += 1

    # Cross-validate the deep fixture's EXACT numbers against the real odin measurement
    # recorded in check-complexity.ps1's own -ProveItFails comment (2026-09-05: cognitive
    # 174, cyclomatic 30) - a byte-for-byte port should reproduce them exactly, not merely
    # land on the same side of the gate.
    masked = mask_rust_literals_and_comments(_DEEP_FN)
    fns = extract_functions(masked)
    if fns:
        clean = _hash_blank(fns[0]["body"])
        cyc = cyclomatic_complexity(clean)
        cog = cognitive_complexity(clean)
        if (cyc, cog) == (30, 174):
            print("  PASS  deep fixture matches the real odin measurement exactly (cyc=30, cog=174)")
        else:
            print(f"  FAIL  deep fixture diverges from the real odin measurement: got cyc={cyc} cog={cog}, expected cyc=30 cog=174")
            failures += 1

    if failures:
        print(f"complexity-scan --self-test: FAILED - {failures} case(s) did not match expectations.")
        return 1
    print("complexity-scan --self-test: OK - clean/over-gate/try-operator fixtures all scored as expected.")
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
