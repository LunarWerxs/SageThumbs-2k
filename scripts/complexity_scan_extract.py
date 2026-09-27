"""Finding every trackable Rust `fn` and its body span (see complexity-scan.py's header)."""

import re

from complexity_scan_headers import _MAX_HEADER_TO_BRACE_GAP, _matching_balanced, _parse_header_tail


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
