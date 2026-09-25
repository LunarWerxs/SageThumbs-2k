"""After a lift, keep private every module of the new crate that nothing outside it names
(lift_layer.py declares them all `pub mod`; widen_pub.py only ever widens).

    python scripts/refactor/private_mods.py <layer> [--apply]

A module is "named outside" when any .rs file outside crates/<layer> (the core crate, the
binaries, tests, examples, the other layers) spells `st2k_<layer>::<module>`, including inside
a grouped `use st2k_<layer>::{...}`. Prints what it would make private; --apply rewrites the
crate's lib.rs.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main():
    layer = sys.argv[1]
    lib = ROOT / "crates" / layer / "src" / "lib.rs"
    text = lib.read_text(encoding="utf-8")
    mods = re.findall(r"^pub mod (\w+);", text, re.M)
    outside = []
    for top in ("src", "tests", "examples", "crates"):
        for p in (ROOT / top).rglob("*.rs"):
            rel = p.relative_to(ROOT).as_posix()
            if rel.startswith(f"crates/{layer}/") or "/vendor/" in rel:
                continue
            outside.append(p.read_text(encoding="utf-8", errors="replace"))
    blob = "\n".join(outside)
    groups = " ".join(re.findall(rf"st2k_{layer}::\{{([^;]*?)\}};", blob, re.S))
    private = []
    for m in mods:
        if re.search(rf"\bst2k_{layer}::{m}\b", blob) or re.search(rf"(?:^|[{{,\s]){m}\b", groups):
            continue
        private.append(m)
    print(f"private in {layer}: {private}")
    if "--apply" in sys.argv and private:
        for m in private:
            text = re.sub(rf"^pub mod {m};", f"mod {m};", text, flags=re.M)
        lib.write_text(text, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
