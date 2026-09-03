# Full Review — nikon-fleet (limited run: --skip-inventory)

Pass ID: `nikon-fleet-20260903-c4e1`
Date: 2026-09-03
Scope: full source tree — src/*.rs, gui/src/main.rs, gui/fleet_gui.py, gui/fleet_lib.py, gui/test_fleet_lib.py, examples/*.rs, scripts/*.sh + setup-sdk-runtime.ps1, Cargo.toml, gui/Cargo.toml, Makefile.

Rationale for this run: this is the repo's first-ever pass under `full-review`'s Phase 1.5 (Interface), Phase 1 quality angles (6-9), and Phase 3.5 (Coverage) — all added to the skill after the repo's last review (2026-06-11). The code delta since then was itself entirely test code (no new production logic), so Phase 2 (Inventory) was skipped as low-marginal-value; everything else ran at full scope against the whole repo since these checks have zero prior history here.

## Summary
| Pass        | Critical | High | Med | Low | Status |
|-------------|----------|------|-----|-----|--------|
| Code review | 0 | 6 | 16 | 21 | ✓ ran (9 angles + severity re-pass) |
| Interface   | 0 | 0 | 1 | 2 | ✓ ran (2 finders + severity re-pass; 5 more findings independently reconfirmed Code review findings, not double-counted) |
| Inventory   | — | — | — | — | SKIPPED — --skip-inventory flag (no new data-capture code since last review; see rationale above) |
| Test review | 0 | 2 | 6 | 4 | ✓ ran (audit-tests + tautology sweep + 9-mutant spot-check) |
| Coverage    | — | — | 0 | — | ✓ ran — 66.42% overall, gui/src/main.rs 50.88%. No file at 0%. Coverage run: 66.42% overall. Coverage is a floor-check only — nonzero coverage does not indicate the tests assert anything meaningful; see Phase 3 / tautology sweep for that. |

**Test Stats** — pass:225 fail:0 mutants:9/9 tautology:0 (mechanically verified only for `gui/test_fleet_lib.py` — the sweep tool is Python-AST-only; the 166 Rust `#[cfg(test)]` tests had no mechanical tautology screening, see [#08](#08) below)

**What Interface/Test caught that Code review missed:** the empty-but-present USB serial silently colliding across cameras ([#07](#07)) is a distinct bug from Code review's "id-{N}" fallback finding ([#02](#02)) — different trigger condition, same family. The tautology-sweep tooling gap itself ([#08](#08)) is a meta-finding no other phase could have surfaced. Interface added the capability-value JSON schema drift risk ([#25](#25)), the `fmt_cap_value` KeyError crash path ([#53](#53)), and the zip-entry `.`-handling divergence ([#54](#54)) — none seen by Code review's 9 angles.

---

## Critical
None.

## High
- [code] gui/fleet_gui.py:29-37 vs gui/src/main.rs:70-75 — the two GUI frontends persist the data-dir preference to two different files (`~/Library/Preferences/...` vs `~/Library/Application Support/...`); changing it in one GUI is invisible to the other. [nikon-fleet-20260903-c4e1#01]
- [code] src/main.rs (cmd_snapshot ~549, cmd_check ~621, cmd_restore ~981) + gui/src/main.rs:509,528-531 — the `"id-{N}"` fallback serial (used when USB serial can't be read) is never matched by any later serial-lookup in the CLI or GUI — a camera recorded this way can never be found again by serial. [nikon-fleet-20260903-c4e1#02]
- [code] gui/fleet_gui.py:339-361 (add_firmware) — the GUI's "Add Firmware…" button writes the old flat layout (no metadata.json), invisible to `fleet firmware ls/pin/rollback` and silently excluded from "Export all". [nikon-fleet-20260903-c4e1#03]
- [code] src/sdk.rs:352-373 (pair_devices) — pairs SDK-enumerated devices to USB devices purely by enumeration order within same-model groups, no identifier cross-check; a fleet's primary use case (multiple same-model bodies) is exactly the trigger condition for a silent wrong-camera pairing. [nikon-fleet-20260903-c4e1#04]
- [code] model_slug logic (`model.replace(' ', "_")`) hand-copied in ≥7 places (src/main.rs x2, src/snapshot.rs, gui/src/main.rs x2, gui/fleet_gui.py x2) despite a canonical `firmware::model_slug()` already existing and reachable at 4 of those sites. [nikon-fleet-20260903-c4e1#05]
- [code] src/main.rs:775-782 (cmd_firmware_pin) — uses `strip_prefix(data_dir)` directly instead of `resolve_snapshot_path` (used correctly elsewhere); breaks the normal-case relative-path invocation with the default data-dir `"."`, confirmed via a standalone rustc repro. [nikon-fleet-20260903-c4e1#06]
- [test] src/main.rs:549-550, 621-622 — a USB serial that's present-but-**empty** (not absent) silently becomes `""` in `cmd_snapshot`/`cmd_check`'s live-snapshot branch, unlike `cmd_devices` which already guards this — two cameras with unreadable serial descriptors both snapshot to `"Z_9_.json"` and collide. Distinct bug from #02 above (different trigger: `Some("")` vs `None`). [nikon-fleet-20260903-c4e1#07]
- [test] `tautology_sweep.py` is Python-AST-only — 166 of the repo's 225 tests (all Rust `#[cfg(test)]` tests, i.e. nearly the entire suite) received **zero mechanical tautology screening** this pass; only `gui/test_fleet_lib.py`'s 59 tests were actually swept. [nikon-fleet-20260903-c4e1#08]

## Medium
- [code] src/main.rs:363 — `cmd_ls`'s `--camera` filter does an unanchored prefix match; misfires on id/serial prefix collisions (e.g. `"id-1"` vs `"id-10"`). [nikon-fleet-20260903-c4e1#09]
- [code] src/main.rs:504-512 vs gui/src/main.rs:791-799 — `name_map_for_model` duplicated verbatim between CLI and GUI (parity-tested, partially mitigated). [nikon-fleet-20260903-c4e1#10]
- [code] src/firmware.rs:181 — `list_archives` sorts firmware versions lexically, not numerically; breaks once any model reaches a double-digit major. [nikon-fleet-20260903-c4e1#11]
- [code] `cmd_firmware_rollback`'s `--serial` flag is parsed but never read; documented as restricting the bundle, has zero effect. [nikon-fleet-20260903-c4e1#12]
- [code] src/main.rs cmd_restore/cmd_snapshot — always return `Ok(())` regardless of accumulated failure counts; scripts checking exit status can't detect a fully-failed run. [nikon-fleet-20260903-c4e1#13]
- [code] src/sdk.rs:796-828 (decode_enum_values) — `w_physical_bytes` (i16) cast to usize with no sign/bounds check before a raw-pointer slice read; write side has an analogous guard, this doesn't. [nikon-fleet-20260903-c4e1#14]
- [code] SnapshotArgs.label / src/snapshot.rs:139-149 — unsanitized user label interpolated into a filename then `Path::join`ed; a `/` in the label redirects the write target. [nikon-fleet-20260903-c4e1#15]
- [code] gui/fleet_gui.py:276-284 (set_reference) — `except OSError` no longer covers the `json.loads`/dict-indexing the try block now also does; malformed snapshot JSON crashes instead of showing the intended error dialog. [nikon-fleet-20260903-c4e1#16]
- [code] cmd_diff/cmd_check call `diff()` not `diff_with_schema()` — the spec'd, tested firmware-version-boundary annotation feature (docs/firmware-archive-spec.md §8) is unreachable from every actual CLI command. [nikon-fleet-20260903-c4e1#17]
- [code] src/sdk.rs `Drop for Sdk` comment's justification for never calling `FreeSDK` doesn't cover the Rust GUI's actual long-running-process-reuse pattern (`initialize_no_usb_reset()` called repeatedly without process exit between calls). [nikon-fleet-20260903-c4e1#18]
- [code] gui/fleet_gui.py `_load_firmware` reimplements archive-scan business logic in Python (contradicts AGENT-README's "thin subprocess frontend" claim) and skips the `format_version` check `list_archives` does. [nikon-fleet-20260903-c4e1#19]
- [code] gui/src/main.rs:520 (do_snapshot) — re-parses the 337,928-line `MaidLayer.config` on every "Take Snapshot" click inside a long-running worker instead of once at startup. [nikon-fleet-20260903-c4e1#20]
- [code] gui/src/main.rs do_export compresses `firmware.bin` with Deflate; the Python GUI deliberately uses Store for the same (large, already-dense) file. [nikon-fleet-20260903-c4e1#21]
- [code] "is this the reference snapshot" is determined via a fragile truncated-timestamp heuristic, independently reimplemented in both GUIs — root cause is the reference file having no durable back-link to its source snapshot. [nikon-fleet-20260903-c4e1#22]
- [code] src/main.rs cmd_snapshot (~572) — schema-miss capabilities fold into the same `read_ok` counter as recognized ones, with no separate count/warning for schema drift. [nikon-fleet-20260903-c4e1#23]
- [code] Firmware archive keyed by free-typed `--version`, lookups use BCD-derived `cam.firmware`; nothing validates/normalizes, so a typo fails silently as "no archive." [nikon-fleet-20260903-c4e1#24]
- [interface] gui/fleet_lib.py's capability-value JSON schema is hand-matched against src/sdk.rs's output with no shared schema and no test piping real `fleet --json` output through the Python parser; #53 below is a live instance of this drift class. No CI exists; `make test` doesn't even run the Python suite. [nikon-fleet-20260903-c4e1#25]
- [test] src/firmware.rs:181 lexical version sort (same as #11) — confirmed no test pins the single-vs-double-digit-major transition. [nikon-fleet-20260903-c4e1#26]
- [test] model_slug hand-copy at ≥5 sites (same family as #05) — confirmed no parity test enforces the copies stay identical. [nikon-fleet-20260903-c4e1#27]
- [test] name_map_for_model (same as #10) — the two copies are checked against matched fixtures, not a true single test that runs both and diffs the result ("shadow parity," not real parity). [nikon-fleet-20260903-c4e1#28]
- [test] src/maid_layer.rs / src/range_value.rs parser helpers — duplicated with no parity test; range_value.rs has zero malformed-input tests vs. maid_layer.rs's four. [nikon-fleet-20260903-c4e1#29]
- [test] src/maid_layer.rs:237 — the `<DeviceCommand>` 5-int-tuple `parts.len() != 5` boundary has no at/below/above test, only the happy path. [nikon-fleet-20260903-c4e1#30]
- [test] src/snapshot.rs:126 (from_json) — no test exercises genuinely malformed/truncated JSON, only valid-JSON-wrong-version. [nikon-fleet-20260903-c4e1#31]

## Low
- [code] gui/src/main.rs do_import status omits firmware count (files ARE imported, just not reported — cosmetic). [nikon-fleet-20260903-c4e1#32]
- [code] src/range_value.rs vs src/maid_layer.rs — duplicate parser helpers; RangeValueConfig is unused outside examples/dump_ranges.rs, so no live cross-component risk. [nikon-fleet-20260903-c4e1#33]
- [code] src/firmware.rs:72-74 (archive_dir) — unsanitized `--version` joined into the archive path with no traversal validation; low because this is a local single-user CLI (self-inflicted risk only). [nikon-fleet-20260903-c4e1#34]
- [code] src/sdk.rs — `check(...)?` runs before freeing an already-populated out-pointer in 3 functions; structural gap, not confirmed to trigger. [nikon-fleet-20260903-c4e1#35]
- [code] cmd_discover — json vs. human-text output diverge on empty-but-present USB serial display (cosmetic-output-only). [nikon-fleet-20260903-c4e1#36]
- [code] src/sdk.rs decode_value has no arm for DT_BOOLEAN; falls to the generic unknown-type sentinel. [nikon-fleet-20260903-c4e1#37]
- [code] cmd_snapshot discards the per-capability error (vs. cmd_restore, which logs it) in structurally identical batch loops. [nikon-fleet-20260903-c4e1#38]
- [code] cmd_snapshot/cmd_check device-selection block duplicated verbatim. [nikon-fleet-20260903-c4e1#39]
- [code] cmd_snapshot/cmd_check capability-read loop duplicated. [nikon-fleet-20260903-c4e1#40]
- [code] gui/src/main.rs poll()/update() — "reload snapshot list if selected" pattern copy-pasted at 5 call sites. [nikon-fleet-20260903-c4e1#41]
- [code] examples/sdk_probe.rs:68 hardcodes `0x02` instead of importing `OP_GET`. [nikon-fleet-20260903-c4e1#42]
- [code] src/main.rs:873 — `format!()` with zero interpolated args. [nikon-fleet-20260903-c4e1#43]
- [code] gui/fleet_gui.py `_data_dir()` re-reads/re-parses settings.json on every call, uncached. [nikon-fleet-20260903-c4e1#44]
- [code] src/maid_layer.rs strip_open_tag/strip_self_tag allocate per line while walking the large config file. [nikon-fleet-20260903-c4e1#45]
- [code] gui/fleet_gui.py + gui/src/main.rs fully deserialize every snapshot JSON just to list 3-4 metadata fields. [nikon-fleet-20260903-c4e1#46]
- [code] gui/src/main.rs:414 clones the entire snapshots Vec every UI repaint frame inside a read-only closure. [nikon-fleet-20260903-c4e1#47]
- [code] src/sdk.rs/src/firmware.rs duplicate the hardcoded "NIKON DSC " USB-product-string prefix; one copy is dead code. [nikon-fleet-20260903-c4e1#48]
- [code] src/sdk.rs `#[repr(C)]` structs have no compile-time size/offset assertions against the vendor SDK layout. [nikon-fleet-20260903-c4e1#49]
- [code] src/sdk.rs cb_noop has the wrong signature for callback slots that may take arguments (already self-documented as an unverified risk). [nikon-fleet-20260903-c4e1#50]
- [code] gui/fleet_lib.py decode_packed_strings is imported but never called in production; dead code. [nikon-fleet-20260903-c4e1#51]
- [code] src/main.rs:477 — hand-typed empty-list JSON literal alongside a serde-built literal for the populated case. [nikon-fleet-20260903-c4e1#52]
- [interface] src/sdk.rs decode_enum_values unsupported-type fallback → JSON object instead of array → gui/fleet_lib.py fmt_cap_value raises an uncaught KeyError. Confirmed real mechanically; scanned all 4 real captured snapshots (Z5 + Z6III, 169 enum values) — zero occurrences of any element type besides the two handled ones, so current real-world exposure is empirically zero. [nikon-fleet-20260903-c4e1#53]
- [interface] gui/src/main.rs accept_zip_entry vs gui/fleet_lib.py accept_zip_entry — diverge on `.` path components (Rust rejects, Python's `Path.parts` silently drops them); both claim "must stay in sync" in comments, neither tests the case. Traced through to the actual write path — confirmed functionally inert (no traversal, `..` still symmetrically rejected by both). [nikon-fleet-20260903-c4e1#54]
- [test] src/diff.rs is_volatile — substring match against an 11-entry list, no false-positive pinning test. [nikon-fleet-20260903-c4e1#55]
- [test] src/firmware.rs model_slug — no test pins the "Z 9" vs "Z_9" input-collision case. [nikon-fleet-20260903-c4e1#56]
- [test] cmd_firmware_pin's strip_prefix — flagged per the audit's prefix-matching mandate, but actually low risk: `Path::strip_prefix` is a safe component-wise stdlib primitive, already boundary-tested. [nikon-fleet-20260903-c4e1#57]
- [test] gui/fleet_lib.py decode_packed_strings — its 6 unit tests pin a decode path that isn't wired into production, reading as coverage that isn't real coverage. [nikon-fleet-20260903-c4e1#58]

---

## Reconfirmed (found independently by ≥2 phases, not double-counted)
- Two-GUI data-dir mismatch (#01) — also independently found by both Phase 1.5 interface finders.
- GUI add_firmware flat-layout gap (#03) — also independently found by Phase 1.5 finder A.
- model_slug/reference_filename duplication (#05) — Phase 1.5 finder B additionally notes the Python-side copies (fleet_gui.py) have zero test coverage, unlike the Rust-side copies.
- FFI struct layout / cb_noop wrong signature (#49, #50) — also independently found by Phase 1.5 finder B, framed as an FFI-contract issue.
- main.rs:477 hardcoded empty-JSON literal (#52) — also independently found by Phase 1.5 finder B, with the added detail that gui/fleet_gui.py is the actual consumer of both code paths.

## Skipped
- **Phase 2 (Inventory)** — `--skip-inventory` flag, per explicit user decision: the code delta since the 2026-06-11 review was entirely test code, so no new data-capture modules exist to inventory this pass.
