"""Repoint every script, workflow and doc that names a lifted module by its old path
(`src/settings.rs`, `src\\formats.rs`, `./src/settings/`) at its new home under
crates/<layer>/src (companion of lift_layer.py).

    python scripts/refactor/repath_layer.py <layer> [--dry-run]

Keeps each match's own separator style. Touches scripts/, .github/, docs/, tests/, crates/
(minus vendor and the lifted crates' own sources, whose paths are relative) and the root
CLAUDE.md / README.md. Binary files, build output and history (ROADMAP, CHANGELOG, the size
budget's rationales) are skipped.
"""
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from lift_layer import LAYERS, ROOT  # noqa: E402

TEXT = {".ps1", ".psm1", ".py", ".mjs", ".js", ".yml", ".yaml", ".json", ".toml", ".md", ".rs", ".sh", ".txt", ".iss"}
SKIP_DIRS = {"vendor", "stage", "__pycache__", "node_modules", "target", ".git"}
# History, which records the paths as they were when it was written, and these instruments.
SKIP_FILES = {"docs/ROADMAP.md", "docs/CHANGELOG.md", "scripts/packaging/size-budget.json",
              "scripts/refactor/repath_layer.py", "scripts/refactor/lift_layer.py"}


def candidates():
    for top in ("scripts", ".github", "docs", "tests", "crates"):
        for p in (ROOT / top).rglob("*"):
            if p.is_file() and p.suffix in TEXT and not SKIP_DIRS & set(p.relative_to(ROOT).parts):
                yield p
    for name in ("CLAUDE.md", "README.md", "AGENTS.md"):
        if (ROOT / name).exists():
            yield ROOT / name


def main():
    layer = sys.argv[1]
    dry = "--dry-run" in sys.argv
    alt = "|".join(sorted(LAYERS[layer], key=len, reverse=True))
    # Already under crates/<some layer>/src: leave it (look-behinds must be fixed-width, so one each).
    behind = "".join(rf"(?<!{name}[/\\])" for name in LAYERS)
    pat = re.compile(rf"{behind}(?<![\w.-])src(?P<sep>[/\\]+)(?P<m>{alt})(?=\.rs\b|[/\\]|\b)")
    total = 0
    for p in candidates():
        rel = p.relative_to(ROOT).as_posix()
        if rel.startswith(f"crates/{layer}/") or rel in SKIP_FILES:
            continue
        t = p.read_text(encoding="utf-8", errors="strict") if p.stat().st_size < 4_000_000 else None
        if t is None:
            continue
        n = pat.sub(lambda m: f"crates{m['sep']}{layer}{m['sep']}src{m['sep']}{m['m']}", t)
        if n != t:
            count = len(pat.findall(t))
            total += count
            print(f"{count:4}  {rel}")
            if not dry:
                p.write_text(n, encoding="utf-8", newline="")
    print(f"{total} paths {'would be ' if dry else ''}repointed")


if __name__ == "__main__":
    main()
