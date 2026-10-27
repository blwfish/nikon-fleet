# Plan: optional MCP server for nikon-fleet

Status: **plan only, nothing built.** Written 2026-10-27. Per the spec-review rule this needs a
fresh-session review (esp. §8, external-system assumptions) before implementation starts. A first
cold review against the code was done 2026-10-27 and its corrections are folded in below (marked
*[review]*); the open items in §8 still need verification, two of them on hardware.

## 1. Why, and what it must not become

Motivating requests (user, 2026-10-27):

- "What are the differences between these two Z 9s?"
- "Make my Z6III as close to this Z 9 as you can, and tell me what did not get copied."

Both are already core operations (`diff`, `transplant`). The MCP is a natural-language front end
over them, in the same way the GUI is a graphical one.

Note the first request needs two snapshots, so it is not answerable until `capture_snapshot`
(Phase 2) unless snapshots already exist on disk. *[review]*

**Constraint:** nikon-fleet must run with no AI assistant (see memory `project-no-ai-runtime`). So
the MCP is a separate, optional binary. No core path, GUI feature, install step or documentation
flow may depend on it. Deleting the crate must leave the CLI and GUI unchanged.

## 2. Finding that shapes the plan: the orchestration is in `main.rs`, not the library

`lib.rs` exports primitives (`sdk`, `snapshot`, `diff`, `transplant::transplant`, `firmware*`). The
*workflows* are in `src/main.rs`: `cmd_snapshot`, `cmd_check`, `cmd_restore`, `cmd_transplant`
(device selection, the ambiguous-target refusal `transplant_target_ambiguous`, outcome counting,
printing). So "a thin wrapper over the library" is not yet true. If the MCP re-implemented these it
would be a second copy of each rule (CLAUDE.md: Parallel Implementation / single source of truth).

**Phase 0 therefore extracts the workflows into a library module (`ops`)** that return structured,
serialisable results. The CLI becomes a printer over them and the MCP a JSON adapter over them.
This also closes the `--json` gaps (only `discover`, `diff` and `firmware latest` have it today).

Several rules already exist in more than one place and Phase 0 should collapse them rather than add
a third copy: the "snapshot is the reference iff byte-identical to the reference file" rule lives in
both the Python `gui/fleet_lib.py` and the Rust GUI's `list_snapshots`. `ops::list` becomes the one
implementation and the GUI migrates onto it (or the plan records explicitly why not). *[review]*

## 3. Architecture

- New workspace member `mcp/` (crate `fleet-mcp`), like `gui/` (the Rust `fleet-gui` crate).
  Depends on `nikon-fleet` as a library. stdio transport, run by the MCP client
  (`fleet-mcp --data-dir … --sdk-bundle …`).
- **macOS only for anything touching cameras.** The MAID SDK is loaded as a CFBundle/Mach-O dylib
  (see the `libloading` comment in `Cargo.toml`). Snapshot/check/restore/transplant cannot run on
  Windows; offline tools (diff, ls) could. `firmware_status` needs network, not a camera.
- **Read-only by default.** Write tools are registered only with `--enable-writes`. Client-side
  approval prompts are a second layer, not the protection (clients differ in how they treat
  `destructiveHint`).
- **One SDK worker thread.** *[review]* `sdk.rs` documents the SDK as not thread-safe and single-
  threaded, and the CF run-loop pump (`CFRunLoopRunInMode`) is per-thread: the IOKit notification
  is registered on whichever thread calls `InitializeSDK`. So all SDK use goes through a dedicated
  `std::thread` worker that owns the `Sdk`, fed by a channel; async handlers send a request and
  await the reply. This replaces the earlier "process-wide mutex + `spawn_blocking`" idea (tokio's
  blocking pool gives an arbitrary thread per call) and makes the Phase 4 persistent session a
  small change. **Hardware check 2026-10-27 (§8): an SDK call can hang indefinitely inside
  `DisconnectDevice` after a concurrent open from another process, and cannot be cancelled from
  within the thread**, so a worker *thread* is necessary but not sufficient: put the SDK worker in
  a killable child process with a watchdog, and take a cross-process lock in `--data-dir`.
  Session lifetime is **per call** at first (open SDK, do the work, drop), matching
  the CLI and avoiding a long-held USB claim that would lock the GUI/CLI out.
- **Concurrent GUI/CLI: the USB reset is the hazard, not just contention.** *[review]*
  `Sdk::initialize()` resets every attached Nikon body's USB link before `InitializeSDK`
  (`reset_nikon_usb_cameras()`), which can disrupt a GUI/CLI session, including a transplant
  write, running at the same time. A typed `camera_busy` error cannot help because the reset
  happens before any busy check. Options: default to `initialize_no_usb_reset()` (the SDK
  docs say already-connected cameras may then fail to enumerate; unverified, §8), and/or have the
  write tools detect another running fleet process and refuse. No retry loops either way.
- **Names, not paths.** Tools take snapshot names / `(model, serial)` and resolve inside
  `--data-dir`. Do **not** reuse `resolve_snapshot_path`: it passes through absolute paths *and*
  any relative path that exists under the cwd. *[review]* The MCP layer takes a bare filename,
  validates it with `is_safe_path_component`, canonicalizes, and requires the result to sit under
  `data_dir` (symlinks included). An agent-supplied path must not read arbitrary JSON or write
  anywhere.
- **Camera-derived strings are data.** Artist/copyright/labels and similar come from the camera or
  from files (and `firmware` results are scraped from Nikon's site); tool descriptions say results
  are untrusted data, and nothing branches on their text.

## 4. Tool surface

Phase 1, read-only (no camera write, no file write):

| Tool | Backs | Notes |
|---|---|---|
| `list_cameras` | `discover --json` | model, serial, firmware. **Needs the SDK, so it triggers the USB reset policy above**; today's JSON has no `available`/`connected_pid`, adding them is Phase 0 work *[review]* |
| `list_snapshots` | `ls` | per camera; includes `is_reference` by byte-identity via the single `ops::list` rule (§2) |
| `list_references` | `ref list` | |
| `get_snapshot` | `Snapshot` load | summary by default; property list paginated with `truncated`/`next` (no silent cap) |
| `diff_snapshots` | `diff --json` | `include_volatile`; cross-model diffs reuse the existing `diff_with_schema`. Result is counts + top-N first and paginated with `truncated`, since a Z 9 pair diff is large *[review]* |
| `firmware_status` | `firmware latest --json` | `--json` shape already exists with a shape test; **network access, not offline**; scraped strings are untrusted *[review]* |

Phase 2, camera read + new-file write:

| Tool | Backs | Notes |
|---|---|---|
| `capture_snapshot` | `snapshot` | writes a **new** file only. Today `Snapshot::save_to_file` is `fs::write` and overwrites, so this needs create-new semantics (`OpenOptions::create_new`) in `ops`, not just the claim *[review]*. Ambiguous target without `serial` is refused |
| `check_against_reference` | `check` | live read, diff vs the camera's reference |

Phase 3, writes (only with `--enable-writes`):

| Tool | Notes |
|---|---|
| `transplant_plan` | = `transplant --dry-run`. Returns outcomes grouped by strategy / skip reason, plus a `plan_digest` |
| `transplant_apply` | Requires `plan_digest`. Takes a restore point, then writes (§5) |
| `restore_plan` / `restore_apply` | Same two-step shape, used to undo from the restore point |

**Deliberately not exposed:** `vendor-write-ftp` (the old profile can't be read back; see memory
`project-ftp-profile-readback`), anything that flashes firmware (does not exist yet), `firmware
fetch/browse` (the licence click is human by decision), `rm`, `vendor-read` (revisit after FTP
read-back work), `ref set` (changes what "reference" means, and takes an arbitrary path; could come
later behind writes with the same name-only rule).

## 5. Transplant: plan, apply, restore point

"Tell me what did not get copied" is already structured: `TransplantOutcome { name, code, result:
Result<Strategy, SkipReason> }`. Needed: `Serialize` on `Strategy`/`SkipReason` as **closed string
tags** with a separate typed detail field (e.g. `write_failed` + `detail`), not the Display text.
The tag mapping is an **exhaustive `match` with no wildcard arm**, so a new `SkipReason` variant
fails to compile until mapped. *[review]*

Report shape: counts per tag, then the full per-property list grouped by tag. `absent_on_target`
and `read_only` are routine and numerous, so lead with counts and put the warn-worthy tags
(`no_matching_option`, `write_failed`, `enum_type_mismatch`, `enum_undecoded_by_sdk`,
`name_mismatch`, …) first. A paginated list sets `truncated`.

**What `plan_digest` is, and is not.** *[review]* It is a **staleness guard**: it proves the camera
or snapshot has not changed since the plan was computed. It is **not** a consent mechanism: an
agent can call plan then apply back-to-back without a human seeing the plan. The real consent gates
are `--enable-writes` and the client's approval prompt. If an unreviewed apply must also be
prevented server-side, the plan result carries a short-lived single-use token that `transplant_apply`
requires; the tool description tells the agent to show the plan to the user first. Decide which
before building; do not describe the digest as the protection.

Digest input must be **stable**: hash(source snapshot bytes, target serial, per-property
`(name, code, tag, structured fields)` in `BTreeMap` order). Never hash the SDK error text carried
by `TargetReadFailed(String)` / `WriteFailed(String)`, or a transient message produces a false
`stale_plan`. *[review]*

Apply sequence (all in one call on the SDK worker):

1. Recompute the dry run on the live target; compare `plan_digest`. Mismatch ⇒ refuse with
   `stale_plan` (camera state or snapshot changed since the plan the user saw).
2. Capture the target's current state as a snapshot labelled `pre-transplant-<timestamp>`
   (the **restore point**), written create-new. **Abort before any camera write** if the file
   write failed or the capture is *worse than that body's known baseline* (unchecked-result rule).
   Note that `cmd_snapshot` today writes the file and bails only when *every* read failed, so a
   half-failed capture reports success; `ops::capture` must expose read_ok/read_err and the set of
   failed codes so the gate can be strict. **"Zero read errors" is not a usable gate:** a healthy
   Z6III (fw 2.00) always reports `ok=200 err=12 skipped(no-get-bit)=17`, deterministic across
   runs (§8, 2026-10-27 hardware check). The gate is instead: every failed code is in the model's
   known-unreadable set (or the same set as the camera's reference), and `read_ok` has not dropped
   below that baseline. The result reports the unreadable codes and the no-get-bit count so the
   user knows what the restore point does *not* cover. *[review]*
3. Run the real transplant; return the outcomes plus the restore point's name.
4. `restore_plan`/`restore_apply` on that name is the undo.

Honest limits to state in the tool description: the restore point holds only what the SDK let us
*read*; write-only or unreadable properties cannot be restored. **Restoring writes every
snapshotted property, not just the ones transplant touched, so any change made to the camera after
the restore point was taken is overwritten.** *[review]* Whether re-applying a snapshot returns
every written property to its prior value has not been verified on hardware (§8).

Restore-point files land in `snapshots/` and would appear in `ls` and the GUI list. Give them a
distinct label prefix and check that nothing treats "latest snapshot" as meaningful, or filter them
from the default lists. *[review]*

## 6. Phases

0. **Extract `ops`** (library): `capture`, `check`, `restore`, `transplant`, `list`; structured
   results; `Serialize` on outcomes; CLI re-pointed at them; `--json` on `ls`, `ref list`, `check`,
   `snapshot`, `transplant`, `restore`; `discover --json` gains `available`/`connected_pid`;
   `ops::capture` returns read stats and uses create-new writes; one `ops::list` byte-identity rule
   (GUI migrates onto it). Introduce a small camera-port trait
   (`capabilities`/`read_capability`/`write_capability`) so plan/apply and the restore-point logic
   are testable without a `Device` (today `transplant()` is documented as not unit-testable).
   *Must wait until the firmware session's uncommitted edits to `src/main.rs` (and `lib.rs`,
   `firmware.rs`, `gui/`) are committed.*
1. `mcp/` scaffold + SDK worker thread + the Phase 1 tools, read-only registration only.
2. `capture_snapshot`, `check_against_reference`.
3. Write tools behind `--enable-writes`.
4. Optional: persistent SDK session (cheap once the worker thread exists); `vendor_read`.

## 7. Tests (threshold / ambiguity list, written before code)

- Read-only mode registers none of the write tools; `--enable-writes` registers them.
- Path outside `data_dir` (absolute, `..`, symlink, **relative path that exists under the cwd**)
  rejected for every path-taking tool.
- Two cameras attached and no `serial` ⇒ refused for capture and transplant.
- `transplant_apply` with: matching digest (proceeds), digest differing by one outcome (refuses),
  missing digest (refuses), restore-point capture failure (no write happens), restore-point file
  write failure (no write happens), **restore-point capture with partial read errors (no write
  happens)**, **restore-point filename already exists (no overwrite, no write happens)**.
- Digest stability: two dry runs with identical state but different `WriteFailed`/`TargetReadFailed`
  text produce the same digest.
- Empty snapshot properties; all-skipped plan (apply is a no-op and says so); source model equals
  target model (decide whether the CLI's "use `restore`" note is returned); non-Z-series model
  rejected (existing guard).
- Pagination: exactly-N and N+1 properties around the page size; `truncated` correct at both, for
  `get_snapshot`, `diff_snapshots` and the plan list.
- Outcome tags: every `SkipReason` variant round-trips to a distinct tag (exhaustive match; a new
  variant must fail to compile until mapped).
- Parity: CLI `--json` and MCP tool results come from the same `ops` value (one source of truth);
  `is_reference` agrees across `ops::list`, the Rust GUI and `fleet_lib.py` for the same fixtures.
- Live (needs the user and hardware): Z 9 ↔ Z6III plan/apply/undo on real bodies.

## 8. External-system assumptions to verify before building

- MCP Rust SDK: assumed `rmcp` (the official `modelcontextprotocol/rust-sdk`), tokio-based, stdio
  server, tool annotations. **Not checked**; confirm crate name, maturity and API first.
- Tool annotations (`readOnlyHint`, `destructiveHint`): assumed advisory only. The server-side
  `--enable-writes` gate does not rely on the client honouring them (and `plan_digest` is a
  staleness guard, not a gate; §5).
- `Sdk`/`Device` thread behaviour (`Send`, main-thread requirements for the CFBundle/IOKit
  notifications that `initialize()`'s USB reset exists to trigger). A dedicated worker thread that
  both initialises and pumps is assumed to work; **unverified**.
- **USB reset vs. enumeration. VERIFIED 2026-10-27 (Z6III, fw 2.00, release build, plain CLI
  process, no AppKit).** `--no-usb-reset discover` enumerates the already-connected body (3 runs,
  ~1.8-3.7 s), same as with the reset. So `initialize_no_usb_reset()` is viable as the MCP default
  and the concurrent-reset hazard (§3) can be avoided.
- **A reset-initialised open during another process's snapshot corrupts that snapshot silently.
  OBSERVED 2026-10-27 (read-only test, 2 trials, `discover` WITH reset while a no-reset
  `snapshot` ran).** Reset at +1.0 s: snapshot completed normally (200 ok / 12 err, but 8 s instead
  of ~5 s). Reset at +2.5 s: the snapshot's `connect` succeeded but it printed **"Reading 0
  capabilities"**, wrote a 232-byte empty snapshot and **exited 0**, because
  `snapshot_capture_failed` is `read_ok == 0 && read_err > 0` and a 0/0 capture slips through.
  Two consequences: (a) the concurrent-reset hazard in §3 is real, so the MCP must never reset
  while any other fleet process may hold a camera (default no-reset; the cross-process lock covers
  the CLI/GUI too); (b) **the restore-point gate must treat `read_ok == 0` (and anything below the
  model baseline) as failure, not just read_err > 0**.
  **CLI fixed 2026-10-27 (uncommitted, `src/main.rs`):** `snapshot_capture_failed` is now
  `read_ok == 0`, and a new `ensure_capabilities_present` aborts `snapshot`/`check`/`restore`/
  `transplant` right after `connect` when the device reports 0 capabilities, before anything is
  read, written or saved. Re-run against the Z6III with the reset landing at +2.4-2.7 s: 3 of 6
  trials hit the 0-capability race and each now exits 1 with an actionable message and no file
  written. **The other 3 trials showed a second, unfixed failure mode: the reset landed a little
  later and `snapshot` captured a *partial* snapshot (`ok=18 err=194`, `ok=77 err=135`,
  `ok=101 err=111`), wrote it, and exited 0.** That is the real case the §5 restore-point gate
  must catch: `read_ok > 0` is not enough; compare against the model baseline
  (`ok=200 err=12` on this body, identical failed codes). The CLI `snapshot` itself still accepts
  such a capture, which is worth a warning or non-zero exit once a per-model baseline exists.
- **Concurrent SDK opens from two processes: NOT safe. OBSERVED 2026-10-27.** One `snapshot` ran
  while a second process opened the SDK (`discover`, no reset). Both calls returned correct
  results, but in one of two trials the snapshot process finished its work, wrote its file, then
  **hung forever in `Drop for Device` -> `DisconnectDevice` -> `MaidCloseObjectEntryPoint`,
  parked in `CFRunLoopRunSpecific`** (main thread, ~2 min, slowly burning CPU) until killed
  with SIGTERM. The camera was healthy afterwards. n=2, so the rate is unknown; the other trial
  was clean. **Follow-up loop (same day, 10 pairs, `discover` started 0.3-3.8 s after the
  snapshot, 30 s hang limit): 1 of 10 hung, at the earliest offset (0.3 s, i.e. `discover` opening
  the SDK while the snapshot was still initialising/connecting); offsets 0.8-3.8 s were clean. The
  hang had the identical stack (`Drop for Device` -> `DisconnectDevice` -> `MaidCloseObjectEntryPoint`
  -> `CFRunLoopRunSpecific`), and the camera enumerated normally afterwards.** Combined with the
  first check that is 2 hangs in 12 overlapping runs (about 17%, wide uncertainty), not purely
  determined by start offset: the first hang came at ~2.0 s, while 2.0 s was clean in the loop.
  The hang is always in the process that held the camera at the time of the second open, never in
  the newcomer. Consequences for the plan: (a) "fails cleanly rather than wedges" is **false**; the
  failure mode is a hang in the holder, not an error in the newcomer; (b) every SDK call needs a
  watchdog, and because a call stuck inside the SDK cannot be cancelled, the worker should be a
  **child process** the MCP can kill and respawn (or the MCP must mark itself poisoned and
  exit), otherwise one hang blocks the whole stdio server; (c) cross-process exclusion needs an
  advisory lock file in `--data-dir` taken by the MCP, the CLI and the GUI alike (a CLI/GUI
  change, so it is Phase 0 work), not just an in-process queue; (d) worth reproducing with a
  deliberate loop before relying on any of this.
- **Restore-point fidelity: VERIFIED for one property, same-model, 2026-10-27 (Z6III fw 2.00).**
  Baseline snapshot B0; a hand-edited one-property snapshot changed `HighlightBrightness`
  (playback display threshold) from index 26 to 25 via `restore` (written=1, errors=0); a fresh
  snapshot B1 read the new value back (diff B0 vs B1: only that property, plus the garbage
  field below). A **full** `restore` of B0 (the restore-point undo) then followed, and a fresh
  snapshot B2 was **identical to B0 ("no differences")**. So undo-by-full-restore returned the
  changed property to its prior value, and re-writing the other ~114 properties did not disturb
  anything detectable. Not covered: cross-model `transplant` (needs the Z 9 as source), any
  property class beyond one enum, unreadable properties (below), and a restore after the camera
  was power-cycled. Observations that change the design:
  - **The dry run does not predict failures.** `restore --dry-run` said written=119, errors=0;
    the real run wrote 115, skipped 2 as `unsupported-type` and had 2 errors
    (`CurrentItemID` 0x8351 and `LiveViewStatus` 0x823e, `SetCapability` -125, i.e. runtime state
    that should never be restored), and still **exited 0** (`restore_completely_failed` needs
    written == 0). So `restore_plan`/`transplant_plan` can only promise "would attempt"; the apply
    result must report actual written/skipped/error counts and codes, and the tool description
    must not present the plan as a prediction. Add a never-restore denylist for runtime-state
    capabilities so the restore point doesn't generate known errors.
  - **Diffs on an unchanged camera are noisy in two properties** (7 snapshots compared):
    `AngleLevel` (live level sensor, differs every capture, expected) and
    `MovieWBFluorescentType`, whose **`default_index` is garbage** (observed 0, 1, 26, 72350,
    50331648) while `value_index` is stable, so the SDK decode likely reads an uninitialised
    field for that 3-element enum. **Root cause confirmed 2026-10-27:** the SDK never writes
    `NkMAIDEnum.ulDefault` for this capability (stale heap: both the GET_VALUE and GET_ARRAY
    structs carry the same junk within a call, and 26 is another capability's real default), no
    capability operation bit distinguishes it from the 69 fine enums, and re-reading cannot detect
    it. So it cannot be fixed at the source; **mitigated 2026-10-27 (uncommitted, `src/diff.rs`):**
    `diff` (and so `check`) ignores an enum's `default_index` unless `--include-volatile`; the
    field stays recorded raw in snapshots so old and new files remain comparable. Range values'
    `default_index` is not ignored (no garbage seen there). `plan_digest` must still hash only
    stable fields, and the unchanged-camera diff on the Z6III is now empty.
- Restore-point coverage data point: a Z6III snapshot reads 200 capabilities and fails 12 identically on every run:
  eleven `-126` (incl. `PictureControlDataEx2` 0x8501, `MoviePictureControlDataEx2` 0x8502,
  `PortraitImpressionBalanceDataSet`, `SpecialShootingSettingInfo`, `GetStorageInfoDataSet`,
  the SB attr set) and one `-117` (`ResetWBMode`), plus 17 with no get-bit and 5 not in
  `MaidLayer.config`. **Picture Controls are not readable through this path**, so they can be
  neither diffed, restored from a restore point, nor (probably) transplanted, which matters for
  "make my Z6III like this Z 9". Confirm whether transplant reports them as skips and say so in
  the user-facing result.
- **Z 9 A (serial 3023668, fw 5.31) baseline, read-only, 2026-10-27 (no Z 9 SDK snapshot existed
  anywhere before this; `references/` only had a July PTP text dump).** 13 captures, `--no-usb-reset`:
  - Shape: **227 capabilities, `ok=196 err=11 skipped(no-get-bit)=20 unknown-schema=5`**, 196
    properties, ~4-5 s per snapshot. The 11 failures are identical every run, all `-126`, and are
    the **same 11 codes as on the Z6III** (`AcceptDiskAcquisition`, the four SB attr caps,
    **`PictureControlDataEx2` and `MoviePictureControlDataEx2`**, `PortraitImpressionBalanceDataSet`,
    `SpecialShootingSettingInfo`, `GetStorageInfoDataSet`, `GetStorageIDS`). The Z6III additionally
    fails `ResetWBMode` (-117). So the Picture Control unreadability is a property of this SDK path,
    not of one body: a Z 9 -> Z6III transplant cannot carry Picture Controls either way, and the
    user-facing "what did not get copied" report must say so explicitly rather than only listing
    per-property skips.
  - Per-model baselines differ (200/12 vs 196/11), so the §5 gate needs one per model, keyed by
    model (and probably firmware).
  - Volatile fields on an untouched Z 9: `AngleLevel` and `AngleLevelPitch` (both already hidden by
    the `AngleLevel` substring). **The `MovieWBFluorescentType` `default_index` garbage does not
    occur on the Z 9** (it is a Z6III-specific uninitialised field), so that diff fix matters
    only for some bodies. Ten of ten solo captures diff clean against each other.
  - **First capture of the session differed and then hung.** The first run's
    `LiveViewZoomArea` options were `[0,256,512,1024,2048]` instead of the `[0,320,640,1280,2560]`
    seen in the other 12, and **that process then hung in teardown for the full 120 s watchdog
    (CPU spinning, as in the Z6III hangs) with NO other process using the camera.** The next 12
    runs (2 + 10 solo) were normal. So (a) the teardown hang is **not exclusively a
    concurrency problem**: 1 solo hang in 13 on the Z 9, alongside 2 in 12 overlapped on the
    Z6III; (b) a plausible but **unproven** link is "first SDK open after the body was attached or
    woken": its state differed too. Consequence: the watchdog / killable-child design in §3 is
    required even with a perfect cross-process lock, and the lock alone must not be presented as
    the fix. Worth a targeted test: replug or power-cycle, then capture several times.
  - Not yet done: Z 9 B, two bodies attached together, and any Z 9 write.
- **FIXED 2026-10-27 (uncommitted, `src/transplant.rs` + `src/main.rs`); was a BLOCKER. See the
  "Fix" paragraph at the end of this bullet.** Found with a Z 9 A snapshot -> Z6III `transplant --dry-run` (read-only; nothing
  was written to either body): `transplant`'s enum-by-label path matches single characters, not
  labels, and would write the wrong value for 8 of the 17 label-matched properties.**
  - Cause: packed-string enums (`elem_type` 7) come back from the SDK as one NUL-delimited byte
    buffer with `elem_bytes=1` and `elem_count` = total bytes (e.g. 178), but
    `sdk::decode_enum_values` (the `(7, bytes)` branch) assumes `elem_bytes` is a per-string stride
    and emits **one array element per byte** (`["J","P","E","G"," ","B","a",...,"" ,...]`, `""` being
    each NUL). `value_index` is an index into the *strings* (Z9 and Z6III `MovieSensitivity` both
    read `'100'` at indices 2 and 0), so `values[value_index]` is an arbitrary single character, and
    `translate_enum` then does `tgt_values.iter().position(|v| v == src_label)`, i.e. "first
    occurrence of that character in the target's byte list". Unit tests missed it because they
    use synthetic whole-string arrays.
  - Measured on this pair: correct string-level outcome is 14 no-change, 2 change
    (`BracketingVary` 'AE & Flash' -> 'AE Only'; `MovieISOAutoHiLimit` '51200' -> '25600'),
    1 skip (`Sensitivity` '64' does not exist on the Z6III). The current code writes a wrong index
    for 8: `CompressionLevel` idx 48 (only 13 real strings, so **out of range**), `ISOAutoHiLimit`
    idx 12 ('3200', should stay '25600'), `MovieSensitivity` idx 3 ('200', should stay '100'),
    `FlashISOAutoHighLimit`, `ISOAutoShutterTime`, `MovieWBMode`, `MovieISOAutoHiLimit`,
    `Sensitivity` (writes where it should skip). The dry run printed all of them as
    "would write ... via enum-by-label" with no hint.
  - Not affected: `restore`/same-model round trips (they copy `value_index` unchanged, which is
    why the earlier restore test passed), `diff`, `check`. Snapshots still contain the full byte
    list, so the strings are recoverable (modulo `from_utf8_lossy` on non-ASCII bytes).
  - **Fix (option b, no snapshot format change):** `translate_enum` now rebuilds the option strings
    from the stored byte list (`packed_string_options`: `elem_bytes==1` -> join runs between `""`
    elements, a final unterminated run counts; `elem_bytes>1` -> elements are already strings;
    anything else is rejected rather than guessed) and matches whole strings. Two real-data cases
    forced extra rules: **duplicate labels** (`ISOControlSensitivity` lists most ISO values twice) are
    skipped as a new `SkipReason::AmbiguousLabel` if the label repeats on either side, instead of
    silently taking the first; and the SDK's **`"Resource String not found"` placeholder** (every
    `EVInterval` option on the Z6III) is never a label: as a source it is `EnumUndecodedBySdk`, as a
    target it simply never matches. Dry runs now print the resolved option, e.g.
    `via enum-by-label: "25600" (#21), unchanged` / `"AE Only" (#1), was "AE & Flash" (#0)`, and the
    summary counts `skipped(ambiguous-label)`. `TransplantOutcome` gained `choice:
    Option<ResolvedChoice>` (typed, for the MCP). Tests: a fixture of the real Z 9 and Z6III arrays
    (`tests/fixtures/packed_string_enums_z9_z6iii.json`, 19 properties) with hard-coded expected
    indices for all 17 label properties, plus boundary/ambiguity/malformed cases; 8 mutants of the
    new logic were tried and the one real gap (source-only duplicates) was closed (the other, in
    `resolve_choice`, is equivalent). **Re-run on the same hardware pair: 16 label enums resolve to
    the right option (14 unchanged, 2 real changes: `BracketingVary`, `MovieISOAutoHiLimit`),
    `Sensitivity` is now a skip, and the previously wrong writes are gone.**
  - **Still open before a real transplant or any MCP write tool:** the state-property denylist and
    raw-value opt-in described in the next bullet. Original options, kept for the record: (a) fix the decode to split on NUL and emit one string per element
    (changes the snapshot format for these enums; old snapshots need a normalising load or old/new
    diffs go noisy); (b) keep the stored format and rebuild strings from the byte list inside
    `translate_enum` (no format change, smaller). Either way add a real-capture fixture test (the
    Z 9 / Z6III arrays above), assert the target index equals the string's position, and make the
    plan/dry-run print the resolved string labels and target index so a human can see them.
- **Denylist and raw-code opt-in IMPLEMENTED 2026-10-27 (uncommitted, `src/transplant.rs`,
  `src/main.rs`); hardware re-run pending (no camera was on the bus when this was finished).**
  - `NEVER_TRANSPLANT` (one table of code, name, reason; 9 entries) is checked first in `precheck`,
    so these never reach a live read or a write and cannot be overridden: `LockCamera`,
    `LiveViewStatus`, `CurrentItemID` (the last two already failed -125 on a real restore),
    `ExternalFlashStatus`, `RawJpegTransferStatus`, `ChangeMonitorOffStatus`,
    `LiveViewPhotoShootingMode`, `ShootingMode`, `Viewmode`. The last four are **judgement calls by
    name**, not evidence from the SDK config (which does not classify state vs setting), so review
    them; a real-config test checks every (code, name) pair against the Z-series sections.
  - Raw-code enum matches are skipped as `SkipReason::RawCodeUnconfirmed` **when they would change
    the target**, unless named with `--allow-raw-code <property>` (repeatable, short or full name,
    validated against the snapshot before the camera is opened: unknown or non-raw-code names are
    errors, so a typo cannot masquerade as a safe run). A raw-code match that resolves to the
    option the target already has passes without opt-in (it is a no-op). Dry runs list each
    gated property with what it would set. `TransplantOptions`/`plan_property` keep this
    device-free and unit-tested (12 mutants of the new logic killed; a CLI test covers the flag
    wiring without hardware).
  - **MEASURED 2026-10-27 on the real Z6III after commit `dff83cd`: the prediction below held
    exactly** (`written=90 literal=17 enum-by-label=16 enum-by-raw-value=36 range-clamped=21`,
    `never-copied=9`, `raw-code-unconfirmed=7`, the same seven names, errors 0), and the
    `--allow-raw-code` dry run moved a named property from the gated list into the write list
    while leaving the others gated; the Z6III's settings were unchanged by the dry runs (before/after
    snapshot diff empty). Original prediction, for the record: written 90,
    never-copied 9, raw-code-unconfirmed 7 (`AEBracketingStep`, `AEBracketingType`,
    `AFSubjectDetection`, `ImageColorSpace`, `MeteringMode`, `MovieFileType`, `MovieScreenSize`);
    36 raw-code enums already match and pass untouched. **Compare against the real output once a
    Z6III is attached.**
  - **Still open:** numeric *literal* scalars are copied as-is (`WBAutoType` 3 -> 2,
    `MovieWBAutoType`, `MovieRecMicrophone` 0 -> 5, `ElectronicVR`, `MovieWindNoiseReduction`),
    but an integer is no more self-describing than a raw code, so they carry the same unverified
    meaning risk and arguably want the same gate (needs a live read of the target to tell a no-op
    from a change). Also unverified: whether `ShootingMode`-style exclusions are the right set,
    and a real transplant has still never been applied to hardware.
- **FIRST CROSS-MODEL WRITE, 2026-10-27 (Z 9 A source -> Z6III fw 2.00), done and undone.** Source
  was the Z 9 baseline cut down to one property (`BracketingVary`); restore point
  `snapshots/Z6_3_..._pre-transplant-2026-10-27_*.json` taken first (`ok=200 err=12`, the model
  baseline). Dry run: `"AE Only" (#1), was "AE & Flash" (#0)`. Real run: `written=1 errors=0`.
  A fresh snapshot differed from the restore point in exactly that one property
  (`value_index` 0 -> 1). Undo by **full `restore` of the restore point**: `written=115`, 2 errors
  (`CurrentItemID`, `LiveViewStatus`, -125, the same two as before), exit 0, and the next snapshot
  was **identical to the restore point ("no differences")**. So the plan -> apply -> verify ->
  restore-point-undo loop works end to end for an enum-by-label change on real hardware.
  Observations that change the plan:
  - **The process hung in teardown AFTER a successful write** (`Transplant complete: written=1`
    printed, then spun until the 90 s watchdog killed it; exit 142). First time on the Z6III
    without a concurrent process, and the first after a write. The write had landed, so **a
    non-zero exit/timeout must never be read as "the write failed"**: the MCP apply result has to
    come from structured output and a read-back/diff, not the process exit status. The camera was
    healthy afterwards. This also strengthens the killable-child-process design (§3).
  - **`ShootNoCard` flipped false -> true across a power cycle** (not caused by anything run
    here; the snapshot after the earlier dry runs was clean). It may be a card-slot-derived or
    otherwise state-like property; it was in the transplant would-write list as a literal. Worth
    a look for the denylist.
  - **`restore` had the same state-property problem as transplant** (it attempted `CurrentItemID`
    and `LiveViewStatus` and reported 2 errors on every full restore). **FIXED 2026-10-27
    (uncommitted, `src/main.rs`):** `restore` now reads the same `NEVER_TRANSPLANT` table through a
    device-free `restore_precheck` (policy skip first, then absent, then read-only) and reports
    `skipped(never-copied)`. Measured on the Z6III with the restore point: dry run `written=110
    never-copied=9 errors=0`; real restore `written=108 read-only=81 unsupported-type=2
    never-copied=9 errors=0` (was `written=115 ... errors=2`), and the camera afterwards was
    identical to the restore point. 5 mutants of the precheck were killed; the loop's `continue` is
    only covered by that hardware run (it needs a Device). Consequence to note: the four
    judgement-call entries (`ChangeMonitorOffStatus`, `LiveViewPhotoShootingMode`, `ShootingMode`,
    `Viewmode`) and `LockCamera` are now also not restored, which is the intended behaviour for an
    undo but is a change for a backup/firmware-rollback restore, so review that list with both
    uses in mind. `errors == 0` is now a meaningful success signal for a restore point.
- **Other transplant risks seen in the same dry run (original findings, now addressed above except as noted):**
  107 properties would be written; 90 already match the Z6III. Among them are **runtime/state
  capabilities that should never be copied**: `LockCamera`, `LiveViewStatus`, `CurrentItemID`,
  `ExternalFlashStatus`, `RawJpegTransferStatus`, `ChangeMonitorOffStatus`, `ShootingMode`,
  `Viewmode`, `LiveViewPhotoShootingMode`, `CCDDataMode` (the first two already errored -125 on a
  Z6III restore). 46 are matched by **raw numeric code**, which is only meaningful if both bodies
  number the same options identically (`PictureControl`, `MeteringMode`, `ShootingMode`,
  `AFSubjectDetection`, `MovieFileType`, `ImageColorSpace`, ...); e.g. Z6III `ShootingMode` 0 ->
  Z9 2 and `MeteringMode` 1 -> 0 would change the camera on an unverified assumption. Add a
  never-copy denylist for state props and require explicit user opt-in per raw-value property.
  Skips were: 3 `no_matching_option` (`EVInterval`, `LockExposureEx`, `SensitivityInterval`),
  1 unreadable on target (`FlashSyncTime`, -106), 2 `unsupported value shape`
  (`WBPresetName`, `MovieWBPresetName`), 76 read-only, 7 absent.
- **Two bodies attached together (Z6III 3014527 + Z 9 A 3023668), read-only, 2026-10-27.**
  - **Enumeration and selection behave correctly on real hardware.** `discover` lists both
    (Z6_3 id 1, Z 9 id 2, correct serials/firmware; the same-model serial-pairing ambiguity
    does not arise with different models). `transplant --dry-run` with no `--serial` **refuses**
    ("2 Nikon cameras are available; pass --serial ...", `transplant_target_ambiguous` on real
    devices, not only in the unit test); with `--serial` it targets the right body. `restore`
    picks the target from the snapshot's serial (Z 9 snapshot -> Z 9, restore point -> Z6III).
    **`snapshot` with no `--serial` silently picked the first available body (the Z6III)**, as
    documented; the MCP's `capture_snapshot` must refuse instead (already in §4), since an agent
    would not notice which body it got.
  - **`discover --json` still has no `available`/`connected_pid`**; whether `available` flips
    while another process holds a body is not observable from the CLI output and remains
    unverified (Phase 0 work).
  - **Concurrent captures of different bodies: 8 simultaneous pairs (16 processes), no hangs,
    every capture complete (Z6III `ok=200 err=12`, Z 9 `ok=196 err=11`), camera enumeration
    intact after each pair.** The Z 9 process started +0 (x2), 0.3, 0.6, 1.0, 1.5, 2.0 and
    2.5 s after the Z6III one (my first loop script had an off-by-one in the offset array under
    zsh; those are the offsets that actually ran). Pairs took 4-7 s against 4-5 s solo. **Do not
    over-read this:** the Z6III-holding processes were exposed to a second SDK open exactly as
    in the earlier hang test, and with the earlier ~17% rate a clean run of 8 such processes has
    roughly a 1-in-4 chance by luck, so it does not show that different-body concurrency is
    safe. It is compatible with "the hazard is a second process touching the *same* body", and
    it shows that two bodies can be read at once without partial captures. A per-camera lock
    is therefore plausible but unproven; a global lock plus the watchdog remains the
    conservative design.
- Dependency on the firmware session's `firmware latest` library shape for `firmware_status`
  (the `--json` contract exists in `main.rs`; confirm it is committed and lifted into the library).

## 9. Out of scope

Any dependence of the CLI/GUI on this crate; remote/HTTP transport; Windows camera access;
automating the firmware licence; FTP profile writes; firmware flashing.
