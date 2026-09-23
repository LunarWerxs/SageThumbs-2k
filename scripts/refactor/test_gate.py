"""Contract tests for scripts/refactor/gate.py.

Only the pure parts are pinned here: the ERR error-line classifier, the lines()
path/line counter, and main()'s argv dispatch. Nothing here runs cargo, st2k,
magick, git, the network, or walks the corpus.
"""
import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

_SPEC = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("gate.py"))
gate = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(gate)


class ErrRegexTests(unittest.TestCase):
    def test_matches_error_and_warning_source_lines(self):
        text = (
            "src/main.rs:3:5: error: unused import\n"
            "crates/x/src/lib.rs:1: warning: dead_code\n"
            "tests/a.rs:9: error: nope\n"
        )
        self.assertEqual(
            gate.ERR.findall(text),
            [
                "src/main.rs:3:5: error",
                "crates/x/src/lib.rs:1: warning",
                "tests/a.rs:9: error",
            ],
        )

    def test_ignores_other_paths_and_indented_lines(self):
        # only src/, crates/, tests/ at column 0 classify as compiler diagnostics
        text = "docs/foo.rs:1: error: x\n    src/a.rs:1: error: x\ncargo: error: x\n"
        self.assertEqual(gate.ERR.findall(text), [])


class LinesTests(unittest.TestCase):
    def _run(self, content):
        with tempfile.TemporaryDirectory() as d:
            Path(d, "f.txt").write_text(content, encoding="utf-8")
            with mock.patch.object(gate, "ROOT", d):
                return gate.lines("f.txt")

    def test_counts_newlines(self):
        self.assertEqual(self._run("a\nb\nc\n"), 3)

    def test_empty_file_is_zero(self):
        self.assertEqual(self._run(""), 0)


class _Recorder:
    def __init__(self):
        self.calls = []

    def make(self, name):
        def f(*args):
            self.calls.append((name, args))
            return 0

        return f


class MainDispatchTests(unittest.TestCase):
    def setUp(self):
        self.rec = _Recorder()
        self.stack = contextlib.ExitStack()
        for name in ("clippy", "consistency", "commit", "tests"):
            self.stack.enter_context(mock.patch.object(gate, name, self.rec.make(name)))
        self.addCleanup(self.stack.close)

    def _main(self, argv):
        with mock.patch.object(sys, "argv", ["gate.py"] + argv):
            return gate.main()

    def test_defaults_to_clippy(self):
        self.assertEqual(self._main([]), 0)
        self.assertEqual(self.rec.calls, [("clippy", ())])

    def test_tests_passes_filter_through(self):
        self.assertEqual(self._main(["tests", "foo"]), 0)
        self.assertEqual(self.rec.calls, [("tests", ("foo",))])

    def test_commit_uses_plan_path(self):
        self.assertEqual(self._main(["commit", "plan.json"]), 0)
        self.assertEqual(self.rec.calls, [("commit", ("plan.json",))])

    def test_unknown_command_prints_usage_and_fails(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = self._main(["wibble"])
        self.assertEqual(code, 2)
        self.assertEqual(self.rec.calls, [])
        self.assertIn("usage: gate.py", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
