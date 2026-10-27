# Plan: optional MCP server for nikon-fleet

**Status: plan only. The MCP server itself is not built.** Written and revised 2026-10-27.

- Hardware evidence behind every "measured" claim below is in
  [mcp-hardware-findings.md](mcp-hardware-findings.md) (findings F1-F13, cited inline).
- Supporting fixes found while checking this plan are already on `master`: `aaa20da`,
  `1043c31`, `dff83cd`, `08db008` (see the findings file). They are CLI/library changes the MCP
  will sit on; none of them is MCP code.
- Review history: a cold review against the code was done 2026-10-27 and folded in; this
  version is the consolidation of that review plus the hardware work. **Per the spec-review rule
  it needs a fresh-session review before implementation starts.** Suggested focus, in order: §8.2
  (what is still unverified, starting with the MCP crate), §9 (decisions left open), then §3/§5.

## 1. Why, and what it must not become

Motivating requests (user, 2026-10-27):

- "What are the differences between these two Z 9s?"
- "Make my Z6III as close to this Z 9 as you can, and tell me what did not get copied."

Both are already core operations (`diff`, `transplant`). The MCP is a natural-language front end
over them, in the same way the GUI is a graphical one.

The first request needs two snapshots, so it is not answerable until `capture_snapshot` (Phase 2)
unless snapshots already exist on disk. (A Z 9 A baseline now exists in the repo's `snapshots/`;
a Z 9 B does not.) The second request cannot carry **Picture Controls** at all: they are
unreadable through this SDK path on both bodies (F5), and the user-facing answer must say so.

**Constraint:** nikon-fleet must run with no AI assistant (see memory `project-no-ai-runtime`). So
the MCP is a separate, optional binary. No core path, GUI feature, install step or documentation
flow may depend on it. Deleting the crate must leave the CLI and GUI unchanged.

## 2. The orchestration is in `main.rs`, not the library

`lib.rs` exports primitives (`sdk`, `snapshot`, `diff`, `transplant`, `firmware*`). The
*workflows* are in `src/main.rs`: `cmd_snapshot`, `cmd_check`, `cmd_restore`, `cmd_transplant`
(device selection, the ambiguous-target refusal `transplant_target_ambiguous`, outcome counting,
printing, `ensure_capabilities_present`, `restore_precheck`), and `firmware latest --json`'s types
(`LatestModel`, `LatestReport`, `survey_json`). So "a thin wrapper over the library" is not yet
true. If the MCP re-implemented these it would be a second copy of each rule.

**Phase 0 therefore extracts the workflows into a library module (`ops`)** that return structured,
serialisable results. The CLI becomes a printer over them and the MCP a JSON adapter over them.
This also closes the `--json` gaps (only `discover`, `diff` and `firmware latest` have it today).

What is already in the library and usable as-is: `transplant::plan_property` (translate + resolve +
gate, device-free), `ResolvedChoice`, `TransplantOptions`, `resolve_allow_raw_code`,
`NEVER_TRANSPLANT` / `never_copied`, `packed_string_options`. What is still in `main.rs` and must
move: everything in the first paragraph above.

Rules that exist in more than one place today, which Phase 0 should collapse rather than copy a
third time: "a snapshot is the reference iff it is byte-identical to the reference file" lives in
both the Python `gui/fleet_lib.py` and the Rust GUI's `list_snapshots` (`gui/src/main.rs`).
`ops::list` becomes the one implementation and the GUI migrates onto it, or the plan records why
not (§9).

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
- **The SDK runs in a killable child process with a watchdog.** The SDK is documented as not
  thread-safe and single-threaded (`sdk.rs`), and the CF run-loop pump is per-thread (the IOKit
  notification is registered on whichever thread calls `InitializeSDK`), so all SDK use goes
  through one dedicated thread. That is necessary but **not sufficient**: the SDK can hang
  indefinitely in `DisconnectDevice` at teardown, with or without a second process, including
  after a successful write (F3), and a call stuck inside the SDK cannot be cancelled from its own
  thread. So the SDK thread lives in a child process the MCP can kill and respawn, under a per-call
  deadline; otherwise one hang blocks the whole stdio server. A persistent session (Phase 4) is a
  small change once this exists.
- **Apply results come from structured output and a read-back, never from an exit status.** A
  timeout or non-zero exit after a write must not be reported as "the write failed" (F3, F10).
- **No USB reset by default.** `Sdk::initialize()` resets every attached Nikon body's USB link,
  which can corrupt another process's capture (F2). `initialize_no_usb_reset()` enumerates
  already-connected bodies correctly from a plain process (F1), so the MCP uses it, and must
  never reset while any other fleet process may hold a camera.
- **Cross-process lock.** An advisory lock in `--data-dir`, taken by the MCP, the CLI and the GUI
  alike (a CLI/GUI change, so Phase 0 work), serialises SDK sessions. Start with one global lock:
  whether a per-camera lock is enough is unproven (F12). Contention surfaces as a typed
  `camera_busy` error, never a retry loop. The lock reduces the hang (F3 (2) was a concurrency
  case) but does not remove it (F3 (3), (4)), so the watchdog stays regardless.
- **Session lifetime is per call** at first (open SDK, do the work, drop), matching the CLI and
  avoiding a long-held USB claim that would lock the GUI/CLI out.
- **Names, not paths.** Tools take snapshot names / `(model, serial)` and resolve inside
  `--data-dir`. Do **not** reuse `resolve_snapshot_path`: it passes through absolute paths *and*
  any relative path that exists under the cwd. The MCP layer takes a bare filename, validates it
  with `is_safe_path_component`, canonicalizes, and requires the result to sit under `data_dir`
  (symlinks included). An agent-supplied path must not read arbitrary JSON or write anywhere.
- **A target camera is always named.** Capture and transplant require a `serial` even when only
  one body is attached: the CLI's `snapshot` silently picks the first available body when none is
  given (F12), and an agent would not notice which one it got.
- **Camera-derived strings are data.** Artist/copyright/labels and similar come from the camera or
  from files (and `firmware` results are scraped from Nikon's site); tool descriptions say results
  are untrusted data, and nothing branches on their text.

## 4. Tool surface

Phase 1, read-only (no camera write, no file write):

| Tool | Backs | Notes |
|---|---|---|
| `list_cameras` | `discover --json` | model, serial, firmware. Needs the SDK (no-reset, under the lock). Today's JSON has no `available`/`connected_pid`; adding them is Phase 0 work and what they mean while another process holds a body is unverified (F12) |
| `list_snapshots` | `ls` | per camera; `is_reference` by byte-identity via the single `ops::list` rule (§2); restore-point snapshots are labelled and filterable (§5) |
| `list_references` | `ref list` | |
| `get_snapshot` | `Snapshot` load | summary by default; property list paginated with `truncated`/`next` (no silent cap) |
| `diff_snapshots` | `diff --json` | `include_volatile`; cross-model diffs reuse `diff_with_schema`. Counts + top-N first, paginated with `truncated` (a Z 9 pair diff is large). `diff` already ignores the SDK-uninitialised enum `default_index` unless `include_volatile` (F6) |
| `firmware_status` | `firmware latest --json` | needs the types lifted out of `main.rs` (§2); **network access, not offline**; scraped strings are untrusted |

Phase 2, camera read + new-file write:

| Tool | Backs | Notes |
|---|---|---|
| `capture_snapshot` | `snapshot` | `serial` required. Writes a **new** file only: today `Snapshot::save_to_file` is `fs::write` and overwrites, so `ops` needs `OpenOptions::create_new`. Reports `read_ok`, `read_err`, the failed codes and the no-get-bit count, and fails when the capture is worse than the model baseline (§5), not only when `read_ok == 0` |
| `check_against_reference` | `check` | live read, diff vs the camera's reference; same capture gate |

Phase 3, writes (only with `--enable-writes`):

| Tool | Notes |
|---|---|
| `transplant_plan` | = `transplant --dry-run`. Inputs: source snapshot name, target `serial`, optional `allow_raw_code` (list of property names, validated against the snapshot before the camera is touched; same rule as the CLI flag). Returns the outcome report (§5) and a `plan_digest` |
| `transplant_apply` | Requires the plan's digest (and token if §9 decides on one). Takes a restore point, then writes (§5) |
| `restore_plan` / `restore_apply` | Same two-step shape, used to undo from the restore point. Skips the same never-copy properties |

**Deliberately not exposed:** `vendor-write-ftp` (the old profile can't be read back; see memory
`project-ftp-profile-readback`), anything that flashes firmware (does not exist yet), `firmware
fetch/browse` (the licence click is human by decision), `rm`, `vendor-read` (revisit after FTP
read-back work), `ref set` (changes what "reference" means, and takes an arbitrary path; could come
later behind writes with the same name-only rule).

## 5. Transplant and restore: report, plan, apply, restore point

### 5.1 Outcomes

Each property yields `TransplantOutcome { name, code, result: Result<Strategy, SkipReason>,
choice: Option<ResolvedChoice> }`. For the MCP both enums serialise as **closed string tags**, with
a separate typed detail field where one exists (e.g. `write_failed` + `detail`), never the Display
text. The tag mapping is an **exhaustive `match` with no wildcard arm**, so a new variant fails to
compile until mapped.

| `Strategy` tag | Meaning |
|---|---|
| `literal` | scalar copied as-is |
| `enum_by_label` | packed-string enum matched on the whole label |
| `enum_by_raw_value` | enum matched by raw numeric code (weaker; see `raw_code_unconfirmed`) |
| `range_clamped` | range value clamped/snapped into the target's bounds |

| `SkipReason` tag | Routine? | Meaning |
|---|---|---|
| `absent_on_target` | routine | capability not on the target |
| `read_only` | routine | target does not allow setting it |
| `unsupported_write_type` | routine | SDK rejected the capability kind |
| `no_matching_option` | **warn** | the source's choice does not exist on the target |
| `unsupported_value_shape` | **warn** | not a scalar/enum/range this code can translate |
| `target_read_failed` | **warn** | the live read of the target failed (detail: SDK error) |
| `malformed_source` | **warn** | corrupt or hand-edited snapshot value (includes pre-fix snapshots whose packed-string `values` are `[]`) |
| `write_failed` | **error** | the write itself failed (detail: SDK error) |
| `enum_undecoded_by_sdk` | **warn** | SDK could not decode the options, or the selected label is the `"Resource String not found"` placeholder |
| `enum_type_mismatch` | **warn** | label enum on one side, raw-code on the other |
| `capability_name_mismatch` | **warn** | the code has unrelated names on the two models |
| `ambiguous_label` | **warn** | the chosen label repeats in either body's option list (e.g. `ISOControlSensitivity` lists most ISO values twice) |
| `never_copied` | routine (policy) | on the never-copy table (§5.2); no override |
| `raw_code_unconfirmed` | policy | matched only by raw numeric code, would change the target, and was not opted in (§5.3) |

Report shape: counts per tag first, then the per-property list grouped by tag with the warn-worthy
tags first; each written or would-write enum carries its `ResolvedChoice` (option label or `code
N`, target index, what the target has now, unchanged or not). A paginated list sets `truncated`.
The report must also state what the SDK cannot read at all (**Picture Controls**, F5), because that
is invisible in a per-property list.

### 5.2 Never-copy table

`NEVER_TRANSPLANT` (code, name, reason) is the single source of truth, read by `precheck` in
transplant **and** `restore_precheck` in restore through `never_copied`. It is checked before any
live read or write and has no override. It holds properties that report what the body or session
is doing, or lock it: `LockCamera`, `LiveViewStatus`, `CurrentItemID` (these two already failed
-125 on a real restore), `ExternalFlashStatus`, `RawJpegTransferStatus`, `ChangeMonitorOffStatus`,
`LiveViewPhotoShootingMode`, `ShootingMode`, `Viewmode`. Only the two -125 entries have hardware
evidence; the other seven rest on their names (the SDK config does not classify state vs setting),
and the code marks four of those (`ChangeMonitorOffStatus`, `LiveViewPhotoShootingMode`,
`ShootingMode`, `Viewmode`) as explicit judgement calls. A real-config test checks every
(code, name) pair against the Z-series sections. Consequence for `restore`: these are not restored
either, which is right for an undo but a change for a backup/firmware-rollback restore (§9).

### 5.3 Raw-code opt-in

A raw-code enum match assumes both bodies number the same option the same way, which nothing
guarantees. It is skipped as `raw_code_unconfirmed` **when it would change the target**, unless the
property is named in `allow_raw_code` (CLI: `--allow-raw-code <property>`, repeatable). A match that
resolves to the option the target already has is a no-op and passes without opt-in. Names are
validated against the source snapshot before the camera is opened; an unknown name, or one that is
not a raw-code enum, is an error, so a typo cannot masquerade as a safe run.

### 5.4 What a plan promises

A plan is **not a prediction.** `restore --dry-run` once said `written=119 errors=0` and the real
run wrote 115 with 2 errors and exit 0 (F4); the live reads in `transplant --dry-run` find
`no_matching_option` but cannot find a failing write. Plan and apply tool descriptions say "would
attempt", and the apply result reports actual written/skipped/error counts and codes.

`plan_digest` is a **staleness guard**: it proves the camera or snapshot has not changed since the
plan. It is **not** a consent mechanism: an agent can plan then apply back-to-back without a human
seeing the plan. The real consent gates are `--enable-writes` and the client's approval prompt. If
an unreviewed apply must also be prevented server-side, the plan result carries a short-lived
single-use token that `transplant_apply` requires and the tool description tells the agent to show
the plan first (decision in §9).

The digest input must be **stable**: hash(source snapshot bytes, target serial, per-property
`(name, code, tag, structured fields)` in `BTreeMap` order). Never hash SDK error text
(`TargetReadFailed`/`WriteFailed` carry it) or the volatile/garbage fields (§4, F6), or a transient
difference produces a false `stale_plan`.

### 5.5 Apply sequence (all in one call on the SDK child)

1. Recompute the dry run on the live target (with the same `allow_raw_code`); compare
   `plan_digest`. Mismatch ⇒ refuse with `stale_plan`.
2. Capture the target's current state as a snapshot labelled `pre-transplant-<timestamp>` (the
   **restore point**), written create-new. **Abort before any camera write** if the file write
   failed or the capture is *worse than that body's baseline*. "Zero read errors" is not a usable
   gate: a healthy Z6III always reports `ok=200 err=12`, a Z 9 `ok=196 err=11` (F5). The gate is:
   every failed code is in the model's known-unreadable set (or the same set as the camera's
   reference), and `read_ok` has not dropped below that baseline. A mid-capture USB reset produced
   captures like `ok=18 err=194` that exit 0 today (F2), which is exactly what this must catch.
   `ops::capture` therefore returns `read_ok`, `read_err` and the failed codes; where per-model
   baselines are stored and how they are created is open (§9).
3. Run the real transplant; return the outcomes plus the restore point's name. Verify by read-back
   (a fresh capture diffed against the restore point) and report the diff, since the process exit
   status cannot be trusted (§3).
4. `restore_plan`/`restore_apply` on the restore point's name is the undo.

### 5.6 Restore and the restore point: honest limits

The restore point holds only what the SDK let us *read*: write-only or unreadable properties
(including Picture Controls) cannot be restored. **Restoring writes every snapshotted property
that is not on the never-copy table, not only the ones transplant touched, so any change made to
the camera after the restore point was taken is overwritten.** Fidelity is verified for one
property only (F4, F10): a one-property cross-model change was applied, read back, and undone by a
full restore that left the camera identical to the restore point. Restore-point files land in
`snapshots/`, so they carry a distinct label prefix; check that nothing treats "latest snapshot" as
meaningful, or filter them from the default lists.

## 6. Phases

**Phase 0 — extract `ops`** (library). Status of each item:

| Item | Status |
|---|---|
| Reject empty captures: `snapshot_capture_failed` is `read_ok == 0`; `ensure_capabilities_present` after every `connect` | **done** (`aaa20da`), in `main.rs` |
| Whole-label packed-string matching; `AmbiguousLabel`; placeholder labels; `ResolvedChoice` on outcomes | **done** (`1043c31`), in the library |
| `NEVER_TRANSPLANT`, raw-code gate, `--allow-raw-code`, device-free `plan_property` | **done** (`dff83cd`), in the library |
| `restore` uses the never-copy table (`restore_precheck`) | **done** (`08db008`), in `main.rs` |
| `diff` ignores the SDK-uninitialised enum `default_index` unless `include_volatile` | **done** (`aaa20da`) |
| Move `cmd_snapshot`/`check`/`restore`/`transplant`, `restore_precheck`, `ensure_capabilities_present`, device selection and the ambiguity refusal into `ops`, returning structured results | todo |
| `Serialize` on `Strategy`/`SkipReason` as closed tags (exhaustive match) and on outcomes | todo |
| `--json` on `ls`, `ref list`, `check`, `snapshot`, `transplant`, `restore`; `discover --json` gains `available`/`connected_pid` | todo |
| `ops::capture` returns `read_ok`/`read_err`/failed codes, uses create-new writes | todo |
| Per-model capture baselines and the "worse than baseline" gate (also useful to the CLI: a partial capture still exits 0) | todo |
| One `ops::list` byte-identity rule; GUI (Rust and Python) migrate onto it | todo |
| Cross-process advisory lock in `--data-dir`, taken by CLI, GUI and MCP | todo |
| A small camera-port trait (`capabilities`/`read_capability`/`write_capability`) so plan/apply and the restore-point logic are testable without a `Device` (`plan_property` covers translate+gate only) | todo |
| Lift `firmware latest --json`'s types out of `main.rs` | todo |

Phase 0 touches `src/main.rs`, `lib.rs` and `gui/`; the firmware session's earlier edits there are
committed, so nothing blocks it.

1. `mcp/` scaffold + SDK child process with watchdog + the Phase 1 tools, read-only registration only.
2. `capture_snapshot`, `check_against_reference`.
3. Write tools behind `--enable-writes`.
4. Optional: persistent SDK session (cheap once the child exists); `vendor_read`.

## 7. Tests (threshold / ambiguity list, written before code)

Already in the repo (do not re-derive): packed-string enum fixtures from the real Z 9 and Z6III
arrays with hard-coded expected indices; boundary/ambiguity/malformed cases for label matching;
never-copy table integrity and its real-config name check; the raw-code gate matrix; `--allow-raw-code`
validation, including a no-hardware CLI test; `restore_precheck` ordering; `diff` ignoring the enum
`default_index`; the empty-capture predicate.

To write for the MCP:

- Read-only mode registers none of the write tools; `--enable-writes` registers them.
- Path outside `data_dir` (absolute, `..`, symlink, **relative path that exists under the cwd**)
  rejected for every path-taking tool.
- Two cameras attached and no `serial` ⇒ refused for capture and transplant; a `serial` is
  required even with one camera attached.
- `transplant_apply` with: matching digest (proceeds), digest differing by one outcome (refuses),
  missing digest/token (refuses), restore-point capture failure (no write happens), restore-point
  file write failure (no write happens), **restore-point capture with partial read errors, e.g.
  `ok=18 err=194` (no write happens)**, **restore-point filename already exists (no overwrite, no
  write happens)**, `allow_raw_code` differing between plan and apply (refuses as stale).
- A process that finishes its write and then hangs/times out: the result is read from the
  read-back diff and reports success, not failure; a process that hangs *before* the write: no
  write is reported.
- Digest stability: two dry runs with identical state but different `WriteFailed`/`TargetReadFailed`
  text, or different volatile/garbage fields, produce the same digest.
- Empty snapshot properties; all-skipped plan (apply is a no-op and says so); source model equals
  target model (decide whether the CLI's "use `restore`" note is returned); non-Z-series model
  rejected (existing guard).
- Pagination: exactly-N and N+1 properties around the page size; `truncated` correct at both, for
  `get_snapshot`, `diff_snapshots` and the plan list.
- Outcome tags: every `SkipReason` and `Strategy` variant maps to a distinct tag (exhaustive match;
  a new variant must fail to compile until mapped).
- Parity: CLI `--json` and MCP tool results come from the same `ops` value; `is_reference` agrees
  across `ops::list`, the Rust GUI and `fleet_lib.py` for the same fixtures.
- Live (needs the user and hardware): Z 9 ↔ Z6III plan/apply/undo on real bodies beyond the single
  enum-by-label property already done (F10): a range, a literal, an opted-in raw-code enum.

## 8. External-system assumptions

### 8.1 Verified on hardware (details in the findings file)

| Assumption | Result | Ref |
|---|---|---|
| `initialize_no_usb_reset()` enumerates already-connected bodies from a plain process | yes | F1 |
| A reset-initialised open during another process's capture is harmless | **no**: empty or partial snapshots, exit 0 | F2 |
| A second process opening the SDK fails cleanly | **no**: the holder can hang at teardown | F3 |
| The teardown hang needs a second process | **no**: also solo, also after a write | F3 |
| Undo by full restore returns the camera to the restore point | yes, for one enum property | F4, F10 |
| A dry run predicts the real result | **no**, for failing writes | F4 |
| A healthy capture has zero read errors | **no**: baselines are 200/12 (Z6III) and 196/11 (Z 9) | F5 |
| Picture Controls can be read and so transplanted | **no**, on both bodies | F5 |
| Diffs of an untouched camera are empty | only after ignoring the enum `default_index` (Z6III) and the volatile list | F6 |
| Enum labels in snapshots are whole strings | **no**: one element per byte (fixed in matching) | F8 |
| Two attached bodies enumerate, select and refuse ambiguity correctly | yes | F12 |
| `snapshot` without `--serial` is safe with two bodies attached | **no**: it picks the first | F12 |

### 8.2 Still unverified (review these)

- **MCP Rust SDK: assumed `rmcp`** (the official `modelcontextprotocol/rust-sdk`), tokio-based,
  stdio server, tool annotations. **Not checked**: confirm crate name, maturity and API first.
  This is the main external-system assumption left.
- Tool annotations (`readOnlyHint`, `destructiveHint`): assumed advisory only. The server-side
  `--enable-writes` gate does not rely on the client honouring them.
- The SDK worker behaves as assumed inside a **child process / dedicated thread**: `Sdk`/`Device`
  are not known to be `Send`, the CLI has only ever run them on its main thread, and the
  CFBundle/IOKit notification that `initialize()`'s USB reset exists to trigger, plus the
  `CFRunLoopRunInMode` pump between `EnumDevices` retries, are per-thread.
- What `available` / `connected_pid` mean while another process holds a body, and whether they
  can be read without opening the SDK.
- The teardown hang's rate and trigger; in particular whether it follows "first open after the body
  was attached or woken" (replug or power-cycle, then capture several times).
- Whether a **per-camera** lock is enough (F12 is inconclusive).
- A reset-initialised open while another process is mid-*write* (not attempted).
- Restore and transplant fidelity beyond one enum property; restore after a power cycle; range,
  string and boolean property round trips.
- The Z 9 B, and same-model pairs (`warn_if_ambiguous_pairing` says serial pairing there is by
  enumeration order).
- Whether the judgement-call entries in the never-copy table are the right set, and whether
  `ShootNoCard` (flipped across a power cycle) belongs in it.

### 8.3 Known defects and gaps accepted for now

- The CLI `snapshot` still accepts a *partial* capture (e.g. `ok=18 err=194`) with exit 0 (F2).
- Numeric **literal** scalars (`WBAutoType`, `MovieRecMicrophone`, `ElectronicVR`, ...) are copied
  as-is, with the same unverified-meaning risk as raw codes and no gate (F9).
- `restore --dry-run` reports counts that real runs can miss (F4).

## 9. Decisions left open for the reviewer

1. **Digest vs token (§5.4):** accept that the digest is only a staleness guard and rely on
   `--enable-writes` plus client approval, or add a server-side single-use plan token.
2. **Lock granularity (§3):** one global lock, or per-camera. Evidence is inconclusive (F12).
3. **Never-copy table membership (§5.2):** which of the judgement calls stay, whether `ShootNoCard`
   joins, and whether `restore` for backup/firmware-rollback should use a smaller table than the
   undo case.
4. **Numeric literals (§8.3):** gate them like raw codes (needs a live read to tell a no-op from a
   change), or accept the risk.
5. **Per-model capture baselines (§5.5):** where they are stored, how they are created and
   refreshed (per model, per firmware?), and what a new model with no baseline does.
6. **GUI migration onto `ops::list` (§2):** do it in Phase 0 or leave the GUI's copies and test
   parity (§7).
7. **Source model equals target model:** return the CLI's "use `restore`" note, or just proceed.

## 10. Out of scope

Any dependence of the CLI/GUI on this crate; remote/HTTP transport; Windows camera access;
automating the firmware licence; FTP profile writes; firmware flashing.
