# Security Policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |

## Scope

djvu-rs is a parser for an untrusted binary format. The following are in scope:

- Panics or aborts when parsing malformed DjVu input
- Out-of-bounds memory access (the crate forbids `unsafe`, but logic bugs can still
  cause index panics)
- Integer overflow leading to incorrect output or resource exhaustion
- Denial-of-service via crafted input (excessive allocation, infinite loops)

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Report privately via GitHub's built-in mechanism:
**Security → Report a vulnerability** on the repository page.

Include:
- A minimal reproducer (a byte sequence or `#[test]` that triggers the issue)
- Rust version and OS
- What you expected vs. what happened

You will receive a response within 7 days. If the issue is confirmed, a patched
release will be published and you will be credited in the changelog unless you
prefer otherwise.

## Fuzzing

The `fuzz/` directory contains libFuzzer targets for the main codec entry points.
If you find a crash via fuzzing, please include the minimized corpus input.

## Decode-time resource ceilings

A malicious file can cost only bounded memory and work before decoding errors
out. Every allocation or loop whose size is driven by untrusted bytes is capped
against an explicit constant or the input length (#589 inventory). The load-
bearing bounds:

| Axis | Bound | Constant |
|------|-------|----------|
| JB2 symbol pixels (per symbol) | 16 MP | `MAX_SYMBOL_PIXELS` |
| JB2 symbol pixels (per stream) | 256 MP | `MAX_TOTAL_SYMBOL_PIXELS` |
| JB2 page / blit pixels | 16 MP / 256 MP | `MAX_PAGE_SYMBOL_PIXELS` / `MAX_TOTAL_BLIT_PIXELS` |
| JB2 record count | 65 536 | `MAX_RECORDS` |
| IW44 declared image | 64 MP | checked in `decode_chunk` |
| BZZ block / total output | 4 MiB / 256 MB | `MAX_BLOCK_SIZE` / `MAX_OUTPUT_SIZE` |
| IFF / FORM nesting depth | 64 | `MAX_IFF_DEPTH` |
| NAVM bookmark depth | 256 | `MAX_NAVM_DEPTH` |
| S-expression (ANTz) depth | bounded | `MAX_SEXPR_DEPTH` |
| **Text-zone (TXTz) depth** | **64** | **`MAX_ZONE_DEPTH`** (#589) |
| **Text-zone child reservation** | **≤ remaining ÷ 17 bytes** | **`MIN_ZONE_RECORD_BYTES`** (#589) |

DIRM component/name tables, ANTz maparea lists, and IFF chunk payloads allocate
only in proportion to bytes actually present in the file (each is length-checked
against the buffer before allocating), so they are bounded by construction.

The `#589` additions close the one unbounded axis the inventory found: a crafted
`TXTz` zone could declare up to ~16.7M children (a 3-byte `i24`) and recurse with
no depth limit. The child count no longer drives an up-front reservation beyond
what the remaining bytes could encode, and the zone tree cannot nest past
`MAX_ZONE_DEPTH`.
