"""Extension -> (classifier, wanted variants, family) for every format corpus-variants.py reports."""

from corpus_variants_indd import INDD_WANTED, indd_variants
from corpus_variants_psd import PSD_WANTED, psd_variants
from corpus_variants_raw import RAW_WANTED, raw_variants
from corpus_variants_tiff_heif import HEIF_WANTED, TIFF_WANTED, heif_variants, tiff_variants
from corpus_variants_vector import (AI_WANTED, CDR_WANTED, EPS_WANTED, PDF_WANTED, ai_variants, cdr_variants,
                                    eps_variants, pdf_variants)


FAMILIES = {
    "ai": (ai_variants, AI_WANTED, "ai"),
    "psd": (psd_variants, PSD_WANTED, "psd"),
    "psb": (psd_variants, PSD_WANTED, "psd"),
    "pdf": (pdf_variants, PDF_WANTED, "pdf"),
    "eps": (eps_variants, EPS_WANTED, "eps"),
    "tif": (tiff_variants, TIFF_WANTED, "tiff"),
    "tiff": (tiff_variants, TIFF_WANTED, "tiff"),
    "heic": (heif_variants, HEIF_WANTED, "heif"),
    "heif": (heif_variants, HEIF_WANTED, "heif"),
    "avif": (heif_variants, HEIF_WANTED, "heif"),
    "indd": (indd_variants, INDD_WANTED, "indd"),
    "indt": (indd_variants, INDD_WANTED, "indd"),
    "cdr": (cdr_variants, CDR_WANTED, "cdr"),
}
# Camera RAW is one family across many extensions: what matters is the container shape and
# the embedded preview, not the maker's suffix.
for _ext in ("cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "pef", "orf", "raf", "rw2",
             "dcr", "kdc", "fff", "3fr", "srw", "x3f", "mrw", "erf", "mos", "iiq"):
    FAMILIES[_ext] = (raw_variants, RAW_WANTED, "raw")

FAMILY_WANTED = {fam: wanted for _, wanted, fam in FAMILIES.values()}
