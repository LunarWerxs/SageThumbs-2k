import importlib.util
from io import StringIO
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

cr_module = None


def setUpModule():
    global cr_module
    try:
        import PIL
        from PIL import Image
    except ImportError as e:
        raise unittest.SkipTest(f"PIL is not available: {e}")

    target_path = Path(__file__).with_name("compare-renders.py")
    spec = importlib.util.spec_from_file_location("compare_renders", target_path)
    cr_module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cr_module)


class TestCompareRenders(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if cr_module is None:
            raise unittest.SkipTest("compare_renders module failed to load or PIL missing")

    def test_as_8bit_scales_16bit_to_8bit(self):
        from PIL import Image
        im16 = Image.new("I;16", (4, 4), color=32768)
        im8 = cr_module.as_8bit(im16)
        # 32768 * (1 / 256) = 128
        self.assertEqual(im8.getpixel((0, 0)), 128)

        # 8-bit image is returned unchanged
        im_rgb = Image.new("RGB", (4, 4), color=(10, 20, 30))
        self.assertIs(cr_module.as_8bit(im_rgb), im_rgb)

    def test_centre_returns_rgba_pixel(self):
        from PIL import Image
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "sample.png"
            Image.new("RGB", (10, 10), color=(12, 34, 56)).save(path)
            pixel = cr_module.centre(str(path))
            self.assertEqual(pixel, (12, 34, 56, 255))

    def test_mean_delta_identical_and_different(self):
        from PIL import Image
        with tempfile.TemporaryDirectory() as tmpdir:
            path_a = Path(tmpdir) / "a.png"
            path_b = Path(tmpdir) / "b.png"
            path_c = Path(tmpdir) / "c.png"

            Image.new("RGB", (8, 8), color=(100, 100, 100)).save(path_a)
            Image.new("RGB", (8, 8), color=(100, 100, 100)).save(path_b)
            Image.new("RGB", (8, 8), color=(200, 200, 200)).save(path_c)

            self.assertEqual(cr_module.mean_delta(str(path_a), str(path_b)), 0.0)
            delta = cr_module.mean_delta(str(path_a), str(path_c))
            # RGB channels differ by 100, alpha is 0 diff: (100 * 3 + 0) / 4 = 75.0
            self.assertAlmostEqual(delta, 75.0, places=1)

    def test_classify_pair(self):
        self.assertEqual(
            cr_module.classify_pair("f1", "ok", "none", None, 2.0),
            ("lost", ("f1", "none")),
        )
        self.assertEqual(
            cr_module.classify_pair("f2", "none", "ok", None, 2.0),
            ("gained", ("f2", "none")),
        )
        self.assertEqual(
            cr_module.classify_pair("f3", "none", "none", None, 2.0),
            ("skip", None),
        )
        self.assertEqual(
            cr_module.classify_pair("f4", "ok", "ok", "corrupt file", 2.0),
            ("error", ("f4", "corrupt file")),
        )
        self.assertEqual(
            cr_module.classify_pair("f5", "ok", "ok", 1.5, 2.0),
            ("same", None),
        )
        self.assertEqual(
            cr_module.classify_pair("f6", "ok", "ok", 2.0, 2.0),
            ("changed", ("f6", 2.0)),
        )
        self.assertEqual(
            cr_module.classify_pair("f7", "ok", "ok", 5.0, 2.0),
            ("changed", ("f7", 5.0)),
        )

    def test_load_expected_ignores_comments_and_blanks(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "expected.txt"
            content = (
                "# Comment line\n"
                "\n"
                "foo.xcf\t10,20,30\n"
                "  bar.xcf  \t  255, 0 , 128  \n"
                "# Another comment\n"
            )
            path.write_text(content, encoding="utf-8")
            loaded = cr_module.load_expected(str(path))
            self.assertEqual(
                loaded,
                {
                    "foo.xcf": (10, 20, 30),
                    "bar.xcf": (255, 0, 128),
                },
            )

    def test_validate_args_requires_mode_and_out(self):
        ap = cr_module.build_arg_parser()

        # Missing both --old and --expect
        args = ap.parse_args(["--corpus", "c"])
        with patch("sys.stderr", new_callable=StringIO):
            with self.assertRaises(SystemExit):
                cr_module.validate_args(ap, args)

        # Differential mode without --out
        args = ap.parse_args(["--corpus", "c", "--old", "st2k_old.exe", "--new", "st2k_new.exe"])
        with patch("sys.stderr", new_callable=StringIO):
            with self.assertRaises(SystemExit):
                cr_module.validate_args(ap, args)

        # Valid differential mode
        args = ap.parse_args([
            "--corpus", "c",
            "--old", "st2k_old.exe",
            "--new", "st2k_new.exe",
            "--out", "out_dir",
        ])
        cr_module.validate_args(ap, args)


if __name__ == "__main__":
    unittest.main()
