# Local developer gates mirroring CI. `make hooks` once to enable the pre-push hook.
.PHONY: check hooks pgo wasm wasm-threads-check

## Run the same deterministic gates CI enforces (fmt, clippy, no_std, wasm, tests).
check:
	@scripts/check.sh

## Enable the version-controlled git hooks (pre-push runs `make check`).
hooks:
	@git config core.hooksPath .githooks && echo "git hooks enabled: core.hooksPath=.githooks"

## Opt-in profile-guided optimization build (layers PGO over fat-LTO). Trains on
## the corpus, then PGO-rebuilds the `djvu` CLI. ~15% faster cold render; see the
## "PGO" entry in PERF_EXPERIMENTS.md. Needs the llvm-tools-preview component.
pgo:
	@scripts/pgo.sh

## Build the shipped browser WASM package: scalar fallback + simd128 variant
## selected at runtime via WebAssembly.validate. Pass OUT=... or FEATURES=...
## to override the generated package directory or wasm feature set.
wasm:
	@bash scripts/build_wasm_dual.sh

## Opt-in wasm32 thread-pool build check (`wasm-threads` feature, rayon via
## wasm-bindgen-rayon). Needs nightly + `-Z build-std` — NOT a required CI gate
## and NOT part of `scripts/check.sh`/`make check`. See WASM_THREADS in
## PERF_EXPERIMENTS.md. Pass `ARGS=--build` for a full release build+link.
wasm-threads-check:
	@scripts/wasm_threads_check.sh $(ARGS)
