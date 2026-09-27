"""The low-level Rust lexical helpers complexity-scan.py's mask is built from (see its header)."""


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
