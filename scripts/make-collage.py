#!/usr/bin/env python3
"""Rebuild the Quick preview hero collage (assets/screenshots/preview-collage.png + site/img).

    python scripts/make-collage.py [path\\to\\SageThumbs2K.exe]

Why this is a script and not a hand-composited image: the previous collage was assembled by
hand, so nothing kept it honest. By 2026-08-24 it was advertising "316 formats" (the real
number was 334), and every window in it wore a caption toolbar that no longer exists. Neither
drift was visible to any check, because a PNG has no tests. Now the header count comes from
`st2k formats --json` and every panel is a live headless capture of the CURRENT build, so
re-running this after a UI change is the whole maintenance story.

Every panel is produced by the app's own `--shot --window preview` harness (builds the window
OFF-SCREEN and renders it with PrintWindow), so nothing appears on screen, nothing steals
focus, and this is safe to run at any time. Same guarantee as scripts/make-shots.ps1, which
produces the single-window assets; this one produces the composite.

Needs Pillow (`pip install pillow`).
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent

# Panel geometry. The capture is requested at this size through `--size`, so the window really
# is this shape rather than being scaled afterwards - text stays at its native rasterisation.
PANEL_W, PANEL_H = 1000, 640
GAP = 22
MARGIN = 26
HEADER_H = 150
BG = (13, 13, 13)


def find_exe(argv: list[str]) -> Path:
    """The app EXE: an explicit argument, then the configured target dir, then the install."""
    candidates: list[Path] = []
    if len(argv) > 1:
        candidates.append(Path(argv[1]))
    try:
        meta = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=True,
            ).stdout
        )
        candidates.append(Path(meta["target_directory"]) / "release" / "SageThumbs2K.exe")
    except Exception:
        pass
    candidates.append(ROOT / "target" / "release" / "SageThumbs2K.exe")
    candidates.append(Path(r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe"))
    for c in candidates:
        if c.is_file():
            return c
    sys.exit("SageThumbs2K.exe not found - build it, or pass its path as arg 1")


def format_count(exe: Path) -> int:
    """The LIVE format count, from the `st2k.exe` sitting BESIDE the app being screenshotted.

    The sibling requirement is the whole point and is not a convenience. An earlier version
    fell back to `C:\\Program Files\\SageThumbs2K\\st2k.exe` when the sibling was missing,
    which silently reads a DIFFERENT, possibly older install: the header would then advertise
    one build's format count over four panels captured from another. That is exactly the
    hand-maintained drift this script exists to end, so it refuses instead.

    It bites under the project's own normal build too: `cargo build --release --bin
    SageThumbs2K` produces no `st2k.exe` at all, so on a clean target dir the fallback would
    have fired every time.
    """
    cli = exe.with_name("st2k.exe")
    if not cli.is_file():
        sys.exit(
            f"no st2k.exe beside {exe}\n"
            "  The format count must come from the SAME build as the screenshots.\n"
            "  Build it (cargo build --release) or point arg 1 at an install that has both."
        )
    out = subprocess.run(
        [str(cli), "formats", "--json"], capture_output=True, text=True, check=True
    ).stdout
    return len(json.loads(out))


def demo_files(scratch: Path) -> dict[str, Path]:
    """The four documents the collage shows, written fresh so the picture is reproducible."""
    md = scratch / "release-notes.md"
    md.write_text(
        "# Project Nebula\n\n"
        "A note about **rendering**. SageThumbs previews Markdown the way GitHub shows it "
        "\u2014 headings, tables, fenced code, task lists and quotes, all offline.\n\n"
        "## Highlights\n\n"
        "- Headings, **bold**, *italic*, and `inline code`\n"
        "- Tables with real grid lines and shaded rows\n"
        "- Fenced code blocks, syntax-highlighted\n"
        "- Clickable links and an outline sidebar\n\n"
        "> Tip: tap **Space** on any file in Explorer for an instant, full-size preview.\n\n"
        "| Feature | Status | Since |\n"
        "| --- | --- | --- |\n"
        "| Email preview | Ready | v2.4 |\n"
        "| 3D-print models | Ready | v2.4 |\n"
        "| Window capture | Ready | v2.4 |\n"
        "| Light / dark per window | Ready | v2.4 |\n",
        encoding="utf-8",
    )

    eml = scratch / "Print files for review.eml"
    eml.write_text(
        "From: Ada Lovelace <ada@example.com>\r\n"
        "To: Alan Turing <alan@example.com>\r\n"
        "Cc: Grace Hopper <grace@example.com>\r\n"
        "Subject: Print files for review\r\n"
        "Date: Mon, 24 Aug 2026 09:14:00 +0000\r\n"
        "MIME-Version: 1.0\r\n"
        'Content-Type: multipart/mixed; boundary="b1"\r\n'
        "\r\n"
        "--b1\r\n"
        "Content-Type: text/plain; charset=utf-8\r\n"
        "\r\n"
        "Morning,\r\n"
        "\r\n"
        "The revised bracket is attached, along with the render I promised. The wall\r\n"
        "thickness is up to 2.4 mm now, so it should stop flexing under load.\r\n"
        "\r\n"
        "No rush on this - have a look whenever the week calms down.\r\n"
        "\r\n"
        "Ada\r\n"
        "--b1\r\n"
        'Content-Type: model/stl; name="bracket-v3.stl"\r\n'
        'Content-Disposition: attachment; filename="bracket-v3.stl"\r\n'
        "\r\n"
        "(binary)\r\n"
        "--b1\r\n"
        'Content-Type: image/png; name="render.png"\r\n'
        'Content-Disposition: attachment; filename="render.png"\r\n'
        "\r\n"
        "(binary)\r\n"
        "--b1--\r\n",
        encoding="utf-8",
        newline="",
    )

    # A real mesh from the test corpus if it is checked out, else a generated one so the
    # script still works from a bare clone.
    stl = scratch / "bracket-v3.stl"
    corpus = ROOT.parent / "test-corpus" / "cube.stl"
    if corpus.is_file():
        shutil.copyfile(corpus, stl)
    else:
        stl.write_text(_ascii_tetrahedron(), encoding="ascii")

    return {"md": md, "eml": eml, "stl": stl, "code": ROOT / "src" / "decode" / "mesh.rs"}


def _ascii_tetrahedron() -> str:
    """A four-face ASCII STL - the fallback when the test corpus is not checked out."""
    pts = [(0.0, 0.0, 60.0), (60.0, 0.0, 0.0), (0.0, 60.0, 0.0), (0.0, 0.0, 0.0)]
    faces = [(0, 1, 2), (0, 3, 1), (0, 2, 3), (1, 3, 2)]
    out = ["solid fallback"]
    for a, b, c in faces:
        out.append("  facet normal 0 0 0")
        out.append("    outer loop")
        for i in (a, b, c):
            out.append("      vertex %f %f %f" % pts[i])
        out.append("    endloop")
        out.append("  endfacet")
    out.append("endsolid fallback")
    return "\n".join(out) + "\n"


def shoot(exe: Path, doc: Path, out: Path, extra: list[str] | None = None) -> Image.Image:
    args = [
        str(exe),
        "--shot",
        str(out),
        "--window",
        "preview",
        "--file",
        str(doc),
        "--size",
        f"{PANEL_W}x{PANEL_H}",
        "--wait-ms",
        "500",
    ]
    args += extra or []
    res = subprocess.run(args, capture_output=True, text=True)
    if res.returncode != 0 or not out.is_file():
        sys.exit(f"capture of {doc.name} failed (exit {res.returncode}): {res.stderr.strip()}")
    img = Image.open(out).convert("RGB")
    # The harness sizes the WINDOW, so a capture can come back a few px off the request.
    return img if img.size == (PANEL_W, PANEL_H) else img.resize((PANEL_W, PANEL_H), Image.LANCZOS)


def font(name: str, size: int) -> ImageFont.FreeTypeFont:
    for candidate in (rf"C:\Windows\Fonts\{name}", name):
        try:
            return ImageFont.truetype(candidate, size)
        except OSError:
            continue
    return ImageFont.load_default()


def main() -> None:
    exe = find_exe(sys.argv)
    total = format_count(exe)
    print(f"collage: exe={exe}")
    print(f"  live format count = {total}")

    with tempfile.TemporaryDirectory(prefix="st2k_collage_") as tmp:
        scratch = Path(tmp)
        docs = demo_files(scratch)
        panels = [
            shoot(exe, docs["md"], scratch / "p1.png"),
            shoot(exe, docs["eml"], scratch / "p2.png"),
            shoot(exe, docs["stl"], scratch / "p3.png"),
            shoot(exe, docs["code"], scratch / "p4.png"),
        ]

        width = MARGIN * 2 + PANEL_W * 2 + GAP
        height = HEADER_H + MARGIN + PANEL_H * 2 + GAP
        canvas = Image.new("RGB", (width, height), BG)
        for i, panel in enumerate(panels):
            x = MARGIN + (i % 2) * (PANEL_W + GAP)
            y = HEADER_H + (i // 2) * (PANEL_H + GAP)
            canvas.paste(panel, (x, y))

        draw = ImageDraw.Draw(canvas)
        title = "Tap Space \u2014 preview anything, instantly"
        sub = (
            "photos \u00b7 Markdown \u00b7 code \u00b7 email \u00b7 3D models \u00b7 data \u00b7 "
            f"PDFs \u00b7 video \u00b7 fonts \u00b7 archives   |   {total} formats, nothing else installed"
        )
        f_title = font("segoeuib.ttf", 58)
        f_sub = font("segoeui.ttf", 27)
        tw = draw.textlength(title, font=f_title)
        sw = draw.textlength(sub, font=f_sub)
        draw.text(((width - tw) / 2, 26), title, font=f_title, fill=(245, 245, 245))
        draw.text(((width - sw) / 2, 98), sub, font=f_sub, fill=(150, 155, 162))

        png = ROOT / "assets" / "screenshots" / "preview-collage.png"
        webp = ROOT / "site" / "img" / "preview-collage.webp"
        png.parent.mkdir(parents=True, exist_ok=True)
        webp.parent.mkdir(parents=True, exist_ok=True)
        canvas.save(png, optimize=True)
        canvas.save(webp, quality=88, method=6)
        print(f"  {png}  ({png.stat().st_size:,} bytes)")
        print(f"  {webp}  ({webp.stat().st_size:,} bytes)")
        print(f"  size = {width}x{height}")


if __name__ == "__main__":
    main()
