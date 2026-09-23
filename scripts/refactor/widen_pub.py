"""Widen to `pub` exactly the items of a freshly lifted crate that the crates above it use,
driven by the compiler's own errors (companion of lift_layer.py).

    python scripts/refactor/widen_pub.py <crate-dir> [--rounds N]

Each round runs `cargo check --workspace --all-targets --message-format=json` (through the
shared fairjob wrapper) and, for every error or warning that means "this item was private to
its old crate", makes the DEFINITION `pub`:

    E0603  <item> is private              -> the "defined here" note's line
    E0624  method is private              -> the "defined here" note's line
    E0616 / E0451  field is private       -> that field, found in its struct
    private_interfaces / private_bounds   -> the more-private type's definition
    E0364 / E0365  re-export of a crate-private item -> the item the note points at

NOT dead_code: an item looks dead only because the entry point that uses it is not `pub` yet,
so widening on it made ~1,650 app items `pub` where a few dozen entry points needed it
(2026-09-23). Widen the entry points; whatever is still dead after that is genuinely dead.

Only files under <crate-dir> are edited. Stops when a round finds nothing to widen, and
prints whatever errors are left (those are real breaks for a person to read).
"""
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FAIRJOB = Path.home() / ".claude" / "tools" / "fairjob.cmd"
ITEM = re.compile(
    r"^(?P<ind>\s*)(?:pub\s*\([^)]*\)\s+|pub\s+)?(?P<rest>(?:(?:const|async|unsafe|extern\s+\"[^\"]*\")\s+)*"
    r"(?:fn|struct|enum|union|const|static|type|trait|mod|use)\b.*)$")
FIELD = re.compile(r"^(?P<ind>\s*)(?:pub\s*\([^)]*\)\s+|pub\s+)?(?P<name>\w+)\s*:(?!:)")


def check():
    cmd = "cargo check --workspace --all-targets --message-format=json"
    out = subprocess.run(["cmd", "/c", str(FAIRJOB), "-Weight", "3", "-MinFreeGB", "5", "-Run", cmd],
                         cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    msgs = []
    for line in out.splitlines():
        if line.startswith("{"):
            m = json.loads(line)
            if m.get("reason") == "compiler-message":
                msgs.append(m["message"])
    return msgs


def within(path, crate):
    try:
        (ROOT / path).resolve().relative_to(crate.resolve())
        return True
    except ValueError:
        return False


USE_OPEN = re.compile(r"^\s*(?:pub\s*\([^)]*\)\s+|pub\s+)?use\s")


def widen_line(lines, idx):
    """Make the item that starts at or just after lines[idx] `pub`. True when changed.

    A name inside a multi-line `use a::{ ... };` group is pointed at on its own line, which is
    no item: the group's `use` line above it is what gets widened."""
    if not ITEM.match(lines[idx]) and not USE_OPEN.match(lines[idx]):
        depth = 0
        for j in range(idx, max(idx - 12, -1), -1):
            depth += lines[j].count("}") - lines[j].count("{")
            if USE_OPEN.match(lines[j]) and depth < 0:
                idx = j
                break
    for j in range(idx, min(idx + 4, len(lines))):
        m = ITEM.match(lines[j])
        if m:
            new = f"{m.group('ind')}pub {m.group('rest')}"
            if new != lines[j] and not lines[j].lstrip().startswith("pub "):
                lines[j] = new
                return True
            return False
    return False


def widen_field(files, struct, field):
    for f in files:
        lines = f.read_text(encoding="utf-8").split("\n")
        for i, l in enumerate(lines):
            if re.search(rf"\bstruct\s+{struct}\b", l):
                depth = 0
                for j in range(i, len(lines)):
                    depth += lines[j].count("{") - lines[j].count("}")
                    fm = FIELD.match(lines[j])
                    if j > i and fm and fm.group("name") == field and not lines[j].lstrip().startswith("pub "):
                        lines[j] = re.sub(r"^(\s*)(?:pub\s*\([^)]*\)\s+)?", r"\1pub ", lines[j], count=1)
                        f.write_text("\n".join(lines), encoding="utf-8", newline="\n")
                        return True
                    if j > i and depth <= 0:
                        break
    return False


def spans_defined_here(msg):
    # E0624 (a private method) labels its definition on a secondary span of the error itself;
    # E0603 puts it in a child note. Take both.
    for sp in msg.get("spans", []):
        if not sp["is_primary"] and (sp.get("label") or "").endswith("defined here"):
            yield sp
    for ch in msg.get("children", []):
        for sp in ch.get("spans", []):
            if "defined here" in (ch.get("message") or "") or (sp.get("label") or "").endswith("defined here"):
                yield sp


def restricted_definitions(name, files, edits):
    """E0364/E0365 carry no note pointing at the item: find its restricted-visibility
    definition, or a crate-private re-export it passes through."""
    item = re.compile(rf"\s*pub\s*\([^)]*\)\s+(?:(?:const|async|unsafe)\s+)*"
                      rf"(?:fn|struct|enum|union|const|static|type|trait|mod)\s+{name}\b")
    reexport = re.compile(rf"\s*pub\s*\([^)]*\)\s+use\b[^;]*\b{name}\b[^;]*;")
    for f in files:
        lines = f.read_text(encoding="utf-8").split("\n")
        hits = [i for i, l in enumerate(lines) if item.match(l) or reexport.match(l)]
        if hits:
            edits[str(f.relative_to(ROOT))].update(hits)


def child_spans_within(msg, crate):
    return [sp for ch in msg.get("children", []) for sp in ch.get("spans", []) if within(sp["file_name"], crate)]


def collect(msg, crate, files, edits, fields):
    """Record what one compiler message asks to widen: definition lines in `edits`
    (file -> line indexes), private struct fields in `fields`."""
    code = (msg.get("code") or {}).get("code")
    text = msg["message"]
    if code in ("E0603", "E0624"):
        for sp in spans_defined_here(msg):
            if within(sp["file_name"], crate):
                edits[sp["file_name"]].add(sp["line_start"] - 1)
    elif code in ("E0616", "E0451"):
        # "field `a` of struct `S` is private" / "fields `a`, `b` and `c` of struct ..."
        m = re.search(r"fields? ((?:`\w+`(?:, | and )?)+) of (?:struct|union) `(?:[\w:]*::)?(\w+)", text)
        if m:
            fields.update((m.group(2), name) for name in re.findall(r"`(\w+)`", m.group(1)))
    elif code in ("E0364", "E0365") and (named := re.match(r"`(\w+)` is only public within", text)):
        restricted_definitions(named.group(1), files, edits)
    elif code in ("private_interfaces", "private_bounds", "E0364", "E0365"):
        for sp in child_spans_within(msg, crate):
            edits[sp["file_name"]].add(sp["line_start"] - 1)


def apply(edits, fields, files):
    changed = 0
    for fname, idxs in edits.items():
        p = ROOT / fname
        lines = p.read_text(encoding="utf-8").split("\n")
        n = sum(widen_line(lines, i) for i in sorted(idxs))
        if n:
            p.write_text("\n".join(lines), encoding="utf-8", newline="\n")
            changed += n
    for struct, field in sorted(fields):
        changed += widen_field(files, struct, field)
    return changed


def report(errors):
    """Print each remaining error once: those are real breaks for a person to read."""
    seen = set()
    for m in errors:
        sp = next((s for s in m["spans"] if s["is_primary"]), None)
        key = (m["message"], sp and sp["file_name"], sp and sp["line_start"])
        if key not in seen:
            seen.add(key)
            print(f"  {sp and sp['file_name']}:{sp and sp['line_start']}: {m['message']}")


def main():
    crate = (ROOT / sys.argv[1]).resolve()
    rounds = int(sys.argv[sys.argv.index("--rounds") + 1]) if "--rounds" in sys.argv else 8
    files = list(crate.rglob("*.rs"))
    for rnd in range(1, rounds + 1):
        msgs = check()
        edits = defaultdict(set)  # file -> line indexes
        fields = set()
        for msg in msgs:
            collect(msg, crate, files, edits, fields)
        changed = apply(edits, fields, files)
        errors = [m for m in msgs if m["level"] == "error"]
        print(f"round {rnd}: widened {changed}; {len(errors)} errors in the run")
        if not changed:
            report(errors)
            return


if __name__ == "__main__":
    main()
