"""The Rust span rules and the comment/literal mask complexity-scan.py scores through (see its
header)."""

from complexity_scan_lexical import _blank, _char_literal_length, _is_escaped, _raw_string_open


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
