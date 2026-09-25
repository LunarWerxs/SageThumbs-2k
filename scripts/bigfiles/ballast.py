"""Grow a file past the size gates WITHOUT changing its picture, cheaply.

Every strategy adds bytes the format's readers skip - ballast - and writes the zeros as a
sparse range (NTFS FSCTL_SET_SPARSE), so a 5 GB twin of a 30 KB sample costs no disk and a
second to make. Two strategies write real bytes instead (`PHYSICAL`): a comment is not
zeros, and a video grown by repeating its content repeats real packets.

The strategies, and which files take which, are the gate's business (`bigfiles.py`); each one
here is a pure "src -> dst of `size` bytes, same picture" transform. Which formats tolerate
which is MEASURED (tailprobe.py and the gate's own normal-vs-big comparison), not assumed."""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from grow_archives import rar_last, sevenzip_gap, tar_last  # noqa: E402
from grow_docs import eps_postscript, ole_front, pdf_body  # noqa: E402
from grow_plain import (  # noqa: E402
    before_tail_tag, fits_hdu, isobmff_free, psd_layers, repeat, tail, xml_comment, zip_last,
)

STRATEGIES = {
    "tail": tail,
    "zip-last": zip_last,
    "psd-layers": psd_layers,
    "isobmff-free": isobmff_free,
    "before-tail-tag": before_tail_tag,
    "xml-comment": xml_comment,
    "fits-hdu": fits_hdu,
    "repeat": repeat,
    "pdf-body": pdf_body,
    "eps-postscript": eps_postscript,
    "ole-front": ole_front,
    "rar-last": rar_last,
    "7z-gap": sevenzip_gap,
    "tar-last": tar_last,
}


# Strategies that write real bytes: made once, at the smallest size only.
PHYSICAL = {"xml-comment", "repeat"}
