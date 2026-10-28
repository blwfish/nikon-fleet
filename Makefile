.PHONY: test test-rust test-gui test-cov

# --workspace: gui/ joined the workspace 2026-09-03 (was a standalone
# crate with its own Cargo.lock). Plain `cargo test`/`cargo llvm-cov` run
# from a workspace root that is itself a package default to that root
# package only, not the other members — omitting --workspace here would
# silently stop running gui/src/main.rs's tests.
test: test-rust test-gui

test-rust:
	cargo test --workspace

# The Python GUI's tests (gui/test_*.py, 500+ cases) were run by nothing until
# 2026-10-28 (review nikon-fleet-20261028-e5a3 H14): neither `make test` nor
# `cargo test --workspace` touched them. pytest is required, not optional; the
# cases that drive a real Tk window skip themselves when there is no display.
# Needs the CLI built first (cargo test --workspace does that) for the tests
# that run the real binary on camera-free commands.
test-gui:
	cd gui && python3 -m pytest -q

# Release-gate cadence only, not part of `make test` — coverage is a
# floor-check (does anything exercise this file at all), not a quality
# gate. Requires `cargo install cargo-llvm-cov` + `rustup component add
# llvm-tools-preview` once per machine. /full-review's Phase 3.5 detects
# this target and runs it.
test-cov:
	cargo llvm-cov --workspace --summary-only
