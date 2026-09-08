#!/bin/bash
set -euo pipefail

# Generate golden test outputs from DjVuLibre tools.
# Run once before implementation begins.
# Requires: djvudump, ddjvu, djvused, bzz (from djvulibre)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ASSETS="$ROOT/references/djvujs/library/assets"
OUT="$SCRIPT_DIR/golden"

echo "Generating golden outputs..."
echo "Assets: $ASSETS"
echo "Output: $OUT"

# --- Phase 1: IFF structure dumps ---
echo "=== IFF dumps ==="
for f in boy_jb2 boy chicken carte navm_fgbz colorbook; do
    djvudump "$ASSETS/${f}.djvu" > "$OUT/iff/${f}.dump" 2>&1
    echo "  $f.dump"
done
djvudump "$ASSETS/DjVu3Spec_bundled.djvu" > "$OUT/iff/djvu3spec_bundled.dump" 2>&1
echo "  djvu3spec_bundled.dump"
djvudump "$ASSETS/big-scanned-page.djvu" > "$OUT/iff/big_scanned_page.dump" 2>&1
echo "  big_scanned_page.dump"

# --- Phase 2: Document metadata ---
echo "=== Document metadata ==="

# Page counts
for f in navm_fgbz DjVu3Spec_bundled colorbook; do
    count=$(djvused "$ASSETS/${f}.djvu" -e 'n' 2>&1)
    echo "$count" > "$OUT/document/${f}_pagecount.txt"
    echo "  $f: $count pages"
done

# Page sizes for navm_fgbz (6 pages)
> "$OUT/document/navm_fgbz_sizes.txt"
for i in $(seq 1 6); do
    djvused "$ASSETS/navm_fgbz.djvu" -e "select $i; size" >> "$OUT/document/navm_fgbz_sizes.txt" 2>&1
done
echo "  navm_fgbz sizes"

# Page sizes for DjVu3Spec_bundled (first 10 pages)
> "$OUT/document/djvu3spec_bundled_sizes.txt"
for i in $(seq 1 10); do
    djvused "$ASSETS/DjVu3Spec_bundled.djvu" -e "select $i; size" >> "$OUT/document/djvu3spec_bundled_sizes.txt" 2>&1
done
echo "  djvu3spec_bundled sizes (first 10)"

# --- Phase 3: JB2 mask outputs (PBM) ---
echo "=== JB2 masks ==="
ddjvu -format=pbm -mode=mask -page=1 "$ASSETS/boy_jb2.djvu" "$OUT/jb2/boy_jb2_mask.pbm" 2>&1
echo "  boy_jb2_mask.pbm"

ddjvu -format=pbm -mode=mask -page=1 "$ASSETS/carte.djvu" "$OUT/jb2/carte_p1_mask.pbm" 2>&1
echo "  carte_p1_mask.pbm"

ddjvu -format=pbm -mode=mask -page=1 "$ASSETS/navm_fgbz.djvu" "$OUT/jb2/navm_fgbz_p1_mask.pbm" 2>&1
echo "  navm_fgbz_p1_mask.pbm"

ddjvu -format=pbm -mode=mask -page=2 "$ASSETS/DjVu3Spec_bundled.djvu" "$OUT/jb2/djvu3spec_p2_mask.pbm" 2>&1
echo "  djvu3spec_p2_mask.pbm"

ddjvu -format=pbm -mode=mask -page=1 "$ASSETS/DjVu3Spec_bundled.djvu" "$OUT/jb2/djvu3spec_p1_mask.pbm" 2>&1
echo "  djvu3spec_p1_mask.pbm"

# --- Phase 4: IW44 background/foreground outputs (PPM) ---
echo "=== IW44 images ==="
ddjvu -format=ppm -page=1 "$ASSETS/boy.djvu" "$OUT/iw44/boy_bg.ppm" 2>&1
echo "  boy_bg.ppm"

ddjvu -format=ppm -page=1 "$ASSETS/chicken.djvu" "$OUT/iw44/chicken_bg.ppm" 2>&1
echo "  chicken_bg.ppm"

# big-scanned-page: subsample to avoid huge file
ddjvu -format=ppm -page=1 -subsample=4 "$ASSETS/big-scanned-page.djvu" "$OUT/iw44/big_scanned_sub4.ppm" 2>&1
echo "  big_scanned_sub4.ppm"

# --- Phase 5: Full composite renders (PPM) ---
echo "=== Composite renders ==="
ddjvu -format=ppm -page=1 "$ASSETS/boy_jb2.djvu" "$OUT/composite/boy_jb2.ppm" 2>&1
echo "  boy_jb2.ppm"

ddjvu -format=ppm -page=1 "$ASSETS/carte.djvu" "$OUT/composite/carte_p1.ppm" 2>&1
echo "  carte_p1.ppm"

ddjvu -format=ppm -page=1 "$ASSETS/colorbook.djvu" "$OUT/composite/colorbook_p1.ppm" 2>&1
echo "  colorbook_p1.ppm"

ddjvu -format=ppm -page=5 "$ASSETS/DjVu3Spec_bundled.djvu" "$OUT/composite/djvu3spec_p5.ppm" 2>&1
echo "  djvu3spec_p5.ppm"

ddjvu -format=ppm -page=1 "$ASSETS/navm_fgbz.djvu" "$OUT/composite/navm_fgbz_p1.ppm" 2>&1
echo "  navm_fgbz_p1.ppm"

ddjvu -format=ppm -page=4 "$ASSETS/navm_fgbz.djvu" "$OUT/composite/navm_fgbz_p4.ppm" 2>&1
echo "  navm_fgbz_p4.ppm"

# Rotated page
ddjvu -format=ppm -page=1 "$ASSETS/boy_jb2_rotate90.djvu" "$OUT/composite/boy_jb2_rot90.ppm" 2>&1
echo "  boy_jb2_rot90.ppm"

# --- Phase 3.1: BZZ test vectors ---
echo "=== BZZ test vectors ==="
# Create known-content BZZ compressed files
echo -n "Hello, World! This is a BZZ test." > "$OUT/bzz/test_short.txt"
bzz -e "$OUT/bzz/test_short.txt" "$OUT/bzz/test_short.bzz"
echo "  test_short.bzz"

# Longer text for multi-block test
python3 -c "print('The quick brown fox jumps over the lazy dog. ' * 200)" > "$OUT/bzz/test_long.txt"
bzz -e "$OUT/bzz/test_long.txt" "$OUT/bzz/test_long.bzz"
echo "  test_long.bzz"

# Single byte
echo -n "X" > "$OUT/bzz/test_1byte.txt"
bzz -e "$OUT/bzz/test_1byte.txt" "$OUT/bzz/test_1byte.bzz"
echo "  test_1byte.bzz"

# Empty file
> "$OUT/bzz/test_empty.txt"
bzz -e "$OUT/bzz/test_empty.txt" "$OUT/bzz/test_empty.bzz" 2>/dev/null || true
echo "  test_empty.bzz"

echo ""
echo "=== Summary ==="
echo "IFF dumps:  $(ls "$OUT/iff/"*.dump 2>/dev/null | wc -l) files"
echo "Document:   $(ls "$OUT/document/"* 2>/dev/null | wc -l) files"
echo "JB2 masks:  $(ls "$OUT/jb2/"*.pbm 2>/dev/null | wc -l) files"
echo "IW44 imgs:  $(ls "$OUT/iw44/"*.ppm 2>/dev/null | wc -l) files"
echo "Composites: $(ls "$OUT/composite/"*.ppm 2>/dev/null | wc -l) files"
echo "BZZ tests:  $(ls "$OUT/bzz/"* 2>/dev/null | wc -l) files"
echo ""
echo "Done!"
