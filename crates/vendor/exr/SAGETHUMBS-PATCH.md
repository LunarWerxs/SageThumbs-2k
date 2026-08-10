# SageThumbs 2K patch to `exr` 1.74.2

This directory is the crates.io `exr` 1.74.2 package
(`711fe42c9964295e01ee3fba3f9fe0e1d24b98886950d68efe81b1c76e21adf3`)
with one storage-only patch centered in
`src/compression/dwa/lossy_dct/transfer_curve.rs`.

Version 1.74.2's DWAA/DWAB implementation keeps two
`OnceLock<[u16; 65536]>` lookup tables inline. On Windows/MSVC, those two
zero-filled arrays occupy 256 KiB in every linked PE artifact. SageThumbs ships
three Rust artifacts, so the same lazy tables added roughly 768 KiB of raw
payload. Their exact effect on the solid-compressed installer depends on the
complete payload and compression settings.

The patch separates each table's loader-zeroed
`UnsafeCell<MaybeUninit<[u16; 65536]>>` storage from its small `OnceLock<()>`
initialized flag. The tables therefore occupy virtual zero-fill rather than
file-backed PE data, require no heap allocation, and cannot leak if the
SageThumbs COM DLL is unloaded and loaded again. Initialization writes directly
to the static storage without a large stack temporary.

The unsafe boundary consists of the storage type's `Sync` implementation,
serialized pointer writes, and the post-initialization shared borrow. Each item
has a local lint exception and safety proof; unsafe code remains denied
crate-wide everywhere else. The public and internal array-typed interfaces,
conversion formulas, SIMD paths, laziness, and thread safety remain unchanged.
`src/lib.rs` and the package description acknowledge this narrow exception to
upstream's otherwise unsafe-free implementation.

An exhaustive test compares both generated tables with the original scalar
functions for every possible half-float bit pattern. A contention test also
starts 16 simultaneous first readers and verifies that all observe the same
table after exactly one complete initialization.

The normalized `Cargo.toml` also carries cargo-machete metadata for exr's
intentional direct `num-complex` version pin. This mirrors the explanation in
`Cargo.toml.orig` and affects tooling only.

Focused verification:

```powershell
cargo test --manifest-path vendor/exr/Cargo.toml --no-default-features --lib compression::dwa::
cargo test --manifest-path vendor/exr/Cargo.toml --no-default-features --test roundtrip roundtrip_dwa
```

The crates.io package excludes `tests/images/**`, so do not run its asset-backed
test filters from this vendored copy.

Remove the `[patch.crates-io]` override in the workspace `Cargo.toml` and this
directory once an upstream `exr` release contains the same leak-free,
loader-zeroed storage fix.
