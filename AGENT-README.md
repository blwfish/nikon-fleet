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
- **Restore** settings from a snapshot back to a camera (same model —
  literal value copy, keyed by capability code).
- **Transplant** settings from a snapshot onto a *different-model* camera
  (`src/transplant.rs`) — MAID capability IDs are a fixed SDK-wide enum
  (confirmed against `MaidLayer.config`: e.g. `Aperture` is always id
  33043), so "the same setting" is already the same code across bodies;
  what differs is enum option ordering/count and range bounds, which this
  translates via a live read of the target's own current value rather than
  assuming the source's index/value carries over unchanged. Does not cover
  Custom Settings menu items (Focus Peaking, etc.) that aren't in MAID's
  capability list at all — those live in the vendor-only PTP-property
  space (see `src/ptp_usb.rs`) and would need their own per-body discovery
  work first.
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
- **What a snapshot holds is shown by one formatter, `ops::render`.**
  `nikon-fleet show <file-or-name> [--filter TEXT] [--json] [--codes] [--raw]` (CLI, read-only,
  no camera or SDK) and the egui GUI's snapshot window (double-click a row,
  or its View button) both call `ops::render::render_snapshot`; the Python
  GUI's `fmt_cap_value` must agree with it, and
  `gui/tests-fixtures/render_values.json` is the table both sides are tested
  against. A value that is missing or unusable is shown as `(missing)` or
  `(malformed: why)` with a closed `kind` and `notes`, never as a default;
  stepped ranges read their position from `value_index` through
  `value_shape`. Change the display rules there, not in a UI.
- **Names for what a snapshot holds come from files on the machine that has
  the SDK, never from the repository.** `ops::labels::Labels` reads
  `sdk-runtime/MaidLayer.config` (display names, and some enumeration value
  names, for the snapshot's model and firmware generation) and
  `sdk-runtime/maid-labels.json` (the base capability names from `Maid3.h`,
  what a `cap_0x..` key is called, and the value names the SDK's PDF documents), which
  `scripts/extract-maid-labels.py <SDK folder>` writes (needs poppler's
  `pdftotext`; `scripts/setup-sdk-runtime.sh` calls it). Both are licensed SDK
  material and gitignored; every part is optional and a missing one is
  reported, not guessed (`nikon-fleet show` prints the numbers and a note on
  stderr). `render_snapshot_with` / `render_value_with` use them and also show a
  range's scale; the plain `render_*` functions never do, which keeps
  `render_values.json` SDK-independent (`render_labels.json` pins the named forms
  with invented labels). The PDF parser leaves a capability unnamed rather than
  complete a list by guessing: see `unparsed` in the generated file for each
  reason. An enum value its capability's list lacks is shown, with the
  `value_not_in_labels` note, never hidden.
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
cargo test --workspace         # Rust unit tests: 108+23 in the nikon-fleet lib/bin, 35+ in gui/ (all offline)
cd gui && python3 -m pytest    # GUI helper unit tests (test_fleet_lib.py; also offline)
python3 -m unittest scripts/test_extract_maid_labels.py   # the SDK-document extractor (invented inputs; offline)
```

To name what `nikon-fleet show` prints (display names, enumeration value names), run
`python3 scripts/extract-maid-labels.py /path/to/S-SDKZ-200BF-ALLIN` by hand; it writes
`sdk-runtime/maid-labels.json` (gitignored) and needs poppler's `pdftotext`
(`brew install poppler`). `scripts/setup-sdk-runtime.sh` runs it too.

### Supported GUI environment

The Python/tkinter GUI (`gui/fleet_gui.py`) and its tests are supported on **Python 3.14 with Tk 9.0.x**
(Homebrew's `python@3.14` and python.org's 3.14 installer; CI runs 3.14.8/Tk 9.0.4). **Tk 8.6 is not
supported** — that is what python.org's Python 3.13 and older ship (3.13.16 has Tk 8.6.18, 3.12.10 has
8.6.16). On Tk 8.6 the GUI tests hang: `test_fleet_gui.py::TestStartup::test_a_settings_error_is_shown_and_nothing_is_guessed`
never returns from Tk's `update` on the macOS runner (found 2026-10-30; cause not diagnosed, and the
tests after it were never reached, so there may be more). The test fixture `tk_root` therefore refuses
Tk older than 9.0 (skips, or fails when `CI`/`REQUIRE_DISPLAY` is set) rather than hanging. The CLI,
the MCP server and the egui GUI do not use Tk. Check yours with
`python3 -c "import tkinter; print(tkinter.TkVersion)"`.

The GUI tests share **one `Tk()` root for the whole run** (`_shared_tk_root` in `gui/test_fleet_gui.py`),
reset between tests. Creating and destroying a root per test crashed intermittently on the macOS runner
on Tk 9.0 (exit 139/138 mid-suite, and a CoreFoundation "process has forked" message in captured stderr):
3 of 6 runs of the per-test-root tests crashed in an A/B on 2026-10-30 against 0 of 6 with the shared
root, and about 5 of 13 per-test-root runs overall against 0 of 8. Tk's macOS port wraps a single
`NSApplication`, which is the likely reason; that part is a hypothesis. Keep it one root.

`gui/` is a workspace member (joined 2026-09-03), not a standalone crate — `cargo test`/`cargo llvm-cov` run without `--workspace` from the repo root cover only the root `nikon-fleet` package and silently skip `gui/src/main.rs`'s tests.

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
