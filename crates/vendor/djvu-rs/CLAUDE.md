# Notes for Claude Code

Performance experiments and their outcomes are logged in
[`PERF_EXPERIMENTS.md`](PERF_EXPERIMENTS.md). When recording a new result
("Kept" / "Reverted" + reason), append a section there, not here.

Each entry: issue, approach, numbers, decision, reason.

## Before pushing — run the local gates

CI failures on `main` are mostly preventable locally. Mirror the deterministic CI
gates with one command:

```
make check        # fmt, clippy -D warnings, no_std build, wasm32 (no_std + wasm), tests
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
