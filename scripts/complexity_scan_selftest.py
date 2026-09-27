"""complexity-scan.py --self-test: fixtures that must score on opposite sides of the gate."""

from complexity_scan_extract import extract_functions
from complexity_scan_mask import mask_rust_literals_and_comments
from complexity_scan_metrics import GATE, _hash_blank, cognitive_complexity, cyclomatic_complexity


# ============================================================================
# --self-test: prove the scanner against two fixtures that MUST score on opposite sides of
# the gate - same convention as check-render-sanity.ps1 / check-prebuild-coverage.ps1's
# -ProveItFails, so a scanner that silently stopped parsing (masking broke, extraction
# found zero functions, ...) is caught rather than read as "clean".
# ============================================================================

_TRIVIAL_FN = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"

# Byte-identical to check-complexity.ps1's -ProveItFails fixture - verified there against a
# REAL odin run (2026-09-05): cognitive 174, cyclomatic 30. Reusing the exact same source
# cross-validates this port against that real measurement instead of a number this script
# invented for itself.
_DEEP_FN = """fn deeply_nested(a: i32, b: i32, c: i32, d: i32, e: i32) -> i32 {
    let mut total = 0;
    if a > 0 {
        if b > 0 {
            if c > 0 {
                if d > 0 {
                    if e > 0 {
                        for i in 0..a {
                            if i % 2 == 0 {
                                if i % 3 == 0 {
                                    if i % 5 == 0 {
                                        while total < b {
                                            match i {
                                                0 => total += 1,
                                                1 => total += 2,
                                                2 => total += 3,
                                                3 => total += 4,
                                                4 => total += 5,
                                                5 => total += 6,
                                                6 => total += 7,
                                                7 => total += 8,
                                                8 => total += 9,
                                                9 => total += 10,
                                                _ => {
                                                    if total > 100 {
                                                        if total > 200 {
                                                            if total > 300 {
                                                                if total > 400 {
                                                                    break;
                                                                } else { total += 1; }
                                                            } else { total += 2; }
                                                        } else { total += 3; }
                                                    } else { total += 4; }
                                                }
                                            }
                                        }
                                    } else if e < -5 { total -= 1; } else { total -= 2; }
                                } else if d < -5 { total -= 3; } else { total -= 4; }
                            } else if c < -5 { total -= 5; } else { total -= 6; }
                        }
                    } else if b < -5 { total -= 7; } else { total -= 8; }
                } else if a < -5 { total -= 9; } else { total -= 10; }
            } else { total -= 11; }
        } else { total -= 12; }
    } else { total -= 13; }
    total
}
"""

# A minimal fixture proving the try-operator ("?") rule this whole gate exists to
# document: FIVE fallible calls chained with `?` inside one flat match arm dispatcher have
# no nesting at all, yet still cross the gate on `?` alone (CLAUDE.md / DEVELOPMENT_
# GOTCHAS.md "it scores every `?` as a branch").
_TRY_OPERATOR_FN = "fn dispatch(x: u8) -> Result<(), E> {\n" + "\n".join(
    f"    if x == {i} {{ step{i}()?; }}" for i in range(1, 32)
) + "\n    Ok(())\n}\n"


def _self_test():
    failures = 0

    def check(label, fixture, expect_over_gate):
        masked = mask_rust_literals_and_comments(fixture)
        fns = extract_functions(masked)
        if len(fns) != 1:
            print(f"  FAIL  {label}: expected exactly 1 function extracted, got {len(fns)}")
            return False
        clean = _hash_blank(fns[0]["body"])
        cyc = cyclomatic_complexity(clean)
        cog = cognitive_complexity(clean)
        over = cyc >= GATE or cog >= GATE
        ok = over == expect_over_gate
        status = "PASS" if ok else "FAIL"
        print(f"  {status}  {label}: cyclomatic={cyc} cognitive={cog} (expected over-gate={expect_over_gate})")
        return ok

    if not check("trivial function stays clean", _TRIVIAL_FN, False):
        failures += 1
    if not check("deeply nested function crosses the gate", _DEEP_FN, True):
        failures += 1
    if not check("chained try-operator crosses the gate on '?' alone", _TRY_OPERATOR_FN, True):
        failures += 1

    # Cross-validate the deep fixture's EXACT numbers against the real odin measurement
    # recorded in check-complexity.ps1's own -ProveItFails comment (2026-09-05: cognitive
    # 174, cyclomatic 30) - a byte-for-byte port should reproduce them exactly, not merely
    # land on the same side of the gate.
    masked = mask_rust_literals_and_comments(_DEEP_FN)
    fns = extract_functions(masked)
    if fns:
        clean = _hash_blank(fns[0]["body"])
        cyc = cyclomatic_complexity(clean)
        cog = cognitive_complexity(clean)
        if (cyc, cog) == (30, 174):
            print("  PASS  deep fixture matches the real odin measurement exactly (cyc=30, cog=174)")
        else:
            print(f"  FAIL  deep fixture diverges from the real odin measurement: got cyc={cyc} cog={cog}, expected cyc=30 cog=174")
            failures += 1

    if failures:
        print(f"complexity-scan --self-test: FAILED - {failures} case(s) did not match expectations.")
        return 1
    print("complexity-scan --self-test: OK - clean/over-gate/try-operator fixtures all scored as expected.")
    return 0
