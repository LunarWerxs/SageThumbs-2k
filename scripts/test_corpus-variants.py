"""Tests for scripts/corpus-variants.py - the pure classifiers and report builders.

Everything here feeds hand-built byte strings to the variant functions; nothing reads the
real corpus, runs a tool, or touches the baseline file. The one thing shared with the script
is its own module, loaded by path.
"""
import importlib.util
import struct
import unittest
from pathlib import Path

cv = None


def setUpModule():
    global cv
    target_path = Path(__file__).with_name("corpus-variants.py")
    spec = importlib.util.spec_from_file_location("corpus_variants", target_path)
    cv = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cv)


class TestCorpusVariants(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if cv is None:
            raise unittest.SkipTest("corpus_variants module failed to load")

    # ---- Illustrator ---------------------------------------------------------------------
    def test_ai_variants_classifies_pdf_and_pdf_half_boundary(self):
        # A non-Adobe-format file is not claimed at all.
        self.assertEqual(cv.ai_variants(b"GIF89a...."), set())

        # PostScript (non-PDF) samples are only 'postscript-based'.
        self.assertEqual(cv.ai_variants(b"%!PS-Adobe-3.0\n"), {"postscript-based"})

        # `pdf_half` is the PDF half before the private data; > 20000 means PDF-compatible.
        # Exactly 20000 is the first value that does NOT count.
        boundary = b"%PDF-1.4\n" + b"x" * (20_000 - len(b"%PDF-1.4\n"))
        self.assertEqual(len(boundary), 20_000)
        self.assertIn("no-pdf-compatible", cv.ai_variants(boundary))
        self.assertIn("pdf-compatible", cv.ai_variants(boundary + b"x"))

        # /Count > 1 flips single-artboard to multi-artboard, both still PDF-based.
        v = cv.ai_variants(b"%PDF-1.4\n/Count 3\ntrailer")
        self.assertIn("pdf-based", v)
        self.assertIn("multi-artboard", v)
        self.assertIn("no-pdf-compatible", v)

        # The private thumbnail marker adds its variant regardless of the PDF shape.
        self.assertIn("has-private-thumbnail", cv.ai_variants(b"%PDF-1.4\n%AI7_Thumbnail"))

    # ---- Photoshop -----------------------------------------------------------------------
    def _psd(self, ver=1, depth=8, mode=3, layers=b"", comp=b"\x00\x00", channels=3):
        hdr = (b"8BPS" + struct.pack(">H", ver) + b"\x00" * 6
               + struct.pack(">HIIHH", channels, 1, 1, depth, mode))
        body = struct.pack(">I", 0)  # colour mode data length
        body += struct.pack(">I", 0)  # image resources length
        body += (struct.pack(">Q", len(layers)) if ver == 2 else struct.pack(">I", len(layers)))
        body += layers
        body += comp
        return hdr + body

    def test_psd_variants_reads_layers_and_blank_composite(self):
        # Raw (uncompressed) uniform paper-white composite in an RGB file is 'composite-blank'.
        blank = self._psd(comp=struct.pack(">H", 0) + b"\xff\xff\xff")
        self.assertEqual(cv.psd_variants(blank),
                         {"psd", "8-bit", "rgb", "no-layers", "composite-blank"})

        # A non-uniform raw composite is real artwork, not blank.
        artwork = self._psd(comp=struct.pack(">H", 0) + b"\x01\x02\x03")
        self.assertIn("has-composite", cv.psd_variants(artwork))

        # Flat layer info: length then a signed i16 count. The count, not the length, is the tell.
        flat_layers = struct.pack(">I", 2) + struct.pack(">h", 2)
        self.assertIn("has-layers", cv.psd_variants(self._psd(layers=flat_layers)))

        # 16-bit files stash layers in an Lr16 tagged block when the flat count is zero.
        section = b"Lr16" + struct.pack(">I", 100) + struct.pack(">h", 5) + b"pad"
        self.assertEqual(cv._psd_tagged_layer_count(section, 4), 5)
        self.assertEqual(cv._psd_tagged_layer_count(b"nothing here", 4), 0)

    def test_psd_colour_variants_maps_version_depth_and_mode(self):
        self.assertEqual(cv._psd_colour_variants(1, 8, 3), {"psd", "8-bit", "rgb"})
        self.assertEqual(cv._psd_colour_variants(2, 16, 4), {"psb", "16-bit", "cmyk"})
        self.assertEqual(cv._psd_colour_variants(1, 32, 7), {"psd", "32-bit", "multichannel"})
        # Unknown depth/mode fall back to a labelled form rather than being dropped.
        self.assertEqual(cv._psd_colour_variants(1, 24, 99), {"psd", "24-bit", "mode-99"})

    # ---- PDF -----------------------------------------------------------------------------
    def test_pdf_variants_pages_and_features(self):
        self.assertEqual(cv.pdf_variants(b"not a pdf"), set())

        rich = (b"%PDF-1.4\n/Linearized 1\n/Count 2\n/Encrypt\n"
                b"/Subtype /Image\n/Font\n")
        self.assertEqual(cv.pdf_variants(rich),
                         {"multi-page", "encrypted", "has-images", "has-text", "linearized"})

        plain = cv.pdf_variants(b"%PDF-1.4\n")
        self.assertEqual(plain,
                         {"single-page", "not-encrypted", "no-images", "no-text", "not-linearized"})

    # ---- EPS -----------------------------------------------------------------------------
    def test_eps_variants_preview_kinds(self):
        self.assertIn("epsi-preview", cv.eps_variants(b"%!PS-Adobe\n%%BeginPreview\n"))
        self.assertIn("photoshop-preview", cv.eps_variants(b"%!PS\n%BeginPhotoshop\n"))
        self.assertIn("illustrator-thumbnail", cv.eps_variants(b"%!PS\n%AI7_Thumbnail\n"))
        self.assertEqual(cv.eps_variants(b"%!PS\n"), {"plain-ps", "no-preview"})

        # DOS EPS: TIFF preview fields at 20..28, WMF fields at 12..20.
        def dos(wmf, tiff):
            return (bytes([0xC5, 0xD0, 0xD3, 0xC6]) + b"\x00" * 8
                    + struct.pack("<II", *wmf) + struct.pack("<II", *tiff))

        self.assertEqual(cv.eps_variants(dos((0, 0), (100, 50))), {"dos-eps", "tiff-preview"})
        self.assertEqual(cv.eps_variants(dos((100, 50), (0, 0))), {"dos-eps", "wmf-only-preview"})
        self.assertEqual(cv.eps_variants(dos((0, 0), (0, 0))), {"dos-eps", "no-preview"})

    # ---- TIFF ----------------------------------------------------------------------------
    def test_tiff_variants_pages_and_tags(self):
        self.assertEqual(cv.tiff_variants(b"not a tiff"), set())

        def entry(tag):
            return struct.pack("<HHI", tag, 4, 1) + struct.pack("<I", 0)

        ifd1 = (struct.pack("<H", 3) + entry(37724) + entry(34665) + entry(259)
                + struct.pack("<I", 50))  # next IFD at 50
        ifd2 = struct.pack("<H", 1) + entry(256) + struct.pack("<I", 0)
        b = b"II*\x00" + struct.pack("<I", 8) + ifd1 + ifd2
        self.assertEqual(cv.tiff_variants(b),
                         {"multi-page", "photoshop-layers", "has-exif", "compressed-tag-present"})

    # ---- HEIF / AVIF ---------------------------------------------------------------------
    def test_heif_variants_brands(self):
        def ftyp(payload):
            return struct.pack(">I", 8 + len(payload)) + b"ftyp" + payload

        self.assertEqual(cv.heif_variants(b"not isobmff"), set())
        self.assertEqual(cv.heif_variants(ftyp(b"heic" + b"\x00\x00\x00\x00")),
                         {"heic", "still"})
        self.assertEqual(cv.heif_variants(ftyp(b"avif" + b"\x00\x00\x00\x00")),
                         {"avif", "still"})
        # 'avis' is both an AVIF brand and a sequence marker.
        self.assertEqual(cv.heif_variants(ftyp(b"avis")), {"avif", "sequence"})

    # ---- InDesign ------------------------------------------------------------------------
    def test_indd_variants_detects_preview_elements(self):
        self.assertEqual(cv.indd_variants(b"not an indd"), set())
        magic = bytes.fromhex("0606edf5d81d46e5bd31efe7fe74b71d")

        whole = magic + b"<x:xmpmeta" + b"<xmpGImg:image>/9j/AAAA/9k=</xmpGImg:image>"
        self.assertEqual(cv.indd_variants(whole),
                         {"document", "one-xmp-packet", "has-xmp-preview",
                          "contiguous-element", "no-fragmented-elements", "whole-jpeg"})

        # An element with no close tag is the fragmented "top strip only" case.
        fragmented = magic + b"<xmpGImg:image>/9j/AAAA<"
        v = cv.indd_variants(fragmented)
        self.assertIn("no-xmp-packet", v)
        self.assertIn("no-contiguous-element", v)
        self.assertIn("fragmented-elements", v)
        self.assertIn("no-whole-jpeg", v)

        # Line-wrapped base64 (InDesign's &#xA;) is skipped over, not treated as an end.
        wrapped = magic + b"<xmpGImg:image>/9j/&#xA;AAAA/9k=</xmpGImg:image>"
        self.assertIn("whole-jpeg", cv.indd_variants(wrapped))

    # ---- report formatter ----------------------------------------------------------------
    def test_build_report_lists_samples_and_missing(self):
        present = {"psd": {"rgb": ["a.psd"], "8-bit": ["a.psd"],
                           "unparseable:ValueError": ["bad.psd"]}}
        report = cv.build_report(present)
        self.assertEqual(report["psd"]["samples"], ["a.psd", "bad.psd"])
        self.assertEqual(report["psd"]["present"]["8-bit"], ["a.psd"])
        expected_missing = sorted(w for w in cv.PSD_WANTED if w not in present["psd"])
        self.assertEqual(report["psd"]["missing"], expected_missing)

        # A family with no samples at all reports every wanted variant as missing.
        empty = cv.build_report({})
        self.assertEqual(empty["pdf"]["samples"], [])
        self.assertEqual(empty["pdf"]["missing"], sorted(cv.PDF_WANTED))


if __name__ == "__main__":
    unittest.main()
