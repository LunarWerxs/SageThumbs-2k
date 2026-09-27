"""The pinned upstream Material Symbols files, and fetching them safely (see build-icon-font.py)."""

from __future__ import annotations

import sys
import urllib.request
from pathlib import Path


# Pinned to a specific upstream commit rather than `master` so a build is reproducible and a
# changed (rotated, compromised, or just re-released) upstream file is caught instead of
# silently bundled. To move the pin: resolve the new commit with
#   git ls-remote https://github.com/google/material-design-icons.git refs/heads/master
# then download both files at that commit and recompute their SHA-256 before updating the four
# constants below together.
UPSTREAM_COMMIT = "0cbb08816df07faaae3dca060d4ebb10b66c214f"
UPSTREAM_CODEPOINTS = (
    f"https://raw.githubusercontent.com/google/material-design-icons/{UPSTREAM_COMMIT}/"
    "variablefont/MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.codepoints"
)
UPSTREAM = (
    f"https://github.com/google/material-design-icons/raw/{UPSTREAM_COMMIT}/variablefont/"
    "MaterialSymbolsOutlined%5BFILL%2CGRAD%2Copsz%2Cwght%5D.ttf"
)
UPSTREAM_CODEPOINTS_SHA256 = "cbea7bfbd34d1d4f8dd2628c34587e447f935cf4f2219b264988da48736eca75"
UPSTREAM_SHA256 = "9370e7137b1a952fb00c5caf770291ea24be807dc0940983f0e3fd81dab0c054"


def verify_sha256(data: bytes, expected: str, label: str) -> None:
    """Fail loudly when a downloaded upstream file doesn't match its pinned digest."""
    import hashlib

    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        print(
            f"upstream {label} did not match the pinned digest:\n"
            f"  expected {expected}\n"
            f"  actual   {actual}",
            file=sys.stderr,
        )
        raise SystemExit(1)


def load_codepoints(src: Path) -> dict[str, int]:
    """Upstream name -> codepoint, from the `.codepoints` file beside the font."""
    cp = src.with_suffix(".codepoints")
    if not cp.exists():
        with urllib.request.urlopen(UPSTREAM_CODEPOINTS) as r:  # nosec B310 - fixed https URL
            data = r.read()
        verify_sha256(data, UPSTREAM_CODEPOINTS_SHA256, "codepoints file")
        cp.write_bytes(data)
    out = {}
    for line in cp.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if len(parts) == 2:
            out[parts[0]] = int(parts[1], 16)
    return out
