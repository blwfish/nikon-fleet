# Full Review — ptp_usb, baseline, and the GUI consumers

Scope: `src/ptp_usb.rs` (2,325 lines), `src/ops/baseline.rs` (586), `gui/src/main.rs` (1,846), `gui/fleet_gui.py` (854), `gui/fleet_lib.py` (285), read in full as a clean working tree at `48d0340`. `gui/test_fleet_lib.py` (512 lines) was read as the tests for the Python half. Callers and the other half of each boundary (`src/main.rs`, `src/snapshot.rs`, `src/firmware.rs`, `src/ops/{list,paths,target,cameras}.rs`, `mcp/src/*`, `docs/`) were read where a finding lives there and are cited with their own path.

Pass ID: `nikon-fleet-20261028-e5a3`

## Summary

| Pass | Critical | High | Med | Low | Status |
|---|---|---|---|---|---|
| Code review | 0 | 10 | 25 | 13 | ✓ ran (9 angles + severity re-pass) |
| Interface | 0 | 6 | 5 | 0 | ✓ ran (2 passes + severity re-pass) |
| Inventory | 0 | 0 | 9 | 6 | ✓ ran (3 Haiku passes per module + severity re-pass) |
| Test review | 0 | 10 | 6 | 2 | ✓ ran (95 mutants, 48 killed, 47 survived) |
| Coverage | — | — | — | — | ✓ ran (floor check only; Rust only, no Python coverage tool is configured) |

**92 distinct findings: 0 Critical, 26 High, 45 Medium, 21 Low.** Many were found independently by several angles; they are merged here and `found_by` in the ledger names the angles. Nothing was fixed in this pass, so every finding is `deferred`.

Test stats: pass 1260 (Rust 1166, Python 94), fail 0, mutants 48/95 killed, tautology sweep 0 hits.

Coverage (lines): `src/ptp_usb.rs` 88.05%, `src/ops/baseline.rs` 98.15%, `gui/src/main.rs` 46.95%, workspace 85.79%. `gui/fleet_gui.py` and `gui/fleet_lib.py` have no coverage figure (see #51).

## Notes on how it ran

- Phase 1 ran 9 angles over the five product files; the inline `#[cfg(test)]` blocks and `gui/test_fleet_lib.py` were left to angle 5 and Phase 3. A separate Sonnet pass assigned every severity; no finder's own label was carried forward.
- Phase 1.5 ran two interface passes: A on the seams between the three snapshot/firmware/settings implementations (CLI, egui, Tk) and B on the camera-facing boundary (`ptp_usb` against the MCP, the ops layer and the hardware docs).
- Phase 2 ran three Haiku finders per module on the data-capture and data-handling modules.
- Phase 3 ran the test audit with mutation, using the backup/restore-in-`finally` protocol and verifying every file with `diff` afterwards. The tree was clean before and after.
- The mechanical sweep that reads Python flagged `gui/fleet_gui.py` as an actively edited module with no test file (#51). The tautology sweep is Python-AST only and returned 0; it cannot see the Rust tests, so the vacuous-test findings (#18–#21) came from reading and mutation, not from it.
- No finding was verified on hardware. Where a description says *probed*, *mutation-confirmed* or *needs hardware*, that is what was done; the rest are read from the code and the repo's own hardware notes.
- The egui and Tk GUIs were not driven. Findings about what a user sees are from the code paths, not from a running window.
- Scope gaps: none. Every file in scope was read by every pass that applies to it.

## Which passes earned their keep

- **Interface** found 11 findings that no code-review angle was looking for, including the three-way drift between the CLI, egui and Tk implementations (#12 onward). The strongest result: the only check on what the Python GUI sends is a `help.contains(flag)` list, and renaming three flags survives it.
- **Test review with mutation** found 18 findings, ten of them High. Four are tests that cannot fail (#18–#21): with the import path filter disabled and a raw join used, all 55 GUI tests passed and a file was written outside the data dir. Code angles had no reason to find that; they read the tests as passing.
- **Inventory (Haiku)** earned its place on the GUI loaders and export/import: unchecked `bin_sha256`, a partial archive written over a backup on export, and startup crashes on odd files (#60, #66, #61). The Sonnet pass kept all of them at Medium or below.
- **Code angles**: 24 findings were found by three or more angles, and the High-rated code findings were mostly the multi-angle ones (the egui snapshot path, import overwrite, same-model pairing, the Python range display). Singletons were mostly Medium or Low.

## Deferred High findings and issues

All 26 High findings are `deferred`, since no fixes were requested for this pass. The repository has a remote (`origin`, github.com/blwfish/nikon-fleet) and the review procedure asks for a GitHub issue for each deferred High. None has been filed: creating 26 public issues is outward-facing, so `issue_url` is `null` in the ledger and the reason says why. File them, or fix the High tier, and update the ledger.

## High

- **#01** [Code] — `src/ptp_usb.rs:398-408` — `find_device` with no `--serial`, one openable body and one skipped body (held by ptpcamerad/icdd) proceeds on the openable one, including for `write_ftp_profile`; the only signal is an `eprintln!` that neither GUI shows. 7b2e#05 was recorded fixed. Untested (see #26).
- **#02** [Code] — `src/ptp_usb.rs:454-466` — The raw-USB vendor path takes no `CameraLock` and SIGKILLs ptpcamerad/icdd and re-asserts configuration 1 on every open; a GUI Vendor Ops click can disturb an MCP/CLI apply or restore holding the lock (the lock exists because a reset corrupts another session). Documented as deliberate without discussing this.
- **#03** [Code] — `src/ptp_usb.rs:141-145` — The write gate is a hand-typed `["Z 9","Z6_3"]`, one of five unlinked model lists, with no firmware dimension although the captures were firmware-specific; reads only warn for an unconfirmed model (warning invisible in both GUIs).
- **#04** [Code] — `src/ptp_usb.rs:593-631,969-997` — The FTP-profile write cannot be self-verified: slot 0 is hard-coded (every write overwrites it), the 82-byte block is copied from one capture and contains a MAC-shaped value and bytes that decode to 192.168.66.87, the original test rig's Z 9 address, shipped to every camera; limits are guesses; no read-back; `RESP_OK` is trusted as proof. The 331-byte capture variant is not modelled.
- **#05** [Code] — `gui/src/main.rs:774-834` — egui `do_snapshot` re-implements capture and gate and has drifted: capture stats discarded, no `read_ok == 0` refusal, and `NoBaseline` (the Z 5, changed firmware, or empty firmware) is saved and reported "Saved" with no note. The shared op, the CLI and the MCP worker all have the guard. The Python GUI's snapshot path similarly discards the gate verdict.
- **#06** [Code] — `gui/src/main.rs:714-732,778-790` — Same-model pairing is by enumeration order and `usb_camera_list()` silently drops unreadable bodies; the GUIs only `eprintln!` a warning and `do_snapshot` pairs with no warning or refusal, so one Z 9 can be stored under the other's serial (2 Z 9s attached). The MCP refuses; the CLI warns.
- **#07** [Code] — `gui/fleet_lib.py:113-131` — `fmt_cap_value` shows `value` for ranges, which is 0.0 on stepped ranges (33 of 39 range properties on a Z 9, all WBTune*); the position is in `value_index` (F18). Fixed in Rust (be50d12) and never in Python. The test passes only because it uses the midpoint index.
- **#08** [Code] — `gui/src/main.rs:1148; gui/fleet_gui.py:576-589` — Import silently overwrites existing snapshots, references and firmware (captures are create-new), validates nothing (an invalid JSON file counts as imported), has no size limit, and aborts midway with no report; rejected entries are not counted. An overwritten reference silently changes `check` results.
- **#09** [Code] — `gui/src/main.rs:486-590` — "Confirm Write" sends the current form to the currently selected serial: the confirm state is not reset when the camera or form changes and the confirm text names no target. With two Z 9s this can write a profile to the body the user did not confirm.
- **#10** [Code] — `gui/ + src/` — Three implementations (CLI, egui, Tk) of snapshot listing with the byte-identity reference rule, reference naming, export/import, capture, firmware reads, settings and serial fallbacks, kept in step by "must stay in sync" comments; the egui copies have already drifted from `ops::list::list_dir`.
- **#11** [Test] — `src/ptp_usb.rs tests` — The scripted transport never delivers what real USB does and the code relies on: a coalesced Data+Response (times out; the Response is dropped by `decode_container`), a ZLP after a 16384-byte container (`ShortContainer(0)`), a split header, a short write, or a reply carrying the real txid (`resp()` hard-codes 0). 11/12/13-byte and 16384/16385-byte boundaries are unpinned. Probed with temporary tests.
- **#12** [Interface] — `tests/gui_cli_argv_parity.rs; Makefile` — The only check on what `fleet_gui.py` sends is `help.contains(flag)` on a hand-copied list: renaming `--host` to `--hostname`, `--port` to `--port-number` and `--password-stdin` to `--password-stdin2` all survive (mutation-confirmed). No argv Python builds is ever run through the binary. And `gui/test_fleet_lib.py` (94 tests, the Python half of every parity pin) is run by neither `make test` nor `cargo test --workspace`.
- **#13** [Interface] — `src/main.rs:559; gui/src/main.rs:786; src/ops/target.rs:92` — Three serial-matching rules. The CLI and the GUI's inline copy take the first match over {USB serial | bare SDK id | id-N}: with body A (id 1, serial AAA) and B (id 2, serial "1"), `--serial 1` picks A. `ops::target` pins the opposite behaviour with a test; the other two have none.
- **#14** [Interface] — `gui/src/main.rs:46-89; gui/fleet_gui.py:33-77` — The `settings.json` contract between the two GUIs is comments only: Rust loads once, Python re-reads on mtime; Rust `save` drops keys Python keeps; a whitespace-only `data_dir` is unset in Rust and a real path in Python, so the two resolve different snapshot directories; Python hard-codes the macOS path.
- **#15** [Interface] — `gui/fleet_gui.py:343-376` — A second firmware-archive loader with a hand-copied `_FIRMWARE_META_FORMAT_VERSION`, no test (it imports tkinter), and different rules from `firmware::list_archives`: validates only `format_version` (`!=`, so `True` and `1.0` pass), defaults missing model/version to blank, lists the legacy flat layout the CLI ignores.
- **#16** [Interface] — `src/snapshot.rs:66-79; src/ops/list.rs:103; gui/fleet_lib.py:258` — Three snapshot readers accept different files: Python never checks `format_version`; `ops::list` needs `properties` and `transport`, the egui `SnapshotSummary` does not. The Python tests write version-less snapshots, pinning the lenient behaviour; no fixture covers a wrong version.
- **#17** [Interface] — `src/firmware.rs:187; src/ops/list.rs:38; src/ops/paths.rs:15; gui/fleet_lib.py:207` — The "plain path component" predicate is copied four times; the firmware copy already differs (no NUL check) and has no parity test.
- **#18** [Test] — `gui/src/main.rs:1623` — `import_rejects_path_traversal_entries_without_escaping_data_dir` is vacuous: it checks `dst/evil_sibling.txt`, so a `../evil_sibling.txt` entry escaping to dst's parent cannot fail it, and it never checks that `good.json` was imported. Mutant: disable the accept filter and use a raw join; all 55 GUI tests pass and a file was physically written outside the data dir.
- **#19** [Test] — `gui/src/main.rs:1306,1317` — The Rust `accept_zip_entry` depth tests pass for the wrong reason: `snapshots/subdir/foo.json` is rejected by the filename check, never the length check. Mutants `len == 2` to `>= 2` and `len == 4` to `>= 4` survive.
- **#20** [Test] — `gui/test_fleet_lib.py:56-135` — The Python `accept_zip_entry` tests are vacuous the same way: dropping the "." rule, the empty-component rule or the firmware depth rule survives. This is the only gate in `import_data`.
- **#21** [Test] — `gui/src/main.rs:1748` — `vendor_write_dry_run_still_works_without_any_cli_present` asserts `!text.contains("topsecret")` against a hex dump, so it can never fail; encoding the dry run without redaction survives all 55 GUI tests.
- **#22** [Test] — `src/ptp_usb.rs:864` — `TRANSACTION_TIMEOUT` has no test: replacing the deadline check with `false` survives. The unrecognised-container loop is bounded only by it.
- **#23** [Test] — `src/ptp_usb.rs:926,2299` — The retry OpenSession's Enforce is unpinned (`resp()` hard-codes txid 0, so a mismatching reply after CloseSession is never delivered), and the Drop-close test's Enforce mismatch is swallowed by `let _`.
- **#24** [Test] — `src/ptp_usb.rs:1046,1362` — `ftp_profile_missing_fields`: four of six names are untested (host, username, password, ssid_5ghz mutants survive). It is the required-field guard for the egui write, and the Python copy is not parity-tested.
- **#25** [Test] — `src/ptp_usb.rs:1569,708-735` — Per-field ASCII limits are untested: the "every length" sweep covers only `ssid_24ghz` (accept side); mutants raising the cap for ssid_24ghz, ssid_5ghz and username survive; non-ASCII rejection is tested for two fields.
- **#26** [Test] — `src/ptp_usb.rs:398` — `find_device` with one openable and one skipped body is untested and untestable as written (`candidate_devices` needs real rusb devices); see #1.

## Medium

- **#27** [Code] — `src/ptp_usb.rs:804-814` — `send_command`/`send_data` discard `write_bulk`'s byte count, so a short write is reported as success; no zero-length packet when 12+blob is a multiple of the max packet size (the blob length varies with the fields: 268 + 2·profile + 2·host + SSIDs + username + password; 512/1024 are reachable). Hardware behaviour unverified.
- **#28** [Code] — `src/ptp_usb.rs:816-838` — `read_container`'s inner loop has no cap and no deadline; the length is wire-controlled up to ~4 GiB; `MAX_TRANSACTION_DATA_BYTES` and `TRANSACTION_TIMEOUT` apply only around it, and the comments claim otherwise (7b2e#20/#26 recorded fixed). Needs a misbehaving device.
- **#29** [Code] — `src/ptp_usb.rs:565-568` — `decode_container` drops bytes past the declared length: a Response coalesced after Data is lost and the transaction waits out its timeout. A test pins the drop and the comment calls it legitimate.
- **#30** [Code] — `src/ptp_usb.rs:865-890,991-995` — The transaction id advances only on success, so after an error the next command reuses it; the 0x90E8/0x90EE writes are `TxidCheck::Unchecked` (ledger W1), so a stale reply is taken as the ack; no wraparound handling. OpenSession is Enforce, which catches leftovers first.
- **#31** [Code] — `src/ptp_usb.rs:378-395` — `select_candidate` takes the first match on a duplicate serial where `ops::target::select_by_usb_serial` returns `DuplicateSerial`; no test.
- **#32** [Code] — `src/ops/baseline.rs:192-216` — `check_against` is `pub` and never checks that the capture and baseline are the same model; the doc says the caller must. A wrong-model superset passes.
- **#33** [Code] — `src/ops/baseline.rs:195-215` — When the firmware differs from the baseline's, an incomplete capture (including empty firmware, which the CLI and egui fall back to because `DeviceInfo.version` is always "") is `NoBaseline`, never `Failed`, so an interrupted capture is saved with only a stderr note.
- **#34** [Code] — `src/ops/baseline.rs:239` — `baseline seed` writes `baselines/<slug>.json` and says "rebuild to use it", but the registry is a hand-kept `include_str!` list: a seeded Z 5 baseline is never read and no test notices a file that is not registered.
- **#35** [Code] — `src/main.rs:968` — `baseline seed --force` can replace a baseline from any snapshot (a partial capture, a restore point) without comparing it to the existing one; snapshots carry no partial marker.
- **#36** [Code] — `src/ops/baseline.rs:106-124` — The known-good set is whatever one capture read, measured only on one Z 9 and the Z6III in one state (the Z 9 B was never attached); a state-dependent capability could make a healthy capture fail the gate.
- **#37** [Code] — `gui/src/main.rs:761-768,799` — An empty USB serial is used as is where the CLI uses `id-N`; the GUI stores serial "", writes `Z_9__...` filenames, and its match on `Some("")` hits the first body with an empty serial (CLI fixed in 9d7cfd2, restore in be50d12, GUI never).
- **#38** [Code] — `src/main.rs:842` — `discover --json` prints the bare SDK id as the serial where every stored identity uses `id-N`; the Python GUI keys on it so a new snapshot never appears in the list it was taken from. The source comments claim discover prints `id-N`.
- **#39** [Code] — `gui/fleet_gui.py:765-768,822-841` — The Python GUI builds argv as `--flag value`; clap rejects a value starting with `-` (profile name, SSID, host, username, label): probed, `--ssid-24ghz -guest` exits 2. `vendor_argv.rs` fixed this for the egui GUI (165e731). Fails loudly.
- **#40** [Code] — `gui/fleet_gui.py:184-196; gui/src/main.rs:884-915` — Every CLI call is a blocking `subprocess.run` on the Tk main thread with no timeout, and `root.update()` keeps the buttons clickable so a double click starts a second camera session; the egui `run_cli` has no timeout either, so a hung CLI wedges the worker forever.
- **#41** [Code] — `gui/fleet_gui.py:447-456` — The overwrite prompt is decided by substring-matching the CLI's stderr ("already archived"/"already exists"); the producer is a typed `FirmwareError::AlreadyArchived`. `strip_sdk_prefix` likewise scrapes the first `{` line out of mixed stdout.
- **#42** [Code] — `gui/fleet_gui.py:391-411,539-589` — The download worker thread catches only `OSError`, so any other exception leaves `poll()` rescheduling forever; `export_data`/`import_data` have no exception handling; settings reads swallow every error.
- **#43** [Code] — `gui/src/main.rs:1046; gui/fleet_gui.py:293; src/main.rs:730` — All three `set_reference` writers build the filename from the snapshot file's own model/serial with no plain-name guard (a `/` or `..` serial writes outside `references/`), and replace an existing reference silently; the write side is untested.
- **#44** [Code] — `gui/src/main.rs:1077-1118` — `do_export` drops unreadable directories and entries silently and reports success ("Exported 0 file(s)").
- **#45** [Code] — `gui/src/main.rs:52-67` — `Settings::load` swallows parse errors and a later save overwrites the file; `save` ignores errors while the UI says "Data dir: ...". `Settings` has no tests.
- **#46** [Code] — `gui/src/main.rs:895,901-906` — A stdin write error is ignored when the child exits 0, so a truncated password write can be reported "Wrote FTP profile".
- **#47** [Code] — `gui/src/main.rs:744,776` — The egui discover/snapshot run the SDK in-process with the `CameraLock` held; DisconnectDevice hangs about 7% of runs (F24), leaving `busy` set and every other process seeing camera-busy. `sdk.rs` marks it a known gap; the GUI hang rate is unmeasured.
- **#48** [Code] — `gui/fleet_gui.py:159-176; gui/fleet_lib.py:258` — The Python GUI keys cameras by serial alone (an `id-N` serial can be shared by two models) and lists files the CLI and egui treat as unreadable (no `format_version` check); "Set as Reference" can copy them.
- **#49** [Code] — `gui/fleet_gui.py:432` — `Z6_3_0200.bin` is suggested as model "Z6 3" and stored as `metadata.model`, so `firmware ls --model Z6_3` and the rollback model check never see it.
- **#50** [Code] — `gui/src/main.rs:1173-1200; gui/fleet_lib.py:39-74` — `accept_zip_entry` has diverged: Rust's `Path::components()` collapses `.`, `//` and a trailing `/`, so Rust accepts `snapshots//a.json` and `firmware/Z_9//5.31/firmware.bin` and Python rejects them; the docstrings and test comments claim lockstep and that `components()` rejects CurDir (false). Write-path safety holds via `safe_join`.
- **#51** [Test] — `gui/fleet_gui.py` — The 854-line GUI (argv construction, discover, import/export, `_read_write_fields`) has no test file (mechanical sweep: actively edited, 17 commits); `fleet_lib` gaps: `parse_propcode` edge cases, `parse_fw_filename` with `²`, sort order, `fmt_cap_value` indices.
- **#52** [Code] — `gui/src/main.rs:947; gui/fleet_gui.py:733; src/ptp_usb.rs:1037` — Two code paths for the FTP dry run (egui encodes in-process, Python passes `--dry-run`), and the field list/trim rules are hand-copied in Rust, egui and Python (keys vs display names).
- **#53** [Interface] — `src/ptp_usb.rs:849-926,1000-1007` — No session poisoning and no `clear_halt`: after any transact error the unread container stays on the pipe and the session stays open; the NX doc records that a hung op stalls the bulk pipe and needed `clear_halt`, which the code never calls; Drop then sends CloseSession into a stalled pipe and the next process starts on it. The CLI is one op per process, so only library callers reuse a poisoned session.
- **#54** [Interface] — `src/ptp_usb.rs:981-997` — A write that fails after the 0x90EE data was sent looks identical (same error, exit 1, same GUI text) to one never sent, though the camera may have changed and the old profile cannot be read back; the MCP has `indeterminate` for exactly this. A false failure, not a false success; a retry is idempotent.
- **#55** [Inventory] — `src/main.rs:1117-1143; src/ptp_usb.rs:659-732` — The CLI vendor-write path never calls `ftp_profile_missing_fields`: all-empty fields give rc 0 and a blank-field blob that a real write would send; the encoder also accepts NUL and control bytes and 33-64-byte SSIDs (802.11 caps at 32). Only the GUIs guard.
- **#56** [Inventory] — `src/ptp_usb.rs:556,878-895` — A Data container's op code is never compared to the requested op, response params are discarded by every caller, and `Unchecked` write ops record no mismatch; OpenSession being Enforce limits it.
- **#57** [Interface] — `src/main.rs:71; gui/fleet_gui.py:33` — Only the GUIs read `settings.json`; the CLI defaults to `.` and the MCP requires `--data-dir`, so a plain `nikon-fleet snapshot` writes to `./snapshots`, which the GUIs never see, though the Python comment says the directory is shared with the CLI.
- **#58** [Interface] — `gui/src/main.rs:1063-1118; gui/fleet_gui.py:539-565` — Export/import is an allow-list of `firmware.bin` and `metadata.json`: `source-page.html` and the six legacy flat `firmware/*.bin` are left out of a user's "backup"; Python exports `firmware.bin` at any depth that import then skips; skipped entries are never counted.
- **#59** [Inventory] — `gui/fleet_gui.py:343-376,476` — `rglob` with no depth limit plus an iid taken from `parent.parent`/`parent`: a `metadata.json` at a non-4-part depth makes `remove_firmware` `rmtree` `fw_dir/B/C`, possibly another archive. A yes/no prompt shows the label; needs an unusual layout.
- **#60** [Inventory] — `gui/fleet_gui.py:360-367,576-589` — `bin_sha256` and `bin_size_bytes` are never verified by the GUI loader or import, so a truncated or replaced `firmware.bin` is listed as archived; matters for the rollback source.
- **#61** [Inventory] — `gui/fleet_lib.py:262-271,202-203; gui/fleet_gui.py:89,373` — Python startup crashes on odd files: `rows.sort` sits outside the try, so two same-second snapshots with a non-string label raise `TypeError` out of `_scan_known_cameras`; `parse_fw_filename` uses `isdigit()`, which accepts `²` that `int()` rejects, reached unguarded from the flat-layout loop; `discover()` assigns `self.cameras` before validating, so a `KeyError` desyncs the listbox.
- **#62** [Inventory] — `gui/fleet_lib.py:100-133; gui/fleet_gui.py:278-283,597-656` — The snapshot detail window crashes (inside a Tk callback) on malformed data and substitutes defaults silently: missing code shows `0x0000`, missing value `None`, a missing `values` renders `[index N / 0]`, indistinguishable from a failed GET_ARRAY; datetime/array/unknown shapes show a dict repr; the range shows only `value`.
- **#63** [Inventory] — `gui/src/main.rs:901-905; gui/fleet_gui.py:184-189` — stderr from a successful CLI run never reaches the user, including the unconfirmed-model warning for vendor reads (the Z 5 is 1 of 4 bodies outside the confirmed list) and the skipped-body warning.
- **#64** [Inventory] — `gui/src/main.rs:976-1044; gui/fleet_lib.py:213-272` — An unreadable snapshot directory shows as "no snapshots"; unreadable references and non-UTF-8 names are dropped; skip warnings go to stderr with no count; row order differs (Rust sorts the full string, Python `captured_at[:19]` plus tiebreakers).
- **#65** [Interface] — `src/ops/cameras.rs:31; docs/verify-a-model.md` — The Z 5 is `known_model: true` with no baseline signal in `list_cameras`, the plans or the guidance, yet every apply refuses `no_baseline`; and `docs/verify-a-model.md` tells the assistant to record `known_model` from `discover --json`, which does not emit it.
- **#66** [Inventory] — `gui/fleet_gui.py:550-565; gui/src/main.rs:1064` — Export truncates the destination zip before reading any source; a mid-write exception leaves a partial archive over the previous backup (no try in Python, no cleanup in Rust).
- **#67** [Test] — `src/ptp_usb.rs:552,820` — The 11/12/13-byte thresholds are partly pinned: a declared-length floor of `< 11` and a first read of exactly 12 header bytes survive.
- **#68** [Test] — `gui/src/main.rs:52,69` — `Settings` has zero tests; mutants that stop ignoring a blank `data_dir` or change the load-failure default survive. Settings selects where all snapshots are saved.
- **#69** [Test] — `src/ops/baseline.rs:528` — The baseline-names sweep skips with only an `eprintln!` when the gitignored `sdk-runtime/` is absent and nothing sets `REQUIRE_SDK_RUNTIME`; a mutant that makes the path look absent survives.
- **#70** [Test] — `gui/fleet_lib.py:77-150` — `fmt_cap_value` index boundaries are unpinned (index equal to length, negative, `<=`): mutants survive; a string or float index raises `TypeError`. Display only.
- **#71** [Test] — `src/ptp_usb.rs:168,629,630,685` — Policy constants are unpinned because the tests use the constants themselves: 16 MiB to 32 MiB, 64 to 65, 200 to 201, 200 to 255 and the `u8::MAX` guard `>=` all survive. Tuning limits, so Medium.

## Low

- **#72** [Code] — `src/ptp_usb.rs:654` — The dry run encodes `*` x `chars().count()`, so a non-ASCII password or one of at most 64 characters but over 64 bytes passes the preview and fails the real write (mutation-confirmed with the CLI). Fails loudly before the camera is touched; 3 call sites.
- **#73** [Code] — `src/ptp_usb.rs:304-331` — The `usb-<bus>:<addr>` fallback serial is printed nowhere, and both GUIs pass `id-N` as `--serial`, so vendor ops on a serial-less body always fail (loudly). 0 of 4 fleet bodies lack a USB serial.
- **#74** [Code] — `src/ptp_usb.rs:459-466` — `set_active_configuration(1)` error swallowed, configuration hard-coded, two `pkill -9` spawns on every open. Behaviour-sensitive; verify on hardware before changing.
- **#75** [Code] — `src/ptp_usb.rs:924,1001-1006` — CloseSession's response code is discarded and Drop can block up to 30 s. Teardown only.
- **#76** [Code] — `src/ptp_usb.rs:77-111` — `VENDOR_OPS`/`vendor_op_info` are dead (tests only; the drift test hard-codes five consts), `PtpUsbSession::model()` is unused, a doc comment sits on the wrong item.
- **#77** [Code] — `src/ptp_usb.rs:686` — `FtpFieldTooLong` reports bytes against a limit in UTF-16 units.
- **#78** [Code] — `src/ptp_usb.rs:349-366` — Efficiency: every Nikon body is opened to read its serial (4 control transfers where 3 do), payloads copied 3x, magic 16384 and 12.
- **#79** [Code] — `src/ops/baseline.rs:49-51` — "Sorted by code" is documented but `validate()` checks only uniqueness; an unsorted file validates and `missing` comes out unsorted (probed). Shipped baselines are sorted.
- **#80** [Code] — `src/ops/baseline.rs` — Doc drift (it says apply refuses an unjudged capture; `AllowPartial` exists), `MissingCapability` duplicates `BaselineCapability`, `GateOutcome::tag()` unused, plural phrasing duplicated.
- **#81** [Code] — `gui/fleet_gui.py:779` — The Python port parse accepts `2_1`, `+21` and non-ASCII digits that Rust rejects; the CLI receives the normalised value; port 0 is accepted by both GUIs.
- **#82** [Code] — `gui/fleet_gui.py:199-253,713` — After a zero-camera discover `self.selected` keeps the old index and `take_snapshot` / `VendorOpsWindow` raise `IndexError` in a Tk callback. No data effect.
- **#83** [Code] — `gui/` — Efficiency: the egui `list_snapshots` re-parses every snapshot per click, export reads whole firmware files into memory, a spinner repaints every frame, Python parses twice at launch.
- **#84** [Code] — `gui/ + src/` — Small items: dead `_fw_meta`, three copies of the macOS default path, a flat-layout branch the CLI ignores, hard-coded example model strings, no `NIKON_FLEET_CLI` override in Python, a 400-line `update()`.
- **#85** [Inventory] — `src/ptp_usb.rs:1071-1078` — `parse_propcode("5010")` is read as decimal 5010 (0x1392), not the documented 0x5010. Read-only; documented decimal acceptance.
- **#86** [Inventory] — `src/ptp_usb.rs:583-590,705-732` — The 331-byte 0x90EE variant (an extra optional UTF-16 field, port 56171) is not modelled; the "byte-for-byte" claim holds for two of three captures.
- **#87** [Inventory] — `src/ptp_usb.rs:418-445,892-900` — The interrupt IN endpoint is never opened, so PTP events (DevicePropChanged) never arrive and the Event arm is unreachable; unknown container kinds are skipped without a counter.
- **#88** [Inventory] — `src/ptp_usb.rs:491,501` — `payload.len() as u32` wraps above 4 GiB and `encode_command` accepts any number of params; unreachable today.
- **#89** [Inventory] — `src/ptp_usb.rs:352-366,969` — Descriptor read failures collapse to `""` or the topology fallback serial, `rusb` errors are discarded, and `read_vendor_property` returns `Ok(empty)` when no Data container arrived.
- **#90** [Inventory] — `src/ptp_usb.rs:256-271` — Only 13 PTP response codes are named; others print as hex.
- **#91** [Test] — `src/ptp_usb.rs:2299` — The Drop-close test replies with txid 0 against an expected 1, so the Enforce mismatch is swallowed by `let _`; mutating Enforce to Unchecked on close survives. Equivalent by design.
- **#92** [Test] — `gui/fleet_lib.py:29,207` — `model_slug` replacing only the first space, `_plain_name_component` without the backslash/NUL rules, and the range branch without `upper` survive; exposure macOS-only and no two-space model.

## Remediation

Run `nikon-fleet-remediate-20261028-d1a7`, fix commit `9da8a99`, ledger commit `04bf6b2`. Nothing is pushed.

Of the 92 findings above, **85 are fixed, 5 deferred, 2 wontfix**. Two cold reviews of the remediation itself found 19 more, recorded in the same ledger entry as `nikon-fleet-remediate-20261028-d1a7#R01..R19`: **16 fixed, 3 deferred**. All 26 High findings are fixed except #04; every Medium is fixed except #36.

Verified: Rust 1419 tests and Python 652 tests pass (`make test` now runs both); the tautology sweep is clean; the clusters ran about 700 one-at-a-time mutations against the new tests and I re-ran nine of them myself (argv parity flag rename, zero-length packet, zip component check, baseline model check, Python port bound, exit-3 prompt, write_atomic symlink and temp name, the parse-only environment gate), all killed.

**Not verified, needs a camera or a bench:**
- The zero-length-packet rule follows PIMA 15740; no hardware has shown that a body needs it.
- `PtpUsbSession::open` taking the CameraLock before the bus is tested with an injected lock; the real path was not run.
- The egui GUI now runs discover and snapshot as CLI subprocesses. Its timeout, kill and partial-output salvage are tested with fake CLIs only, and the USB reset now happens in the CLI process.
- Exit status 4 (indeterminate FTP write) is tested as a mapping; a real indeterminate write was not provoked.
- No GUI window was driven; the egui UI wiring (button enablement, busy counting) has no automated test beyond the pure logic it calls. The Python GUI tests drive a real Tk root with a fake CLI and fail instead of skipping when `CI`, `REQUIRE_DISPLAY` or `REQUIRE_CLI` is set.
- Two Python tests failed once each under heavy load and did not reproduce in about 25 reruns; the test helper's wait ceiling was raised to 60 s, which is not proven to be the cause.

**Still deferred, and what it would take:**
- #04 (High): the FTP-profile block still ships the capture rig's address as its default; what `[0,0]` means, the real field limits and the 331-byte variant need a bench session. No GitHub issue is filed (public posting).
- #36: the baseline's known-good set is state-dependent; needs captures per model in the states a user reaches, and the second Z 9.
- #74: the hard-coded USB configuration 1 and the two `pkill` spawns per open need a hardware experiment.
- #86, #87: the 331-byte variant's extra field and the interrupt endpoint need a capture or a bench session and a design.
- R17: `restore` targeting an `id-N` snapshot serial can land on the wrong body when two same-model bodies are attached (pre-existing; needs a design decision).
- R18, R19: two Low items in the vendor path (bus touched before the lock; a failed descriptor read reported as "not found").

**Behaviour changes worth a look:** `discover --json`'s `serial` is now the stored identity (`id-N` when there is no USB serial), not the bare SDK id; `vendor-write-ftp` rejects SSIDs over 32 bytes and control characters; `make test` needs pytest; the CLI's `--data-dir` default stays `.` on purpose (see `nikon-fleet settings show`).
