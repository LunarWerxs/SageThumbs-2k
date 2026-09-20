You are refactoring ONE Rust file in SageThumbs 2K, a Windows shell extension (the cwd is the repo root; the file is UTF-8 with LF line endings). The repo's complexity scanner flags these functions in `{file}`:
{rows}

Run the scanner with: python scripts/complexity-scan.py --root . --json --warnings
It takes about 6 s and prints a JSON list of every function in the repo scoring 15..29, as objects {{score, file, line, function, metric}} with metric "cog" (cognitive) or "cyc" (cyclomatic). Filter for "file": "{file}".

GOAL: take each listed function BELOW 15 on the metric named, without changing behaviour, by extracting a helper along a NATURAL seam, and without creating any new function in this file that scores 15 or more on either metric.

What the scanner charges (measured on this repo): every `?` is a branch; a `?` inside a loop or inside a closure also pays the nesting penalty, so lift the loop or closure body into a free function at nesting zero (there each `?` costs 1 and the loop costs 2); nested `if let` / `match` under a loop is the other usual seam; two arms or two blocks that spell out the same steps become one helper called twice. A dispatcher's cyclomatic score IS its arm count (a `match` with 20 arms scores 20): leave those and say so.

RULES:
- Behaviour must be IDENTICAL: same results, same order of side effects, same error paths, same strings. Copy bodies verbatim into the helper; pass what it needs by reference (`&`, `&mut`) with explicit types; keep the helper private (`fn`, never `pub`) in the same file, directly below the function it serves, with a one-line `///` doc comment saying what it does. Early `return`s, `break`s and `continue`s inside the moved block must keep their meaning (return a bool or an Option from the helper and act on it in the caller).
- Only split along a seam a reader would NAME (a loop body, a validation block, a per-arm routine, a "build X" block). If the only way under 15 is to cut mid-thought, do NOT split: report that function as skipped with the reason.
- Prefer ONE extraction per listed function, and it must earn its name: never a helper that only wraps a single call or a single `if` (a fn that returns `Some(None)` unless a tag matches is the shape to avoid). If one lift is not enough, lift a BIGGER block (the whole loop body, the whole arm) rather than adding a second thin helper.
- Skip any function that is a `#[test]`, lives under `#[cfg(test)]`, or is a fuzz driver.
- Do not touch any other file. Do not reorder, rename or reformat anything else in this file. Never run cargo, rustc or rustfmt: the author runs clippy and the tests over the whole batch, and concurrent cargo runs collide on the target dir.
- Keep the `use` lines as they are unless the helper needs an item the file does not already import.
- VERIFY after editing: run the scanner once and check (a) none of the listed functions still appears for this file at 15 or more on its metric, and (b) no NEW function from this file appears. If a split made the band worse, undo it (restore the original text) and report skipped. Run the scanner at most twice.

Answer through submit_result with: status (done | partial | skipped), functions (one row per listed function: name, metric, before, after, action - what you did in one line), helpers_added (the new helper names), reason (one line per skipped function, or "" if none).
