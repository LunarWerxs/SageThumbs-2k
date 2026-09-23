"""Tests for the pure parts of extract_items.py: the insert-point finders and `carve`,
which decides what stays in the hub and how a moved item's body is rewritten.

Nothing here runs cargo, rustfmt, git, or touches the real corpus: every input is a
list of Rust source lines built in memory.
"""
import contextlib
import importlib.util
import io
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = Path(__file__).with_name("extract_items.py")

# `import _rs` / `from _items import ...` at the top of the target are sibling modules,
# not third-party: make them importable however this test was launched.
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

extract_items = None
if extract_items is None:
    spec = importlib.util.spec_from_file_location("extract_items", SCRIPT)
    extract_items = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(extract_items)


class InsertPointTests(unittest.TestCase):
    def test_after_inner_docs_stops_at_first_code_line(self):
        # `//!` docs and inner attrs are not code; the first real line ends the header.
        keep = ["//! doc", "#![cfg(test)]", "", "fn x() {}"]
        self.assertEqual(extract_items.after_inner_docs(keep), 2)
        # an empty file has no header at all
        self.assertEqual(extract_items.after_inner_docs([]), 0)

    def test_after_use_block_returns_index_after_last_use(self):
        keep = ["use a::b;", "use c::d;", "", "fn x() {}"]
        self.assertEqual(extract_items.after_use_block(keep), 2)
        # no use block: nothing to anchor to
        self.assertIsNone(extract_items.after_use_block(["fn x() {}", ""]))

    def test_after_mod_block_skips_test_only_module_decls(self):
        # the `#[cfg(test)] mod tests;` at the tail must not drag the insert point down with it
        keep = ["mod foo;", "#[cfg(test)]", "mod tests;", "pub fn u() {}"]
        self.assertEqual(extract_items.after_mod_block(keep), 1)

    def test_insert_point_prefers_mod_block(self):
        keep = ["mod a;", "", "use x::y;"]
        self.assertEqual(extract_items.insert_point(keep), 1)
        # with no mod block it falls back to just past the use block
        self.assertEqual(extract_items.insert_point(["use x::y;", "", "fn f() {}"]), 1)


class CarveTests(unittest.TestCase):
    def test_carve_widens_visibility_and_deepens_super_paths(self):
        lines = [
            "use super::*;",              # 0 stays
            "",                            # 1 stays
            "fn foo() {",                 # 2 moves
            "    let x = super::bar();",  # 3 moves
            "}",                           # 4 moves
            "",                            # 5 rides along with the item
            "fn keep_me() {}",            # 6 stays
        ]
        found = [(2, 5, "fn foo")]
        keep, moved, total = extract_items.carve(lines, found)

        self.assertEqual(keep, ["use super::*;", "", "fn keep_me() {}"])
        self.assertEqual(
            moved,
            [
                "pub(super) fn foo() {",
                "    let x = super::super::bar();",  # one module deeper
                "}",
                "",                                   # the blank line after the item travels
            ],
        )
        self.assertEqual(total, 3)

    def test_carve_keeps_inline_mod_paths_unchanged(self):
        # an inline `mod x { use super::*; }` travels with its parent: its paths stay right
        lines = ["mod inner {", "    use super::*;", "}", "", "fn other() {}"]
        found = [(0, 3, "mod inner")]
        keep, moved, _ = extract_items.carve(lines, found)

        self.assertIn("    use super::*;", moved)      # NOT super::super::*
        self.assertNotIn("    use super::super::*;", moved)
        self.assertEqual(keep, ["fn other() {}"])

    def test_carve_of_empty_selection_is_a_noop(self):
        lines = ["fn a() {}", "", "fn b() {}"]
        keep, moved, total = extract_items.carve(lines, [])
        self.assertEqual(keep, lines)
        self.assertEqual(moved, [])
        self.assertEqual(total, 0)


class ListItemsTests(unittest.TestCase):
    def test_list_items_prints_one_line_per_item(self):
        parsed = [(0, 3, "fn foo"), (5, 7, "struct Bar")]
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            extract_items.list_items(parsed)
        out = buf.getvalue().splitlines()
        self.assertEqual(len(out), 2)
        self.assertIn("fn foo", out[0])
        self.assertIn("struct Bar", out[1])
        self.assertIn("1", out[0])   # start lines are 1-based


if __name__ == "__main__":
    unittest.main()
