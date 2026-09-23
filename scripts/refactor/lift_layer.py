"""Lift one layer of the library (see crate_layers.py's LAYERS) out of `sagethumbs2k_core` into
its own workspace crate, and repoint every path that named it.

    python scripts/refactor/lift_layer.py <layer> [--dry-run]

What it does, in order:
  1. `git mv` each module of the layer (its `<m>.rs` and `<m>/` folder) to crates/<layer>/src/.
  2. Moves each module's declaration (and the comment/attribute lines above it) out of
     src/lib.rs into the new crate's lib.rs, as `pub mod`, and drops lib.rs's `pub use` lines
     that re-exported from those modules (their users are repointed at the owner instead).
  3. Rewrites every path that named a moved module or macro:
       - in the core library (src/**, not src/bin):  crate::m          -> <lib>::m
       - in the binaries, tests and examples:        sagethumbs2k_core::m -> <lib>::m
       - a grouped `use crate::{a, m, b}` is split into `use crate::{a, b}` + `use <lib>::m`
       - a name lib.rs re-exported (`sagethumbs2k_core::PdfPage`) -> its owner's full path
  4. Writes crates/<layer>/Cargo.toml (package `sagethumbs2k-<layer>`, lib `st2k_<layer>`) with
     the external crates the moved files name, and adds the crate to the workspace and as a
     dependency of the root package and of every crate above it that is already lifted.

Visibility is NOT touched here: `pub(crate)` items the layers above use are widened by
`widen_pub.py`, driven by the compiler's own errors. Run `cargo check --workspace
--all-targets` after this and feed it to that script.
"""
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "src"
sys.path.insert(0, str(Path(__file__).parent))

LAYERS = {}
_text = (Path(__file__).parent / "crate_layers.py").read_text(encoding="utf-8")
for name, mods in re.findall(r'\("(\w+)",\s*((?:"[^"]*"\s*)+)\)', _text):
    LAYERS[name] = " ".join(re.findall(r'"([^"]*)"', mods)).split()
ORDER = list(LAYERS)

# External crates a moved file may name, as `<crate>::` in source -> Cargo.toml dependency line.
# Kept in step with the root Cargo.toml; `windows` comes from [workspace.dependencies].
EXTERNAL = {
    "windows": 'windows = { workspace = true }',
    "windows_registry": 'windows-registry = "0.6"',
    "windows_core": 'windows-core = "0.62"',
    "windows_implement": 'windows-implement = "0.60"',
    "windows_future": 'windows-future = "0.3"',
    "image": 'image = { workspace = true }',
    "serde_json": 'serde_json = "1"',
    "base64": 'base64 = "0.23"',
    "percent_encoding": 'percent-encoding = "2"',
    "moxcms": 'moxcms = "0.8"',
    "zune_jpeg": 'zune-jpeg = "0.5"',
    "tiff": 'tiff = { version = "0.11", default-features = false }',
    "exif": 'kamadak-exif = "0.6"',
    "img_parts": 'img-parts = "0.4"',
    "webp": 'webp = { version = "0.3", default-features = false, optional = true }',
    "resvg": 'resvg = { version = "0.48", default-features = false }',
    "roxmltree": 'roxmltree = "0.21"',
    "zip": 'zip = { version = "8", default-features = false, features = ["deflate-flate2"] }',
    "flate2": 'flate2 = { version = "1", default-features = false, features = ["rust_backend"] }',
    "ruzstd": 'ruzstd = "0.9"',
    "sevenz_rust2": 'sevenz-rust2 = { version = "0.23", default-features = false }',
    "lofty": 'lofty = "0.25"',
    "rars": 'rars = "0.9.4"',
    "djvu_rs": 'djvu-rs = { version = "0.35.0", default-features = false, features = ["std"] }',
    "jxl_oxide": 'jxl-oxide = { version = "0.12", default-features = false, features = ["image"] }',
    "bcdec_rs": 'bcdec_rs = "0.2"',
    "exr": 'exr = { version = "=1.74.2", default-features = false }',
}


def lib_name(layer):
    return f"st2k_{layer}"


def files_of(base, m):
    out = [p for p in (base / f"{m}.rs",) if p.exists()]
    if (base / m).is_dir():
        out += sorted((base / m).rglob("*.rs"))
    return out


def exported_macros(files):
    names = []
    for f in files:
        names += re.findall(r"#\[macro_export\]\s*macro_rules!\s*(\w+)", f.read_text(encoding="utf-8"))
    return names


# ---------------------------------------------------------------------------------------------
# lib.rs surgery


def split_lib(lib_text, mods):
    """(new lib.rs text, [declaration blocks moved], {reexported name: full path})."""
    lines = lib_text.split("\n")
    keep, moved, reexports = [], [], {}
    i = 0
    pending = []  # comment/attribute lines waiting to see what they belong to
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        if stripped.startswith(("//", "#[")) and not stripped.startswith("//!") and not stripped.startswith("#!["):
            pending.append(line)
            i += 1
            continue
        m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", stripped)
        if m and m.group(1) in mods:
            moved.append(pending + [re.sub(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod", "pub mod", line)])
            pending = []
            i += 1
            continue
        u = re.match(r"^pub use (\w+)::(.+?);\s*$", stripped)
        if u and u.group(1) in mods:
            head, rest = u.group(1), u.group(2)
            for leaf in _flatten_use(rest):
                reexports[leaf.split("::")[-1]] = f"{head}::{leaf}"
            pending = []  # a doc comment on a re-export goes with it
            i += 1
            continue
        # Multi-line `pub use m::{...};`
        u = re.match(r"^pub use (\w+)::\{", stripped)
        if u and u.group(1) in mods:
            block = [line]
            while not lines[i].rstrip().endswith("};"):
                i += 1
                block.append(lines[i])
            body = " ".join(block)
            inner = body[body.index("{") + 1: body.rindex("}")]
            for leaf in _flatten_use(inner):
                reexports[leaf.split("::")[-1]] = f"{u.group(1)}::{leaf}"
            pending = []
            i += 1
            continue
        keep += pending
        pending = []
        keep.append(line)
        i += 1
    keep += pending
    return "\n".join(keep), moved, reexports


def _split_top(s):
    """Split a use-list body on top-level commas."""
    parts, depth, cur = [], 0, ""
    for ch in s:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur.strip())
    return parts


def _flatten_use(body):
    out = []
    for part in _split_top(body):
        if "{" in part:
            prefix = part[: part.index("{")].rstrip(":")
            for leaf in _flatten_use(part[part.index("{") + 1: part.rindex("}")]):
                out.append(f"{prefix}::{leaf}")
        else:
            out.append(part.split(" as ")[0].strip())
    return out


# ---------------------------------------------------------------------------------------------
# path rewriting

GROUP = re.compile(r"(?P<lead>(?:pub(?:\([^)]*\))?\s+)?use\s+)(?P<root>crate|sagethumbs2k_core|core|st2k_core)::\{", re.S)


def _braced(parts):
    return parts[0] if len(parts) == 1 and parts[0] != "self" else "{" + ", ".join(parts) + "}"


def rewrite_groups(text, root_names, heads, reexports, lib):
    """Split every grouped `use <root>::{...};` whose members include a moved head or a name
    lib.rs re-exported from one."""
    out, pos = [], 0
    for m in GROUP.finditer(text):
        if m.group("root") not in root_names or m.start() < pos:
            continue
        # find the matching close brace
        i, depth = m.end(), 1
        while depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        end = text.index(";", i) + 1
        body = text[m.end(): i - 1]
        kept, moved = [], []
        for part in _split_top(body):
            head = re.match(r"(\w+)", part).group(1) if re.match(r"(\w+)", part) else ""
            if head in heads:
                moved.append(part)
            elif head in reexports:
                moved.append(reexports[head] + part[len(head):])
            else:
                kept.append(part)
        if not moved:
            continue
        indent = re.match(r"[ \t]*", text[text.rfind("\n", 0, m.start()) + 1:]).group(0)
        lead = m.group("lead")
        new = []
        if kept:
            new.append(f"{lead}{m.group('root')}::{_braced(kept)};")
        new.append(f"{lead}{lib}::{_braced(moved)};")
        out.append(text[pos: m.start()])
        out.append(("\n" + indent).join(new))
        pos = end
    out.append(text[pos:])
    return "".join(out)


def rewrite_file(path, text, roots, heads, reexports, lib):
    t = rewrite_groups(text, roots, heads, reexports, lib)
    alt = "|".join(sorted(heads, key=len, reverse=True))
    # A macro's `$crate::m` names the crate the macro is defined in; once `m` lives elsewhere
    # the path goes through the owner, which every crate above it depends on.
    t = re.sub(rf"\$crate::({alt})\b", rf"::{lib}::\1", t)
    for root in roots:
        t = re.sub(rf"(?<![\w$]){root}::({alt})\b", rf"{lib}::\1", t)
        for name, full in reexports.items():
            t = re.sub(rf"(?<![\w$]){root}::{name}\b", f"{lib}::{full}", t)
    return t


def main():
    layer = sys.argv[1]
    dry = "--dry-run" in sys.argv
    mods = LAYERS[layer]
    lib = lib_name(layer)
    dest = ROOT / "crates" / layer
    below = ORDER[: ORDER.index(layer)]

    moved_files = [f for m in mods for f in files_of(SRC, m)]
    macros = exported_macros(moved_files)
    heads = set(mods) | set(macros)

    lib_rs = (SRC / "lib.rs").read_text(encoding="utf-8")
    new_lib, blocks, reexports = split_lib(lib_rs, set(mods))
    missing = set(mods) - {re.search(r"pub mod (\w+)", b[-1]).group(1) for b in blocks}
    if missing:
        sys.exit(f"not declared in lib.rs: {sorted(missing)}")

    ext = set()
    for f in moved_files:
        ext |= set(re.findall(r"\b([a-z_][a-z0-9_]*)::", f.read_text(encoding="utf-8"))) & set(EXTERNAL)
    print(f"{layer}: {len(mods)} modules, {len(moved_files)} files, macros {macros}, "
          f"re-exports {sorted(reexports)}, external {sorted(ext)}")
    if dry:
        return

    # 1. move the files, keeping every relative `include_str!`/`include_bytes!` pointed at the
    #    same file (a path that climbs out of src/ now climbs out of crates/<layer>/src/).
    (dest / "src").mkdir(parents=True, exist_ok=True)
    moved_set = {f.resolve() for f in moved_files}
    include = re.compile(r'(include_(?:str|bytes)!\(\s*")([^"]+)("\s*\))')
    for f in moved_files:
        new_dir = (dest / "src" / f.relative_to(SRC)).parent
        text = f.read_text(encoding="utf-8")

        def fix(m, f=f, new_dir=new_dir):
            target = (f.parent / m.group(2)).resolve()
            if target in moved_set or any(target.is_relative_to(SRC / mm) for mm in mods):
                target = dest / "src" / target.relative_to(SRC)
            rel = Path(os.path.relpath(target, new_dir)).as_posix()
            return m.group(1) + rel + m.group(3)

        fixed = include.sub(fix, text)
        # Test code that reads a repo file off the manifest dir keeps meaning the repo root.
        fixed = fixed.replace('env!("CARGO_MANIFEST_DIR")', 'concat!(env!("CARGO_MANIFEST_DIR"), "/../..")')
        if fixed != text:
            f.write_text(fixed, encoding="utf-8", newline="\n")
    for m in mods:
        for p in (SRC / f"{m}.rs", SRC / m):
            if p.exists():
                subprocess.run(["git", "mv", str(p), str(dest / "src" / p.name)], cwd=ROOT, check=True)

    # 2. the new crate's lib.rs and Cargo.toml
    decls = "\n".join("\n".join(b) for b in sorted(blocks, key=lambda b: re.search(r"pub mod (\w+)", b[-1]).group(1)))
    (dest / "src" / "lib.rs").write_text(
        f"//! The `{layer}` layer of SageThumbs 2K's library (see scripts/refactor/crate_layers.py).\n"
        "//! It names only the layers below it, so an edit above it never recompiles it.\n\n"
        "#![allow(non_snake_case)]\n"
        "// Compiled into the shell-extension DLL, which runs inside explorer.exe under\n"
        "// `panic = \"abort\"`: no `.unwrap()`/`.expect()` outside tests (see the core crate).\n"
        "#![warn(clippy::unwrap_used, clippy::expect_used)]\n\n" + decls + "\n",
        encoding="utf-8", newline="\n")
    deps = [EXTERNAL[e] for e in sorted(ext)]
    deps += [f'{lib_name(b)} = {{ package = "sagethumbs2k-{b}", path = "../{b}" }}' for b in below]
    (dest / "Cargo.toml").write_text(
        f'[package]\nname = "sagethumbs2k-{layer}"\nversion.workspace = true\nedition = "2021"\n'
        'rust-version.workspace = true\npublish = false\nlicense = "PolyForm-Noncommercial-1.0.0"\n'
        f'description = "SageThumbs 2K library, {layer} layer"\n\n[lib]\nname = "{lib}"\n\n'
        "[dependencies]\n" + "\n".join(deps) + "\n",
        encoding="utf-8", newline="\n")
    (SRC / "lib.rs").write_text(new_lib, encoding="utf-8", newline="\n")

    # 3. repoint every path
    targets = []
    for p in SRC.rglob("*.rs"):
        rel = p.relative_to(SRC).as_posix()
        if rel == "build.rs":
            continue
        roots = ["sagethumbs2k_core", "core"] if rel.startswith("bin/") else ["crate"]
        targets.append((p, roots))
    for top in ("tests", "examples"):
        for p in (ROOT / top).rglob("*.rs"):
            targets.append((p, ["sagethumbs2k_core", "core"]))
    for above in ORDER[ORDER.index(layer) + 1:]:
        for p in (ROOT / "crates" / above / "src").rglob("*.rs") if (ROOT / "crates" / above).exists() else []:
            targets.append((p, ["crate"]))
    changed = 0
    for p, roots in targets:
        t = p.read_text(encoding="utf-8")
        # `core::` is only an alias of the core crate in a file that says so.
        roots = [r for r in roots if r != "core" or "use sagethumbs2k_core as core;" in t]
        n = rewrite_file(p, t, roots, heads, reexports, lib)
        if n != t:
            p.write_text(n, encoding="utf-8", newline="\n")
            changed += 1
    print(f"repointed {changed} files")

    # 4. the workspace
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    # A member of both lists: `cargo test` at the root tests the default members only.
    cargo = re.sub(r"^members = \[", f'members = ["crates/{layer}", ', cargo, count=1, flags=re.M)
    cargo = re.sub(r'^default-members = \["\.", ', f'default-members = [".", "crates/{layer}", ', cargo, count=1, flags=re.M)
    dep = f'{lib} = {{ package = "sagethumbs2k-{layer}", path = "crates/{layer}" }}'
    cargo = cargo.replace("\n[dependencies]\n", f"\n[dependencies]\n# The {layer} layer of the library, its own crate.\n{dep}\n", 1)
    (ROOT / "Cargo.toml").write_text(cargo, encoding="utf-8", newline="\n")
    for above in ORDER[ORDER.index(layer) + 1:]:
        toml = ROOT / "crates" / above / "Cargo.toml"
        if toml.exists():
            t = toml.read_text(encoding="utf-8")
            t = t.replace("[dependencies]\n", f'[dependencies]\n{lib} = {{ package = "sagethumbs2k-{layer}", path = "../{layer}" }}\n', 1)
            toml.write_text(t, encoding="utf-8", newline="\n")
    print("done; now: cargo check --workspace --all-targets, then widen_pub.py")


if __name__ == "__main__":
    main()
