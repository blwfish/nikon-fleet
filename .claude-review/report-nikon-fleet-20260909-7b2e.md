# Full Review — nikon-fleet, c9b3685..HEAD (gui/, src/)

Scope: the 9 files changed since the last full review (2026-09-03, `c9b3685`): `gui/fleet_gui.py`, `gui/fleet_lib.py`, `gui/src/main.rs`, `gui/test_fleet_lib.py`, `src/lib.rs`, `src/main.rs`, `src/ptp_usb.rs` (963 lines, new), `src/sdk.rs`, `src/transplant.rs` (517 lines, new). 2181 lines added.

Pass ID: `nikon-fleet-20260909-7b2e`

## Summary

| Pass        | Critical | High | Med | Low | Status |
|-------------|----------|------|-----|-----|--------|
| Code review | 3 | 9 | 8 | 7 | ✓ ran (9 angles + severity re-pass) |
| Interface   | 1 | 5 | 1 | 0 | ✓ ran (2 passes + severity re-pass) |
| Inventory   | 0 | 4 | 7 | 1 | ✓ ran (6 passes across 2 modules + severity re-pass) |
| Test review | 2 | 4 | 3 | 3 | ✓ ran |
| Coverage    | — | — | — | — | SKIPPED (no coverage tool configured) |

Raw findings across all phases: 58. After merging findings independently rediscovered by 2+ phases (strong corroborating signal, not separate bugs), **42 distinct defects**: **4 Critical, 15 High, 12 Medium, 11 Low.** (Finding #20 below is the same defect as #10, independently corroborated by test review — merged in the ledger, kept as a separate note here for provenance.)

Every phase beyond code review earned its keep: **Interface** found 3 things code review's 9 angles missed entirely (no round-trip verification for the FTP blob encoder, the sentinel-shape misclassification in `transplant.rs`, and the untested CLI-argv/GUI-argv boundary). **Inventory** found the `decode_response_params` silent-truncation bug (3/3 passes) and the discarded-error pattern in `transplant.rs` (3/3 passes) — neither flagged by code review's angles even though one of them (altitude) was looking directly at the surrounding code. **Test review** found two fresh surviving mutants (an untested `raw.len()==12` boundary, an untested `FTP_ASCII_FIELD_MAX` exact-limit) that only mutation testing could surface, plus zero test coverage on the new session-retry protocol logic.

---

## Critical

- **[code+interface+test]** `src/ptp_usb.rs:304-314` — `decode_container` panics (`raw[12..end]` with `end<12`) whenever a USB response declares a container length under 12 bytes while the actual read is ≥12 bytes. Confirmed by direct execution (exit 101) in two independent passes. In the CLI this crashes the process (exit 101, bypassing the normal `anyhow`-formatted error path). In the egui GUI it's worse: the panic happens inside the background worker thread's `for cmd in rx` loop, which has no `catch_unwind` — it **permanently kills the worker thread**, and every subsequent GUI command afterward silently no-ops (the channel receiver is gone) with **no error shown to the user**. The GUI just hangs in "busy" state forever. Test review additionally found a second, distinct surviving mutant on the same guard (`raw.len() < 12` → `<= 12`) — no test uses a 12-byte buffer, a completely normal zero-payload PTP response. **[nikon-fleet-20260909-7b2e#01]**

- **[code+inventory]** `src/ptp_usb.rs:304-315` (`decode_container`) + `:506-510` (`read_container`) — a single fixed 16384-byte `read_bulk()` call; when a container's declared length exceeds one read, the payload is silently truncated (`length.min(raw.len())`, no error/flag) **and** the unread continuation bytes are left in the USB pipe, so the *next* `read_container()` call misinterprets those leftover bytes as a fresh 12-byte header — silent protocol desync, not just data loss. This isn't hypothetical: the module's own referenced doc (`docs/nx-field-session-2026-07-09.md`) documents a sibling Nikon vendor opcode needing 24576-byte chunking specifically because single transfers can exceed 16KB. `read_vendor_property` is a generic API for any u16 propcode, not scoped to small responses. A related, distinct data-loss point in the same area: `decode_response_params` (`:317-322`) uses `chunks_exact(4)`, silently dropping any non-4-byte-aligned trailing bytes with no signal — found independently by all 3 inventory passes. **[nikon-fleet-20260909-7b2e#02]**

- **[code]** `src/ptp_usb.rs` (`PtpUsbSession::open` via `find_device`/`candidate_devices`) — the new raw-USB vendor-write path (`vendor-write-ftp`, opcodes 0x9413/0x90E8/0x90EE/0x943B) never checks camera model before sending. `nikon_usb_devices()` filters only on USB vendor ID (all Nikon bodies); a `model_from_product_string` helper already exists in `src/sdk.rs`, three lines from functions this module *did* import, but was left private/unused. These are reverse-engineered wire formats the module's own doc comments say are confirmed only on Z6III/Z9 — and per `AGENT-README.md` this fleet includes a Z5. Selecting that camera in Vendor Ops and clicking Write sends an unconfirmed wire-format blob to it today, not hypothetically. **[nikon-fleet-20260909-7b2e#03]**

- **[code]** `src/main.rs:1388` (`cmd_transplant`) — with ≥2 Nikon cameras attached and no `--serial`, `fleet transplant` — a destructive cross-model settings write — resolves the target via `select_device()`, whose ambiguous-case arm silently returns "the first available device" instead of erroring. This directly contradicts the command's own doc comment ("Required when more than one Nikon camera is on USB") and diverges from sibling command `cmd_restore` and the new `ptp_usb::find_device`, both of which correctly error on ambiguity in the identical situation. Confirmed by reading `select_device`'s actual `None` arm. User's fleet is explicitly multi-camera (Z5, Z6III, 2×Z9). **[nikon-fleet-20260909-7b2e#04]**

## High

- **[code]** `src/ptp_usb.rs:167-177` (`candidate_devices`) — silently drops any USB device whose `device_descriptor()`/`.open()` fails (`filter_map`+`.ok()?`, no counter/log) — the module's own doc says this is the *common* case on macOS (`ptpcamerad`/`icdd` claim interfaces first), not rare. With 2 cameras attached and one fails to open, `find_device(None)` sees 1 candidate and silently operates on it as if unambiguous. Same function: `desc.serial_number_string_index().unwrap_or(0)` (`:173`) treats a missing serial-string index as "use index 0" — the language descriptor, not a serial — risking a garbage/wrong device-identity read in a tool whose core job is distinguishing physical bodies by serial. **[#05]**

- **[code]** `src/main.rs:839`, `gui/src/main.rs:836-841`, `gui/fleet_gui.py:806-816` — the dry-run/preview path prints the fully hex-encoded FTP profile blob to stdout/the GUI's visible output box; the plaintext password is trivially recoverable from the hex bytes, defeating the password-masking on the entry field one line above it in both GUIs. Persists in terminal scrollback/logs, unlike the CLI-arg exposure below. **[#06]**

- **[code]** `src/transplant.rs:163-166` (`translate_range`) — the discrete-step formula `((clamped-lower)/(upper-lower))*(steps-1)` assumes the Nikon MAID SDK's `ul_value_index` linearly subdivides `[lower,upper]`; nothing verifies this against real SDK/camera behavior, only self-consistency is tested. If wrong, transplant silently writes a working-but-wrong value to a live camera — correct return code, wrong answer, indistinguishable from success. **[#07]**

- **[code]** `src/transplant.rs:228-256` (`transplant_one`) — the module's entire premise (cross-model MAID capability IDs are a fixed SDK-wide enum) is verified only by spot-checking "a few well-known capabilities" per its own doc comment. `cap_map` already carries `cap.description` (a live SDK-provided name per ID) but `transplant_one` never compares it against the source snapshot's own name before translating/writing — a cheap, already-fetched check going unused. **[#08]**

- **[code+interface]** `src/ptp_usb.rs:167-193` (`find_device`/`candidate_devices`) — the new vendor-ops device selector reimplements serial matching from scratch, comparing only against the raw USB `iSerialNumber`. It does not recognize the `"id-{id}"` synthetic-serial fallback used whenever the real USB serial can't be read — a bug class already fixed project-wide in commit `9d7cfd2`, never propagated to this path. Concretely: selecting the one attached camera and clicking Read/Write fails with `SerialNotFound("id-5")` instead of working. **[#09]**

- **[code]** `gui/src/main.rs:524-548` vs `gui/fleet_gui.py` — "missing field" validation checks `.trim().is_empty()` on FTP profile fields, but the value actually written uses the **untrimmed** original. The Python/tkinter GUI does `.strip()` the same fields. Identical user input (e.g. a pasted value with trailing whitespace) produces different on-camera FTP configs depending which GUI was used, with no parity test between them. **[#10]**

- **[code+interface+test]** `gui/fleet_lib.py:135-151` vs `src/ptp_usb.rs:630-637` (`parse_propcode`) — independently reimplemented in Python (can't call into the Rust crate), with the Python docstring explicitly claiming parity. **Two separate confirmed divergences**, found by two different passes via direct execution: (1) Python tolerates whitespace between the `0x` prefix and digits (`"0x 10"` → 16; Rust rejects it), and (2) Python accepts underscore digit-group separators (`"0xD0_53"` → valid; Rust's `from_str_radix` rejects them). No shared test-vector fixture protects either language from drifting further — each has its own hand-written test suite that happens to overlap on common cases. **[#11]**

- **[code]** `gui/src/main.rs:687-690` — unverified repeat-risk of a previously-fixed crash class: this codebase already had to patch `do_discover` (commit `48704d5`) because firing USB disconnect/reconnect-shaped events from a background thread invalidated the Metal drawing surface mid-frame and segfaulted the GUI; that fix deliberately kept USB-reset logic CLI-only. The new Vendor Ops code performs an adjacent operation (`claim_with_retry`'s repeated `SET_CONFIGURATION` + killing `ptpcamerad`/`icdd`) from that same GUI background-worker context, and per its own commit message the button was confirmed only to render, never actually clicked live. **[#12]**

- **[inventory]** `src/transplant.rs:241` — `device.read_capability()`'s actual error is discarded via `map_err(|_| SkipReason::TargetReadFailed)`, found independently by all 3 inventory passes on this file. This conflates a genuine SDK/USB communication failure with an ordinary, expected case for a cross-model tool (source/target capability shapes legitimately differing) — no way to distinguish "device is malfunctioning" from "these two bodies just differ here." **[#13]**

- **[code+inventory+interface]** `src/transplant.rs:123-151` (`is_enum_shape`/`translate_enum`) vs `src/sdk.rs:849-897` (`decode_enum_values`) — a three-part cluster around the same architectural gap: (a) `elem_type` is used as a categorical dispatch field via `.unwrap_or(0)` instead of validating against the known SDK value set, unlike the stricter handling of sibling fields on the same struct; (b) a source/target `elem_type` disagreement (packed-string vs. raw-code) isn't explicitly detected — it falls through to the generic `NoMatchingOption`; (c) `decode_enum_values` can emit error-sentinel *objects* (`_invalid_physical_bytes`, `_overflow_computing_packed_string_length`, etc.) that `is_enum_shape` misclassifies as normal enum shapes, since it only checks that `"values"` is present, not that it's an array. All three collapse into vague, misleading `SkipReason`s (`MalformedSource`/`NoMatchingOption`) that reach users directly as distinctly-worded CLI warnings (`main.rs:1431-1444`), actively misleading operators about the real root cause. The write payload itself is unaffected (built from `target_live.clone()`, not the misclassified value) — this is a diagnostic-accuracy defect, not a wrong-write risk, but it's real and currently triggerable. **[#14]**

- **[code+interface]** `src/ptp_usb.rs:179` (`find_device`) vs `:929` (`select`, a hand-copied test-only re-implementation of the same match logic, needed since `find_device` requires real USB hardware) — no shared mechanism connects them. This has already caused a real, confirmed miss: `find_device`'s `id-{id}` serial-fallback bug (finding #09) was invisible to this entire test suite, because the tests only exercise the parallel copy, not the real function. **[#15]**

- **[interface]** `gui/fleet_gui.py:805-816` vs `src/main.rs:268-292` (`VendorWriteFtpArgs`) — currently fully consistent field-for-field, but no test invokes the compiled `fleet` binary from Python to verify this alignment holds. A future Rust-side rename or an `#[arg(long=...)]` override would silently break the GUI's argv construction with nothing catching it before a runtime clap-parse error. No subprocess-argv boundary test exists anywhere in this repo. **[#16]**

- **[interface]** `src/ptp_usb.rs` — no decode/parse function exists anywhere for the `0x90EE` FTP-profile blob. `write_ftp_profile` only checks the PTP response code (transaction accepted), not that any field was actually stored correctly. `encode_ftp_profile`'s only regression protection is two byte-for-byte comparisons against captured pcap hex — real protection for those two specific inputs, nothing verifies field combinations outside them (e.g. a host string that shifts a length byte's width, a port crossing a byte-order-visible boundary). A blind write on a reverse-engineered protocol writing physical camera network config. **[#17]**

- **[test]** `src/ptp_usb.rs:494-611` (`transact`, the `SESSION_ALREADY_OPEN` retry-once path, `read_vendor_property`, `write_ftp_profile`, `ping`) — zero test coverage of any kind. New, non-trivial protocol retry logic with no safety net beyond live hardware. **[#18]**

- **[test]** `src/ptp_usb.rs:385` (`ascii_bytes`, `FTP_ASCII_FIELD_MAX`=64) — surviving mutant: `s.len() > max` → `s.len() >= max`. All 26 `ptp_usb` tests stay green under the mutant. No test asserts a field of exactly 64 bytes succeeds — only the `max+1` rejection case is tested. **[#19]**

- **[test]** `gui/src/main.rs` (vendor-write handler) vs `gui/fleet_gui.py:704-720` — same defect as finding #10, independently verified by test review via direct reading: Python `.strip()`s FTP fields before sending, Rust's egui handler sends the untrimmed clone. Neither GUI is unit-tested; no parity test exists. **[#20]**

## Medium

- **[code]** `src/ptp_usb.rs:527-539` (`transact()`) — no aggregate timeout/iteration bound while waiting for a Response container; each individual `read_bulk` caps at 5s but the whole transaction doesn't. A malfunctioning device (or the desync from #02) could hang `transact()` indefinitely. **[#21]**
- **[code]** `src/main.rs:284`, `gui/fleet_gui.py:812`, `gui/src/main.rs:549` — FTP password passed as a plain `--password <value>` CLI arg, visible to other local users via `ps`/`/proc/<pid>/cmdline` during the write (narrower/more transient exposure than #06). **[#22]**
- **[code]** `src/main.rs:819-821` + `gui/src/main.rs:816-818` (`fn hex_bytes`) — defined byte-for-byte identically in both binaries despite both already importing from `nikon_fleet::ptp_usb`, an established shared path. **[#23]**
- **[code]** `src/ptp_usb.rs:507` (`read_container`) — allocates a fresh zero-initialized 16384-byte `Vec` on every bulk-IN read (hot path, 2+ calls per transaction with a data phase); `PtpUsbSession` already holds long-lived state that could hold a reused scratch buffer. **[#24]**
- **[code]** `src/ptp_usb.rs:563-601` — `if code != RESP_OK {...}` hand-copied 4 times instead of centralized in `transact()`; low risk today (3-4 call sites) but this module is an explicitly-growing reverse-engineering effort. **[#25]**
- **[code]** `src/ptp_usb.rs` module-level + `docs/nx-field-session-2026-07-09.md` — which vendor ops are confirmed USB-safe vs WiFi-only exists only as scattered prose/doc-comments, not a structured queryable table — directly underlies the model-safety gap (#03); a structured table would make correct model-gating easier to build and keep correct as the module grows. **[#26]**
- **[inventory]** `src/ptp_usb.rs:530` (`transact()`) — `data_in` accumulates response bytes with no size cap; a misbehaving device streaming unbounded DATA containers could exhaust memory with no signal. **[#27]**
- **[inventory]** `src/ptp_usb.rs:536` (`transact()`) — `_ => continue` on any unrecognized container kind loops indefinitely with no logging/error/iteration bound — an unrecognized protocol container becomes a silent, undiagnosable hang. **[#28]**
- **[inventory]** `src/transplant.rs:101,223,252` (`SkipReason::WriteFailed(String)`, property-name keys) — unbounded external strings (SDK error text, snapshot property names) with no truncation cap or overflow flag. **[#29]**
- **[inventory]** `src/transplant.rs:156-166` (`translate_range`) — `value`/`lower`/`upper` read as f64 with no finite check (NaN/Infinity) before clamp/divide arithmetic; likely surfaces downstream as an opaque SDK write rejection rather than a clear "malformed input" signal. **[#30]**
- **[inventory]** `src/transplant.rs:159` (`translate_range`) — target's `steps` uses lenient `.unwrap_or(0)` ("continuous") while `lower`/`upper` two lines above use strict `.ok_or(MalformedSource)` on the same struct — inconsistent strictness across sibling fields. **[#31]**
- **[test]** `src/transplant.rs:163` — surviving mutant: `steps >= 2` → `steps >= 1`. Full 25-test `transplant::tests` suite stays green; no test uses `steps == 1` or `== 2`. **[#32]**

## Low / Nit

- **[code]** `gui/fleet_gui.py:782` vs `gui/src/main.rs:524-530` — both GUIs independently hand-list `FtpProfile`'s "required" fields rather than deriving from one canonical definition. **[#33]**
- **[code]** `src/ptp_usb.rs:268-290` — `encode_command`/`encode_data` near-identical copy-paste (same 5-line header-assembly block). **[#34]**
- **[code]** `src/ptp_usb.rs:297-299` — stale `#[allow(dead_code)]`/comment on `Container.code`; it's actually read at `:534`. **[#35]**
- **[code]** `src/ptp_usb.rs:563` (`PtpUsbSession::ping`) — dead code, never called anywhere despite its doc comment claiming it's "exposed mainly for tests/diagnostics." **[#36]**
- **[code]** `src/main.rs:1432,1436,1442` — identical `eprintln!` warn line copy-pasted into 3 match arms. **[#37]**
- **[code]** `src/ptp_usb.rs:172,479` — `PtpUsbSession::open()` opens the target device twice per session (once to read the serial, again for the real handle). **[#38]**
- **[code]** `src/ptp_usb.rs:243,260` (`claim_with_retry`) — `last_err` captured per failed attempt (up to 40) but discarded; the returned error loses the actual `rusb::Error` reason. **[#39]**
- **[inventory]** `src/ptp_usb.rs:341-348` (`FTP_BLOB_MYSTERY_BLOCK`) — 82-byte opaque constant on the *encode* (outgoing) path, documented as byte-identical across 3 real captures, no live-response drift check. Low: encode-path not ingestion, assumption already backed by real capture evidence. **[#40]**
- **[test]** `src/ptp_usb.rs` (`FTP_HOST_MAX_UNITS`/profile_name caps) — only "above limit → rejected" is tested; at-boundary success is unpinned either direction. **[#41]**
- **[test]** `src/ptp_usb.rs:407` — `len_incl_null > u8::MAX as usize` is dead code for every current caller (all `max_units` ≤ 255); untested and unreachable today. **[#42]**
- **[test]** `gui/fleet_gui.py` — inline `0 <= port <= 0xFFFF` check duplicates the same expression in `gui/fleet_lib.py:150`; currently consistent, flagged as a maintenance-risk seam. **[#43]**

---

## Deferred findings still open (from 2026-09-03 ledger, unrelated to this pass)

Carried forward for visibility, not re-scored this pass: `#08` (tautology_sweep.py Python-only, confirmed this session the Rust suite has zero mock usage so real exposure is low), `#22` (cosmetic UI badge back-link field), `#50` (sdk.rs cb_noop, unverified callback signature).
