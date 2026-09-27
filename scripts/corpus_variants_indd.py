"""Adobe InDesign authoring variants: the XMP preview elements (see corpus-variants.py)."""


# ---- Adobe InDesign ------------------------------------------------------------------------------
_B64 = set(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=\r\n \t")


def indd_variants(b):
    """The preview is a base64 JPEG inside `<xmpGImg:image>` elements of the XMP packet, and an
    .indd is a block database that leaves EARLIER packets behind, half-overwritten - so the
    variants that matter are how many elements there are and whether each is contiguous with
    its close tag (a fragmented element is the "top strip only" field bug of the reader)."""
    v = set()
    if b[:16] != bytes.fromhex("0606edf5d81d46e5bd31efe7fe74b71d"):
        return v
    v.add("document")
    packets = b.count(b"<x:xmpmeta")
    v.add("several-xmp-packets" if packets > 1 else ("one-xmp-packet" if packets == 1 else "no-xmp-packet"))
    opens, contiguous, fragmented, whole = _indd_scan_elements(b)
    v.add("has-xmp-preview" if opens else "no-xmp-preview")
    if opens:
        v.add("contiguous-element" if contiguous else "no-contiguous-element")
        v.add("fragmented-elements" if fragmented else "no-fragmented-elements")
        v.add("whole-jpeg" if whole else "no-whole-jpeg")
    return v


def _indd_scan_elements(b):
    """(opens, contiguous, fragmented, whole) over the `<xmpGImg:image>` elements in b."""
    opens = contiguous = fragmented = whole = 0
    pos = 0
    while True:
        at = b.find(b"<xmpGImg:image>", pos)
        if at == -1 or opens >= 64:
            break
        opens += 1
        s = at + len(b"<xmpGImg:image>")
        j = _indd_base64_end(b, s)
        if b[j:j + len(b"</xmpGImg:image>")] == b"</xmpGImg:image>":
            contiguous += 1
            if _indd_whole_jpeg(b[s:j]):
                whole += 1
        else:
            fragmented += 1
        pos = j
    return opens, contiguous, fragmented, whole


def _indd_base64_end(b, j):
    """First index past the base64 run at b[j:] (InDesign breaks it into lines with &#xA;)."""
    while j < len(b):
        if b[j] in _B64:
            j += 1
        elif b[j:j + 5] == b"&#xA;":
            j += 5
        else:
            break
    return j


def _indd_whole_jpeg(run):
    # FFD8FF opens as /9j/; the EOI FFD9 closes as /9k= , 2Q== or /Z by alignment.
    run = run.replace(b"&#xA;", b"")
    return run.startswith(b"/9j/") and run.rstrip().endswith((b"/9k=", b"2Q==", b"/Z"))


INDD_WANTED = {"document", "one-xmp-packet", "several-xmp-packets", "has-xmp-preview",
               "no-xmp-preview", "contiguous-element", "fragmented-elements", "whole-jpeg"}
