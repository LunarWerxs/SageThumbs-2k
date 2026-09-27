"""Instancing, subsetting, optical normalization and renaming of the icon font (see
build-icon-font.py)."""

from __future__ import annotations

import io
from pathlib import Path

from icon_font_spec import INSTANCE, NORM_CENTER, NORM_MAX_SCALE, NORM_MIN_SCALE, NORM_TARGET, SCALE_GROUPS


def build_instance(src: Path, fill: int, unicodes: list[int], remap: dict[int, int] | None = None):
    """One instanced, subset font in memory."""
    from fontTools.subset import Options, Subsetter
    from fontTools.ttLib import TTFont
    from fontTools.varLib import instancer

    font = TTFont(str(src))
    instancer.instantiateVariableFont(font, {**INSTANCE, "FILL": fill}, inplace=True)
    opts = Options()
    opts.layout_features = []      # no ligatures: this app addresses glyphs by codepoint
    opts.name_IDs = ["*"]          # keep names so the face can be renamed below
    opts.notdef_outline = True     # a visible .notdef beats an invisible failure
    sub = Subsetter(options=opts)
    sub.populate(unicodes=unicodes)
    sub.subset(font)
    if remap:
        for table in font["cmap"].tables:
            table.cmap = {remap.get(cp, cp): g for cp, g in table.cmap.items()}
    buf = io.BytesIO()
    font.save(buf)
    buf.seek(0)
    return buf


def glyph_bounds(glyph_set, glyph_name):
    """(xMin, yMin, xMax, yMax) of a glyph's ink, or None when it draws nothing."""
    from fontTools.pens.boundsPen import BoundsPen

    pen = BoundsPen(glyph_set)
    glyph_set[glyph_name].draw(pen)
    return pen.bounds


def normalize_optical_sizes(font) -> list[tuple[int, str, float, int]]:
    """Scale each glyph about the grid centre so the set reads as one size. See NORM_* in icon_font_spec.py.

    Returns one (codepoint, glyph name, scale, new longest side) row per glyph, for the report
    the caller prints - a silent geometry pass is exactly the kind of change that should be
    visible in the build output.
    """
    from fontTools.misc.transform import Transform
    from fontTools.pens.recordingPen import DecomposingRecordingPen
    from fontTools.pens.transformPen import TransformPen
    from fontTools.pens.ttGlyphPen import TTGlyphPen

    cmap = font.getBestCmap()
    cx, cy = NORM_CENTER
    # Snapshot the glyph set ONCE, before any outline is replaced: every measurement and every
    # decompose below must see the ORIGINAL outlines, or a glyph read after its own rewrite
    # would be measured (or a component resolved) against already-scaled contours.
    gs = font.getGlyphSet()

    # Pass 1: the scale each glyph wants on its own.
    scales: dict[str, float] = {}
    for cp, name in cmap.items():
        b = glyph_bounds(gs, name)
        if b is None:
            continue
        longest = max(b[2] - b[0], b[3] - b[1])
        if longest <= 0:
            continue
        scales[name] = min(max(NORM_TARGET / longest, NORM_MIN_SCALE), NORM_MAX_SCALE)

    # Pass 2: force the grouped codepoints onto one shared scale (the smaller of the two, so a
    # grouped glyph can only ever come out at or under its own target - never overshoot it).
    for group in SCALE_GROUPS:
        names = [cmap[cp] for cp in group if cp in cmap and cmap[cp] in scales]
        if len(names) > 1:
            shared = min(scales[n] for n in names)
            for n in names:
                scales[n] = shared

    # Pass 3: rewrite the outlines. Decomposing first means a composite glyph is flattened to
    # contours rather than scaled twice (once as the component, once by its own transform).
    glyf = font["glyf"]
    report = []
    for cp, name in sorted(cmap.items()):
        b = glyph_bounds(gs, name)
        s = scales.get(name)
        if b is None or s is None:
            continue
        if abs(s - 1.0) >= 1e-9:
            rec = DecomposingRecordingPen(gs)
            gs[name].draw(rec)
            out = TTGlyphPen(None)
            # Translate to the grid centre, scale, translate back - so a glyph keeps the
            # position it was drawn at instead of drifting toward the origin as it grows.
            t = Transform().translate(cx, cy).scale(s, s).translate(-cx, -cy)
            rec.replay(TransformPen(out, t))
            glyf[name] = out.glyph()
            glyf[name].recalcBounds(glyf)
        report.append((cp, name, s, round(max(b[2] - b[0], b[3] - b[1]) * s)))
    return report


def rename_face(font, name: str) -> None:
    """Give the merged font its own family/full/PostScript name."""
    ps = name.replace(" ", "")
    for rec in font["name"].names:
        nid = rec.nameID
        if nid in (1, 3, 4, 6, 16):
            font["name"].setName(ps if nid == 6 else name, nid, rec.platformID, rec.platEncID, rec.langID)
