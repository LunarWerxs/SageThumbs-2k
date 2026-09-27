"""Per-function cyclomatic and cognitive complexity, and the gate they are scored against (see
complexity-scan.py's header)."""

import re

GATE = 30
WARN = 15


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
