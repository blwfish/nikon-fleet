.PHONY: test test-cov

test:
	cargo test

# Release-gate cadence only, not part of `make test` — coverage is a
# floor-check (does anything exercise this file at all), not a quality
# gate. Requires `cargo install cargo-llvm-cov` + `rustup component add
# llvm-tools-preview` once per machine. /full-review's Phase 3.5 detects
# this target and runs it.
#
# Scope: the main nikon-fleet crate only. gui/ is a separate crate (not a
# workspace member) with zero #[test] functions as of 2026-07-18 — nothing
# to measure there yet; revisit once it has real tests.
test-cov:
	cargo llvm-cov --summary-only
