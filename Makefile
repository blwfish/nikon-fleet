.PHONY: test test-cov

# --workspace: gui/ joined the workspace 2026-09-03 (was a standalone
# crate with its own Cargo.lock). Plain `cargo test`/`cargo llvm-cov` run
# from a workspace root that is itself a package default to that root
# package only, not the other members — omitting --workspace here would
# silently stop running gui/src/main.rs's tests.
test:
	cargo test --workspace

# Release-gate cadence only, not part of `make test` — coverage is a
# floor-check (does anything exercise this file at all), not a quality
# gate. Requires `cargo install cargo-llvm-cov` + `rustup component add
# llvm-tools-preview` once per machine. /full-review's Phase 3.5 detects
# this target and runs it.
test-cov:
	cargo llvm-cov --workspace --summary-only
