#!/usr/bin/env python3
r"""Look at every rendered thumbnail and ask whether it is a BROKEN PICTURE.

WHY THIS EXISTS, and what it is NOT. Every other gate in this repo asks a question that a
wrong picture can answer correctly:

  * `regression.ps1`'s sweep asks "did a non-empty PNG appear".
  * `verify.ps1 -Samples` asks "did st2k exit 0 and write a file".
  * `_expected-colors.txt` asks the right question, but only about ~18 hand-curated samples.
  * `check-render-parity.ps1` asks "did the picture CHANGE since the last release", so it is
    blind to anything that has been wrong since the day it was written.

That gap shipped twice, and both shipped bugs were sitting in the corpus the whole time:

  * `.indd` — a spliced-together JPEG that decoded to a few correct rows of page and then flat
    grey. Reported from the field on 2.3.0; `test-corpus/sample.indd` had rendered it that way
    since June.
  * `.dcr` — a Kodak RAW that thumbnailed pure BLACK because the first decode tier answered
    from a near-black placeholder IFD. ImageMagick decoded the same file to a real photo.

Both have a machine-checkable signature, and neither needs a per-sample expectation:

  TRUNCATED   the bottom of the image is a flat band of the neutral fill colour a JPEG/WIC
              decoder leaves where it ran out of data, over content that is NOT flat.
  UNJUDGED    (reported, never failed on) our render carries no detail AND no independent
              decoder can read the format, so nothing here can say whether it should have.
  FEATURELESS our render carries essentially no detail while an INDEPENDENT decoder
              (ImageMagick, which shares no code with us) gets real detail out of the file.

Usage:
    python scripts/render-sanity.py --corpus ..\test-corpus --rendered ..\test-corpus\_render
                                    [--magick <path>] [--allow <manifest>] [--report-only]

EXIT CODES
    0  nothing suspect (or --report-only)
    1  at least one render carries a broken-picture signature
    2  cannot run (no Pillow, or no renders to look at)

A finding here is not automatically a bug in this repo — a degenerate corpus sample produces
one too. Read it, then either fix the decoder or add the sample to the allow manifest WITH a
reason. Never silence one by loosening a threshold.
"""

import argparse
import os
import subprocess
import sys
import tempfile

try:
    from PIL import Image, ImageStat
except ImportError:
    print("render-sanity: needs Pillow (pip install -r scripts/requirements-dev.txt) — SKIPPED",
          file=sys.stderr)
    sys.exit(2)

# ── TRUNCATED ────────────────────────────────────────────────────────────────────────────
# A decoder that runs out of data mid-image leaves the rest at the neutral fill: mid-grey,
# because an all-zero YCbCr block is Y=0/Cb=Cr=128 rendered as ~128,128,128. Requiring the
# band to BE that colour is what keeps this free of false alarms — a picture that genuinely
# ends in flat black, flat white or a flat dark UI panel (the corpus has all three) does not
# match, and on 364 corpus renders the only hits were the two InDesign samples.
FILL_GREY = 128
FILL_TOLERANCE = 8  # how far each channel may sit from FILL_GREY
FILL_NEUTRALITY = 6  # how far the channels may sit from EACH OTHER
MIN_TAIL = 0.25  # a quarter of the image lost is unambiguous; the .indd bug lost half

# ── FEATURELESS ──────────────────────────────────────────────────────────────────────────
# "No detail" has to be judged against something, and the only honest something is a decoder
# that shares no code with ours. The gap between the two thresholds is deliberately wide: a
# render at sd < 2 is flat to the eye, and an independent decode at sd > 15 has real structure,
# so nothing lands in between by accident.
OURS_FLAT_SD = 2.0
THEIRS_DETAILED_SD = 15.0

# ── UNJUDGED ────────────────────────────────────────────────────────────────────────
# FEATURELESS can only speak when ImageMagick reads the format too, and the formats this
# project exists FOR are largely the ones it cannot. So a render with no detail in a format
# magick declines is not a finding here - there is nothing honest to compare it against - but
# it is not nothing either. This is exactly where a blank tile hides: 2.3.1 fixed a DjVu whose
# every page past ~4267 px came back a flat grey rectangle, and this gate could not have seen
# it, because magick has no DjVu delegate.
#
# It is REPORTED, never failed on, and deliberately carries no allow-list. Every one of these
# in the corpus was checked by hand on 2026-08-21 and all were legitimate: solid-colour
# fixtures, a 381-byte RAR that really does hold two tiny images, and document previews that
# average to a blank page once shrunk to 96 px. Turning that into a 30-line allow-list would
# cost the gate the thing that makes it worth running - that it needs no curated expectation
# per sample - and would give somebody a place to silence a real one. A number that grows is
# the signal; go look at what joined it.


def channel_sd(path):
    """Largest per-channel standard deviation, or None if the file will not open."""
    try:
        with Image.open(path) as im:
            return max(ImageStat.Stat(im.convert("RGB")).stddev)
    except Exception:
        return None


def flat_tail(path):
    """(fraction of trailing rows that are one flat colour, that colour) for an image whose
    top is NOT flat. Returns (0.0, None) for a wholly flat image — that is a different
    question, and plenty of corpus samples are flat by design."""
    try:
        with Image.open(path) as opened:
            im = opened.convert("RGB")
            width, height = im.size
            raw = im.tobytes()  # 3 bytes per pixel, row-major
    except Exception:
        return 0.0, None
    if width == 0 or height == 0:
        return 0.0, None
    stride = width * 3

    def uniform(y):
        row = raw[y * stride : (y + 1) * stride]
        first = row[:3]
        # A row of one colour IS that pixel repeated — one comparison, no per-pixel loop.
        return tuple(first) if row == first * width else None

    tail, colour = 0, None
    for y in range(height - 1, -1, -1):
        found = uniform(y)
        if found is None:
            break
        if colour is None:
            colour = found
        elif found != colour:
            break
        tail += 1
    if tail == height:
        return 0.0, None  # wholly flat: not a truncation, see above
    return tail / height, colour


def is_fill_grey(colour):
    if colour is None:
        return False
    return all(abs(c - FILL_GREY) <= FILL_TOLERANCE for c in colour) and (
        max(colour) - min(colour) <= FILL_NEUTRALITY
    )


def independent_sd(magick, sample, scratch):
    """Standard deviation of an INDEPENDENT decode, or None when ImageMagick declines the
    format (which is most of what this repo exists for, so it is a skip, not a failure)."""
    if not magick:
        return None
    out = os.path.join(scratch, "independent.png")
    if os.path.exists(out):
        os.remove(out)
    try:
        subprocess.run(
            [magick, sample + "[0]", "-resize", "256x256", out],
            capture_output=True,
            timeout=60,
        )
    except Exception:
        return None
    return channel_sd(out) if os.path.exists(out) else None


def load_allow(path):
    """`filename  # reason` per line. A bare filename with no reason is accepted but the
    caller is told, because an unexplained allow entry is how a gate rots."""
    allow = {}
    if not path or not os.path.exists(path):
        return allow
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            name, _, reason = line.partition("#")
            allow[name.strip()] = reason.strip()
    return allow


def render_for(rendered_dir, sample):
    stem, ext = os.path.splitext(sample)
    return os.path.join(rendered_dir, f"{stem}_{ext.lstrip('.')}.png")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--rendered", required=True)
    ap.add_argument("--magick")
    ap.add_argument("--allow")
    ap.add_argument("--report-only", action="store_true")
    args = ap.parse_args()

    if not os.path.isdir(args.rendered):
        print(f"render-sanity: no renders at {args.rendered} — SKIPPED", file=sys.stderr)
        return 2

    allow = load_allow(args.allow)
    scratch = tempfile.mkdtemp(prefix="st2k-sanity-")
    findings, checked, cross_checked = [], 0, 0
    unjudged = []

    samples = sorted(
        f
        for f in os.listdir(args.corpus)
        if os.path.isfile(os.path.join(args.corpus, f))
        and not f.startswith("_")
        and f not in ("contact.png", "README.md")
        and not f.endswith(".lnk")
    )
    for sample in samples:
        ours = render_for(args.rendered, sample)
        if not os.path.exists(ours):
            continue
        checked += 1
        if sample in allow:
            continue

        tail, colour = flat_tail(ours)
        if tail >= MIN_TAIL and is_fill_grey(colour):
            findings.append(
                (
                    "TRUNCATED",
                    sample,
                    f"bottom {tail:.0%} is flat rgb{colour} — the decoder's no-data fill, "
                    "so the picture stops partway down",
                )
            )
            continue

        ours_sd = channel_sd(ours)
        if ours_sd is not None and ours_sd < OURS_FLAT_SD:
            theirs_sd = independent_sd(args.magick, os.path.join(args.corpus, sample), scratch)
            if theirs_sd is None:
                unjudged.append(sample)
            else:
                cross_checked += 1
                if theirs_sd > THEIRS_DETAILED_SD:
                    findings.append(
                        (
                            "FEATURELESS",
                            sample,
                            f"our render has no detail (sd {ours_sd:.2f}) but an independent "
                            f"decoder gets a real picture out of it (sd {theirs_sd:.2f})",
                        )
                    )

    print(f"[sanity] {checked} renders checked, {cross_checked} cross-checked against ImageMagick")
    if unjudged:
        print(
            f"[sanity] {len(unjudged)} flat render(s) no independent decoder can judge "
            "(reported, not failed):"
        )
        print("           " + " ".join(unjudged))
    if allow:
        unexplained = [n for n, reason in allow.items() if not reason]
        print(f"[sanity] {len(allow)} allow-listed", end="")
        print(f", {len(unexplained)} with NO reason: {' '.join(unexplained)}" if unexplained else "")

    if not findings:
        print("[sanity] no broken-picture signatures")
        return 0

    print(f"\n[sanity] {len(findings)} suspect render(s):")
    for kind, sample, why in findings:
        print(f"  {kind:12s} {sample:34s} {why}")
    print(
        "\n[sanity] Each of these is either a decoder bug or a degenerate corpus sample.\n"
        "         Fix the decoder, or add the sample to the allow manifest WITH a reason.\n"
        "         Do NOT loosen a threshold to make one go away."
    )
    return 0 if args.report_only else 1


if __name__ == "__main__":
    sys.exit(main())
