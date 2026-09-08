#!/bin/bash -eu
# Build script invoked by OSS-Fuzz infrastructure.
#

cd $SRC/djvu-rs

# Each target lives at fuzz/fuzz_targets/<name>.rs and is registered in
# fuzz/Cargo.toml. OSS-Fuzz's Rust base image configures sanitizer flags via
# the environment; cargo-fuzz builds all registered targets with those flags.
cargo fuzz build -O
fuzz_out=fuzz/target/x86_64-unknown-linux-gnu/release
for target in fuzz_full fuzz_iff fuzz_jb2 fuzz_bzz fuzz_iw44 fuzz_encode fuzz_bzz_encode fuzz_g4 fuzz_metadata; do
    cp "$fuzz_out/$target" "$OUT/$target"
done

# Seed corpora — OSS-Fuzz convention: $OUT/<target>_seed_corpus.zip is
# unpacked into the per-target corpus dir on the first run. We ship the
# in-tree fuzz/corpus/<target>/ contents which are all small (< 200 KB
# each, < 1 MB total) and CC0/synthetic.
for target in fuzz_full fuzz_iff fuzz_jb2 fuzz_bzz fuzz_iw44 fuzz_encode fuzz_bzz_encode fuzz_g4 fuzz_metadata; do
    if [ -d "fuzz/corpus/$target" ] && [ -n "$(ls -A fuzz/corpus/$target 2>/dev/null)" ]; then
        (cd "fuzz/corpus/$target" && zip -qr "$OUT/${target}_seed_corpus.zip" .)
    fi
done
