# Notes for Claude Code

Performance experiments and their outcomes are logged in
[`PERF_EXPERIMENTS.md`](PERF_EXPERIMENTS.md). When recording a new result
("Kept" / "Reverted" + reason), append a section there, not here.

Each entry: issue, approach, numbers, decision, reason.

Every `###` entry also gets a row in [`EXPERIMENTS_INDEX.md`](EXPERIMENTS_INDEX.md)
**in the same PR** — that file's own maintenance rule; an incomplete index causes
duplicate work (a 2026-07-10 audit found four issues already answered by
unindexed entries).

`PERF_EXPERIMENTS.md` is marked `merge=union` in `.gitattributes`: concurrent
branches appending sections merge without a conflict. The tradeoff is that
edits to the *same existing* section are silently duplicated rather than
flagged — prefer appending over rewriting old entries.

## Before pushing — run the local gates

CI failures on `main` are mostly preventable locally. Mirror the deterministic CI
gates with one command:

```
make check        # fmt, clippy -D warnings, no_std build, wasm32 (+ simd128, no_std_smoke), tests (cli,tiff)
make hooks        # one-time: enable the pre-push hook (core.hooksPath=.githooks)
```

`scripts/check.sh` is the single source of truth (the pre-push hook and `make check`
both call it). The `cargo build --no-default-features --target wasm32-unknown-unknown`
step is the one that catches no_std `vec!` / leaked `std::*` regressions (#448) before
they reach CI. Requires `rustup target add wasm32-unknown-unknown`.

`main` is protected (ruleset): pushes land via PR and the deterministic checks
(`Lint`, `Test (stable)`, `wasm32 build check`, `MSRV`, `Dependencies`) must be green
to merge. Fuzz/Benchmarks are intentionally *not* required — they are
non-deterministic / slow; their robustness comes from in-code margins and guards,
not merge gating.
