"""Reading a rustfmt-formatted .rs file as a list of top-level items, and the visibility a moved
item needs one module deeper. The parsing half of extract_items.py."""
import re

ITEM = re.compile(
    r"^(?P<vis>pub(\([^)]*\))?\s+)?(?P<q>(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*)(?P<kind>fn|struct|enum|union|trait|type|const|static|impl|mod|macro_rules!)\b(?P<rest>.*)$"
)
IMPL = re.compile(r"^impl(<[^>]*>)?\s+(?:(?P<trait>[\w:<>, ]+?)\s+for\s+)?(?P<ty>[\w:]+)")
LEAD = re.compile(r"^(///|//|#\[|#!\[)")
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
CLOSER = re.compile(r"^[\]\)}]+;?$")
SUPER = re.compile(r"^(\s*)pub\(super\) ")
FIELD = re.compile(r"^    [a-z_][a-z0-9_]*: ")
METHOD = re.compile(r"^    (const |unsafe )?fn ")


# --- parsing ---------------------------------------------------------------------------


def item_key(ln, m):
    """'fn name', 'struct name', 'impl Ty', 'impl Trait for Ty', 'macro name' - or None."""
    kind = m.group("kind")
    rest = m.group("rest").strip()
    if kind == "impl":
        im = IMPL.match(ln)
        if im is None:
            return None
        trait = im.group("trait")
        return f"impl {trait + ' for ' if trait else ''}{im.group('ty')}"
    if kind == "macro_rules!":
        return "macro " + rest.split("{")[0].strip()
    name = NAME.match(rest)
    return f"{kind} {name.group(0)}" if name else None


def attr_open(lines, idx):
    """Index of the `#[` line opening the multi-line attribute that closes at `idx`, or None."""
    k = idx
    while k > 0 and not lines[k].startswith("#["):
        k -= 1
    return k if lines[k].startswith("#[") else None


def lead_start(lines, i):
    """First line of the doc comments / attributes that belong to the item at `i`."""
    start = i
    while start > 0:
        prev = lines[start - 1]
        if prev.startswith("#!["):
            break
        if LEAD.match(prev):
            start -= 1
            continue
        opened = attr_open(lines, start - 1) if prev in (")]", "]") else None
        if opened is None:
            break
        start = opened
    return start


def scan_to(lines, j, pred):
    while j < len(lines) and not pred(lines[j]):
        j += 1
    return j


def item_end(lines, i):
    """Exclusive end line of the item that starts at `i`."""
    first = lines[i].rstrip()
    if first.endswith(";"):
        return i + 1
    if first.endswith(("{", "(")):
        return scan_to(lines, i + 1, lambda ln: ln == "}" or CLOSER.match(ln) is not None) + 1
    j = scan_to(lines, i, lambda ln: ln.rstrip().endswith(("{", ";")))
    if j < len(lines) and lines[j].rstrip().endswith(";"):
        return j + 1
    return scan_to(lines, j + 1, lambda ln: ln == "}") + 1


def parse_items(lines):
    """[(start incl. leading docs/attrs, end exclusive, key)] for every top-level item."""
    items = []
    i = 0
    while i < len(lines):
        ln = lines[i]
        m = None if ln.startswith(" ") else ITEM.match(ln)
        key = item_key(ln, m) if m else None
        if key is None:
            i += 1
            continue
        end = item_end(lines, i)
        items.append((lead_start(lines, i), end, key))
        i = end
    return items


# --- visibility ------------------------------------------------------------------------


def mark_top(ln, m):
    """A column-0 item line: private -> `pub(super)`. Returns (line, (in_struct, in_impl))."""
    kind = m.group("kind")
    if not m.group("vis") and kind not in ("impl", "macro_rules!"):
        ln = "pub(super) " + ln
    in_struct = kind == "struct" and ln.rstrip().endswith("{")
    # inherent impls only: a trait impl's methods take no visibility qualifier
    in_impl = kind == "impl" and " for " not in ln
    return ln, (in_struct, in_impl)


def make_pub_super(block):
    """Private -> `pub(super)`, fields and inherent methods included. An item that was ALREADY
    `pub(super)` was visible to the hub's parent; one level deeper that is
    `pub(in super::super)`, and the hub re-exports it by name."""
    out = []
    state = (False, False)
    for ln in (SUPER.sub(r"\1pub(in super::super) ", raw) for raw in block):
        top = None if ln.startswith(" ") else ITEM.match(ln)
        if top:
            ln, state = mark_top(ln, top)
        elif (state[0] and FIELD.match(ln)) or (state[1] and METHOD.match(ln)):
            ln = "    pub(super) " + ln[4:]
        out.append(ln)
    return out


def declared_vis(block):
    for ln in block:
        m = ITEM.match(ln)
        if m:
            return (m.group("vis") or "").strip()
    return ""


def reexports(lines, found, child):
    """Items that were `pub` / `pub(crate)` / `pub(super)` keep that reach through the hub: a
    private glob import does not re-export them, so they are named."""
    by_vis = {}
    for s, e, key in found:
        kind, _, name = key.partition(" ")
        vis = declared_vis(lines[s:e])
        if vis and kind not in ("impl", "macro"):
            # a `#[cfg(test)]` item's re-export must carry the same gate or the non-test build breaks
            test_only = any(ln.strip() == "#[cfg(test)]" for ln in lines[s:e] if ln.startswith("#"))
            by_vis.setdefault((vis, test_only), []).append(name)
    out = []
    for (vis, test_only), names in sorted(by_vis.items()):
        out += ["#[cfg(test)]"] if test_only else []
        out.append(f"{vis} use {child}::{{{', '.join(names)}}};")
    return out
