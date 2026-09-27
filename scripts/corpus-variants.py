"""Which AUTHORING VARIANTS of each container format the test corpus actually holds.

Issues #44 and #45 (2026-09-19) got through fifty-odd releases and every full-codebase pass
because every pass graded "does this file produce a thumbnail" against the corpus, and all
three Illustrator samples in it are PDF-compatible, single-artboard files. A file saved without
"Create PDF Compatible File" produces a thumbnail too - of Illustrator's notice page. The hole
was in the SAMPLE SET, by save-time option, and no amount of reading code finds a variant the
corpus never contains.

This is the instrument for that hole. For each format whose authoring options change what is
INSIDE the file, it classifies every corpus sample by those options and prints, per format, the
variants present and the variants with NO sample. Exit 0 always: the output is the report; a
gate that wants a number reads the `MISSING` lines.

    python scripts/corpus-variants.py            # both corpora
    python scripts/corpus-variants.py --json     # machine-readable
    python scripts/corpus-variants.py --gate     # exit 1 if a variant the committed baseline
                                                 # lists has DISAPPEARED (the ratchet: the
                                                 # corpus may only gain variants, never lose
                                                 # one); MISSING variants print, never fail
    python scripts/corpus-variants.py --write-baseline   # after adding samples, record them

The baseline is `scripts/corpus-variants-baseline.txt` (one `ext:variant` per line, committed).
`verify.ps1` runs the gate; a sample deleted by mistake fails there, not at the next report.

Variant definitions are deliberately about what the FILE holds, never about what SageThumbs does
with it, so the report stays true when the decoder changes.
"""
import json
import os
import sys
from collections import defaultdict
from pathlib import Path

from corpus_variants_families import FAMILIES, FAMILY_WANTED

# Not used here: test_corpus-variants.py loads THIS file by path and tests the classifiers
# (and two PSD helpers) through it, so they stay importable from here after the move.
from corpus_variants_indd import indd_variants
from corpus_variants_psd import PSD_WANTED, _psd_colour_variants, _psd_tagged_layer_count, psd_variants
from corpus_variants_tiff_heif import heif_variants, tiff_variants
from corpus_variants_vector import PDF_WANTED, ai_variants, eps_variants, pdf_variants

ROOT = Path(__file__).resolve().parent.parent.parent
CORPORA = [ROOT / "test-corpus", ROOT / "test-corpus-real"]


def read_head(p, n=4 << 20):
    with open(p, "rb") as f:
        return f.read(n)


BASELINE = Path(__file__).resolve().parent / "corpus-variants-baseline.txt"


def collect_present():
    """family -> variant -> sample names, over both corpora."""
    present = defaultdict(lambda: defaultdict(list))
    for corpus in CORPORA:
        if not corpus.is_dir():
            continue
        for p in sorted(corpus.iterdir()):
            ext = p.suffix.lower().lstrip(".")
            if ext not in FAMILIES or not p.is_file():
                continue
            fn, _, fam = FAMILIES[ext]
            try:
                for var in fn(read_head(p)):
                    present[fam][var].append(p.name)
            except Exception as e:  # a malformed sample is itself a finding
                present[fam][f"unparseable:{type(e).__name__}"].append(p.name)
    return present


def build_report(present):
    report = {}
    for fam, wanted in FAMILY_WANTED.items():
        have = present.get(fam, {})
        report[fam] = {
            "samples": sorted({n for names in have.values() for n in names}),
            "present": {k: sorted(v) for k, v in sorted(have.items())},
            "missing": sorted(w for w in wanted if w not in have),
        }
    return report


def print_report(report):
    for fam, r in report.items():
        print(f"== {fam}: {len(r['samples'])} sample(s)")
        for k, names in r["present"].items():
            print(f"   {k:24s} {len(names)}  {', '.join(names[:4])}{' ...' if len(names) > 4 else ''}")
        if r["missing"]:
            print(f"   MISSING {fam}: {', '.join(r['missing'])}")


def run_gate(have_now):
    """Exit 2 = NOT MEASURED, 1 = a baselined variant is gone, 0 = every baselined one present."""
    if not any(c.is_dir() for c in CORPORA):
        print("corpus-variants: NOT MEASURED - no test corpus on this machine")
        sys.exit(2)
    if not BASELINE.is_file():
        print(f"corpus-variants: no baseline at {BASELINE.name} (run --write-baseline)")
        sys.exit(2)
    expected = {l.strip() for l in BASELINE.read_text(encoding="utf-8").splitlines() if l.strip()}
    lost = sorted(expected - have_now)
    gained = sorted(have_now - expected)
    if gained:
        print(f"corpus-variants: {len(gained)} variant(s) present but not in the baseline - run --write-baseline: {', '.join(gained)}")
    if lost:
        print(f"corpus-variants: FAIL - {len(lost)} variant(s) the baseline lists are GONE from the corpus: {', '.join(lost)}")
        sys.exit(1)
    print(f"corpus-variants: ok - all {len(expected)} baselined variants present")


def main():
    report = build_report(collect_present())
    if "--json" in sys.argv:
        print(json.dumps(report, indent=2))
        return
    have_now = {f"{fam}:{k}" for fam, r in report.items() for k in r["present"] if not k.startswith("unparseable:")}
    if "--write-baseline" in sys.argv:
        BASELINE.write_text("\n".join(sorted(have_now)) + "\n", encoding="utf-8")
        print(f"wrote {len(have_now)} present variants to {BASELINE.name}")
        return
    print_report(report)
    if "--gate" in sys.argv:
        run_gate(have_now)


if __name__ == "__main__":
    main()
