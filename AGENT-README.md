# AGENT-README.md

Orientation for an AI agent picking up work in this repo. Read this before
touching code — it front-loads the context that would otherwise take several
exploration passes to reconstruct.

## What this project is

`nikon-fleet` is a Rust CLI (`fleet`) + Python/tkinter GUI for managing
settings and firmware across a small personal fleet of Nikon Z-series
cameras (Z 5, Z 6III, two Z 9 bodies). It talks to cameras over USB via the
Nikon Remote SDK v2 (a vendored CFBundle/DLL, not distributed with this
repo — see `sdk-runtime/`) and via raw USB descriptor reads (`rusb`) for
data the SDK doesn't expose correctly.

Core capabilities:
- **Snapshot** a camera's full settings state to JSON.
- **Diff** two snapshots, or a live camera against a saved reference
  (`fleet check`), to see what's changed.
- **Restore** settings from a snapshot back to a camera.
- **Firmware archive**: store firmware `.bin` files keyed by (model,
  version), pin a canonical settings snapshot to each version, and generate
  guided rollback bundles (manual re-flash is required — Nikon doesn't
  expose a firmware-write API).

This is a single-user hobby/utility tool (the user's own camera fleet), not
a product with external users. Favor pragmatic fixes over generalizing for
inputs that can't occur.

## Repo layout

```
src/            Rust library + CLI (the `nikon-fleet` crate; binary name `fleet`)
  main.rs         clap CLI surface — thin wrapper, logic lives in the library modules
  sdk.rs          FFI + safe wrapper around the Nikon SDK dylib (the only `unsafe` module)
  maid_layer.rs   Parser for MaidLayer.config (SDK's capability schema, custom pseudo-XML)
  range_value.rs  Parser for RangeValue.config (allowed values/defaults per capability)
  snapshot.rs     Snapshot data model (BTreeMap<String, serde_json::Value>, JSON on disk)
  diff.rs         Same-model snapshot diffing
  firmware.rs     Firmware archive: storage layout, metadata, rollback bundle generation
gui/            Python/tkinter GUI — a frontend that shells out to the `fleet` binary
  fleet_gui.py    tkinter app
  fleet_lib.py    Pure-function helpers (parsing/formatting), unit-tested in isolation
  test_fleet_lib.py
examples/       Standalone probes for exercising the SDK/parsers without the full CLI
scripts/        setup-sdk-runtime.{sh,ps1} — stage the vendor SDK into sdk-runtime/
docs/
  firmware-archive-spec.md   Design spec for the firmware archive feature (see below)
  todo.md                    Backlog — PTP/IP reverse-engineering, resource-string labels, etc.
sdk-runtime/    Staged vendor SDK files (dylib/DLLs + .config schemas). Gitignored.
firmware/       Archived firmware .bin files. Gitignored (contains large binaries).
snapshots/      Saved camera snapshots (JSON). Gitignored.
```

`sdk-runtime/`, `firmware/`, `snapshots/`, `references/`, and `target/` are
all gitignored — they're machine-local state, not source. If you need to
see what's in them to debug something, read the files directly; don't
expect `git log` to have history for them.

## Architecture notes worth knowing before editing

- **Camera identity does NOT come from the SDK.** The SDK's `DeviceInfo`
  returns empty strings for firmware version and serial on every tested
  model. Firmware version comes from the USB `bcdDevice` descriptor field
  (BCD-decoded: `0x0531` → `"5.31"`); serial comes from the USB
  `iSerialNumber` string descriptor. This is read via `rusb`, independent
  of the SDK connection. See `sdk.rs` (`usb_camera_list`, `UsbCameraInfo`)
  and `docs/firmware-archive-spec.md` §3 for the full rationale — don't
  reintroduce SDK-sourced identity.
- **Model name is a lookup key, not a derivable string.** Canonical model
  identifier = USB product name with the `"NIKON DSC "` prefix stripped
  (`"NIKON DSC Z 9"` → `"Z 9"`). Naming is inconsistent across generations
  (`"Z 9"` vs `"Z6_3"` — space vs. underscore, "III" abbreviated as `_3`).
  This stripped form is also the `MaidLayerConfig` key. Do not try to
  normalize it programmatically.
- **MaidLayer.config / RangeValue.config are not real XML.** They embed
  data in tag names via colon delimiters (e.g.
  `<capability:kNkMAIDCapability_Aperture-33285>`), and `</model>` /
  `</capability>` close tags don't repeat the embedded data. Both parsers
  are hand-rolled line-oriented state machines — see the module doc
  comments in `maid_layer.rs` and `range_value.rs` before changing the
  format handling.
- **Schema is versioned by firmware, cumulatively.** Capability
  availability sections in `MaidLayerConfig` are matched by *major*
  firmware version only (`"5.31"` → schema section `"5.0"`), and sections
  are cumulative (Z 9's full capability set at fw 5.x = union of sections
  `2.0, 3.0, 4.0, 5.0`). The Z 5 has only a `[common]` section — no
  firmware-boundary diff annotation is possible for it, and that's handled
  as a graceful skip, not an error.
- **The SDK connection has a USB-reset dance.** `Sdk::initialize()` resets
  the camera's USB connection via `rusb` immediately before
  `InitializeSDK`, then pumps the CoreFoundation run loop between
  `EnumDevices` retries. This works around the SDK's IOKit device-arrival
  notification missing cameras already connected at process start. The
  `--no-usb-reset` CLI flag exists because this reset invalidates Metal
  surfaces when called from a GUI/AppKit context — the GUI always passes
  it. See the module doc comment at the top of `sdk.rs`.
- **`Sdk::drop` deliberately does not call `FreeSDK`.** See the `Drop` impl
  and its comment before "fixing" this.
- **Snapshot values are `serde_json::Value`, not a typed enum.** Camera
  property values are heterogeneous (ints, strings, byte arrays, nested
  arrays) and this is intentional — see the design-choices comment at the
  top of `snapshot.rs`. Properties are stored in a `BTreeMap` specifically
  so JSON diffs in git show real changes, not key reordering.
- **Cross-model diffing is out of scope.** `diff.rs` only diffs two
  snapshots from the *same* camera model — see the module doc comment for
  why (semantic alignment between bodies needs per-model MaidLayer
  availability data and better UX for "doesn't exist on this body" than a
  symmetric diff gives).
- **Settings write-back exists via `SetCapability`, but read is the
  primary path.** `fleet restore` and the GUI's editing flow use
  `OP_SET`/`SetCapability`; `fleet firmware rollback` still requires a
  *manual* firmware flash (Nikon exposes no firmware-write API) and emits
  a human-readable settings table plus step-by-step instructions rather
  than attempting to push settings automatically as part of rollback.
- **The GUI is a thin subprocess frontend, not a reimplementation.**
  `fleet_gui.py` shells out to the compiled `fleet` binary
  (`target/release/nikon-fleet`, falling back to `target/debug`) and
  parses its `--json` output. Business logic belongs in the Rust
  library/CLI, not in the GUI. `fleet_lib.py` holds the GUI's own pure
  helper functions (string parsing/formatting) and is unit-tested
  independently in `test_fleet_lib.py`.

## Before making a change

- If you're touching identity sourcing (model/serial/firmware version),
  parser format handling (`maid_layer.rs`/`range_value.rs`), or the
  SDK lifecycle (`sdk.rs`), re-read the relevant module doc comment first
  — these were each the product of debugging sessions with non-obvious
  findings, and the comment usually explains a constraint that isn't
  visible from the code shape alone.
- `docs/firmware-archive-spec.md` is the design spec for the firmware
  archive feature and documents the fleet's actual USB descriptor values
  (product names, `idProduct`, `bcdDevice`, serials) as of the 2026-06-02
  probe — treat the numbers as historical/example data, not a live
  registry; if a new body joins the fleet, this doc won't know about it.
- `docs/todo.md` is the live backlog (PTP/IP capture for undocumented
  vendor properties, MaidLayer resource-string label decoding, menu-order
  sort, settings-bundle editing). Check it before assuming a gap is
  unnoticed.

## Building and testing

```bash
cargo build --release          # produces target/release/nikon-fleet
cargo test                     # Rust unit tests (108+ across the library, run offline — no camera/SDK needed)
cd gui && python3 -m pytest    # GUI helper unit tests (test_fleet_lib.py; also offline)
```

Running the CLI/GUI against real hardware requires:
1. The vendor Nikon Remote SDK staged into `sdk-runtime/` — run
   `scripts/setup-sdk-runtime.sh /path/to/S-SDKZ-200BF-ALLIN` once (the SDK
   itself is not in this repo or its history).
2. On macOS, examples that talk to the SDK/camera need ad-hoc codesigning
   with `sdk-entitlements.plist` (see the header comment in
   `examples/sdk_probe.rs` for the exact `codesign` invocation).
3. An actual camera connected over USB. There is no camera simulator/mock
   in this repo — hardware-dependent code paths cannot be exercised by
   `cargo test` alone. Say so explicitly if you can't verify a
   camera-facing change rather than assuming the unit tests cover it.

`examples/` binaries (`dump_schema`, `dump_ranges`, `load_sdk`,
`sdk_probe`) are useful minimal repros when isolating an SDK or parser
issue from the full CLI.
