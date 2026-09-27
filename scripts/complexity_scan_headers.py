"""Parsing a Rust `fn` header's type parameters and parameter list, reference quirks included
(see complexity-scan.py's header)."""


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
