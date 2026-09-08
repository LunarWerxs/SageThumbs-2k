# Sources and Attribution

## Specification References

This crate is implemented from the public DjVu v3 specification only.
All code is written from scratch based on these documents:

1. **sndjvu spec** — https://www.sndjvu.org/spec.html
   Primary implementation reference for IFF format, chunk layout, and decoding algorithms.

2. **DjVu3Spec.pdf** — LizardTech / AT&T Labs
   Official DjVu File Format Specification v3, available from the DjVu community.
   Describes INFO chunk layout, IW44 wavelet coding, JB2 bitonal coding, BZZ compression.

3. **LeCun et al. 1998** — "DjVu: A Compression Technology for Scanned Document Images"
   https://cs.nyu.edu/~yann/research/djvu/
   Original research paper describing the IW44 and JB2 compression algorithms.

## License Statement

**No code was copied from djvulibre, djvu-rs, or any GPL-licensed source.**

This implementation is written entirely from the public specification documents listed above.
The crate is licensed under MIT.

## Test corpus provenance

Public-domain DjVu fixtures used for benchmarks and integration tests are
documented in [`tests/corpus/README.md`](tests/corpus/README.md) (tier 1 +
tier 2). Fetch/verify helpers live in `scripts/fetch_corpus.sh`.
