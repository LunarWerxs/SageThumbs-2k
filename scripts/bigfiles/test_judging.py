"""The big-file gate's judge fails a twin that opens holes in its picture.

Its colour check alone passed a dense 3D scan drawn as see-through speckle (the shaded pixels
that were drawn kept their colours), so a twin's alpha is compared too."""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

judging = None


def setUpModule():
    global judging
    spec = importlib.util.spec_from_file_location("judging", Path(__file__).with_name("judging.py"))
    judging = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(judging)


class SeeThroughTwin(unittest.TestCase):
    def setUp(self):
        from PIL import Image

        self.dir = tempfile.mkdtemp(prefix=f"st2k_judge_{os.getpid()}_")
        # A solid shaded disc on a transparent ground, the way a mesh render looks.
        solid = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
        for y in range(64):
            for x in range(64):
                if (x - 32) ** 2 + (y - 32) ** 2 < 28 ** 2:
                    solid.putpixel((x, y), (90, 120, 200, 255))
        # The same disc with every other pixel see-through: the colours left are the same.
        holes = solid.copy()
        for y in range(64):
            for x in range(64):
                if (x + y) % 2:
                    r, g, b, _ = holes.getpixel((x, y))
                    holes.putpixel((x, y), (r, g, b, 0))
        self.solid, self.holes = (os.path.join(self.dir, n) for n in ("solid.png", "holes.png"))
        solid.save(self.solid)
        holes.save(self.holes)

    def tearDown(self):
        for p in (self.solid, self.holes):
            os.remove(p)
        os.rmdir(self.dir)

    def test_a_see_through_twin_fails(self):
        verdict, why = judging.judge("cli", (self.solid, 500, None), (self.holes, 500, None))
        self.assertEqual(verdict, "FAIL", why)
        self.assertIn("see-through", why)

    def test_the_same_picture_passes(self):
        verdict, why = judging.judge("cli", (self.solid, 500, None), (self.solid, 500, None))
        self.assertEqual(verdict, "PASS", why)


if __name__ == "__main__":
    unittest.main()
