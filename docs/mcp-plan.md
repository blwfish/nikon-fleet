# Plan: optional MCP server for nikon-fleet

**Status: plan only. The MCP server itself is not built.** Written and revised 2026-10-27.

- Hardware evidence behind every "measured" claim below is in
  [mcp-hardware-findings.md](mcp-hardware-findings.md) (findings F1-F13, cited inline).
- Supporting fixes found while checking this plan are already on `master`: `aaa20da`,
  `1043c31`, `dff83cd`, `08db008` (see the findings file). They are CLI/library changes the MCP
  will sit on; none of them is MCP code.
- Review history: (1) a cold review against the code, folded in earlier on 2026-10-27; (2) a
  fresh-session spec review the same day (external-system assumptions checked against crates.io
  and the code), folded into the next version. The second review changed structure, not just
  wording: the SDK worker is a child process on its own main thread (§3), undo is a targeted
  restore (§5.6), capture and apply refuse when two bodies of one model are attached (§3), and
  the plan tools are read-only-mode tools (§4); (3) a cold read of those structural changes,
  checked against the code, the findings file and crates.io, folded into this version. It moved
  the lock out of `--data-dir`, added a worker result protocol with an `indeterminate` outcome and
  rules for client timeouts, cancellation and concurrent calls (§3), restricted targets to USB
  serials (§3), widened the undo set to include side effects (§5.5-5.6), specified `restore_apply`
  (§4), and put the two spikes ahead of Phase 0 (§6). **Those changes have not had a further cold
  read.** Suggested focus for any further pass, in order: §3 (worker protocol, queue), §5.5-5.6,
  §6 ordering.

## 1. Why, and what it must not become

Motivating requests (user, 2026-10-27):

- "What are the differences between these two Z 9s?"
- "Make my Z6III as close to this Z 9 as you can, and tell me what did not get copied."

Both are already core operations (`diff`, `transplant`). The MCP is a natural-language front end
over them, in the same way the GUI is a graphical one.

The first request needs two snapshots, so it is not answerable until `capture_snapshot` (Phase 2)
unless snapshots already exist on disk. (A Z 9 A baseline now exists in the repo's `snapshots/`;
a Z 9 B does not, and capturing it while both Z 9s are attached is refused by the same-model rule
in §3.) The second request cannot carry **Picture Controls** at all: they are
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

One rule exists in two places today: "a snapshot is the reference iff it is byte-identical to the
reference file" lives in both the Python `gui/fleet_lib.py` and the Rust GUI's `list_snapshots`
(`gui/src/main.rs`). The CLI's `ls` has no reference marking at all, so `ops::list` adds the
behaviour to the CLI rather than merging two copies. It becomes the canonical rule, but the GUIs
are **not** migrated in Phase 0: the Python GUI cannot call Rust `ops` (it would have to shell out
to `ls --json`). The §7 parity test pins all three to the same fixtures instead; migrating the
GUIs is a later cleanup (decided, §9).

## 3. Architecture

- New workspace member `mcp/` (crate `fleet-mcp`), like `gui/` (the Rust `fleet-gui` crate).
  Depends on `nikon-fleet` as a library. stdio transport, run by the MCP client
  (`fleet-mcp --data-dir … --sdk-bundle …`). It spawns the `nikon-fleet` binary (path via
  `--worker`, default: beside itself) as `nikon-fleet worker` for anything that needs the SDK.
- **MCP library: `rmcp`** (the official `modelcontextprotocol/rust-sdk`), pinned to an **exact**
  version, features `server`, `macros`, `transport-io`. Do not take `3.8.0`: it was published the
  day before the review that produced this note (about 4k downloads, eight 3.x minors since July);
  start from `=3.5.1` (a month old) and confirm in spike A (§6). Its minimum Rust is
  1.88 against the toolchain's 1.95, and edition 2024: compatible.
- **macOS only for anything touching cameras.** The MAID SDK is loaded as a CFBundle/Mach-O dylib
  (see the `libloading` comment in `Cargo.toml`). Snapshot/check/restore/transplant cannot run on
  Windows; offline tools (diff, ls) could. `firmware_status` needs network, not a camera.
- **Three tiers, enforced server-side.** (a) *Read-only*: offline tools and live camera **reads**
  (including the plan tools, which only read). (b) *File-writing*: `capture_snapshot` writes a new
  file in `--data-dir`, never overwriting; on by default because it cannot change a camera, but
  bounded (retention, below). (c) *Camera-writing*: the `*_apply` tools, registered only with
  `--enable-writes`. Client-side approval prompts are a second layer, not the protection (clients
  differ in how they treat `destructiveHint`). Whether `rmcp` can register tools conditionally at
  startup is unverified (§8.2); if not, register all and refuse in the handler with a typed
  `writes_disabled`.
- **Retention.** There is no `rm` tool, so captures and restore points only accumulate. Files the
  MCP creates are marked by label prefix, added server-side so an agent-supplied label cannot
  dodge it: `mcp-<label>` for captures, `mcp-restore-<op>-<timestamp>` for restore points. Each
  kind has its own per-camera cap (placeholders, chosen at build time). `capture_snapshot` refuses
  with `data_dir_full` past the capture cap; `*_apply` refuses with `data_dir_full`, before any
  write, past the restore-point cap. Nothing is auto-deleted.
- **The SDK runs in a killable worker process with a watchdog.** The SDK is documented as not
  thread-safe and single-threaded (`sdk.rs`), and it can hang indefinitely in `DisconnectDevice`
  at teardown, with or without a second process, including after a successful write (F3); a call
  stuck inside the SDK cannot be cancelled from its own thread. So all SDK use happens in
  `nikon-fleet worker`, a child process **running the SDK on its own main thread**, which is the
  configuration the CLI uses. (The GUI also runs the SDK, per call, on a background thread with
  the AppKit run loop running: `gui/src/main.rs`. So off-main-thread use is not unknown, but the
  worker does not depend on it.) That removes any question of `Sdk`/`Device` being `Send` or of
  the per-thread CF run-loop pump. `worker` is a hidden, internal subcommand: the one
  MCP-motivated addition to the core binary, inert without the MCP. The MCP server spawns one
  worker per call under a per-call deadline and kills it on expiry; otherwise one hang blocks the
  whole stdio server. A persistent session (Phase 4) is a small change once this exists.
- **Worker protocol.**
  - The worker returns line-delimited JSON on a **dedicated fd, not stdout** (the SDK dylib is
    third-party code and may write to stdout); its stdout is discarded and its stderr goes to the
    MCP server's log. The MCP server's own stdout is the protocol channel, so `ops` must never
    print (tested, §7).
  - Lines are `progress` (a named phase: connected, restore point written, writing n/N,
    read-back) and exactly one `result`. The `result` line is **written and flushed before the
    `Device`/`Sdk` is dropped**, so a teardown hang (F3 (4): a hang after a completed write) cannot
    lose it. Once the parent has read the `result` it is final: the worker gets a short grace to
    exit, then is sent SIGKILL and reaped, and a hang or non-zero exit after the result is not an
    error. (F3's hangs were ended with SIGTERM or an alarm and the camera was healthy afterwards;
    SIGKILL of the worker has not been tried separately, §8.2.)
  - No `result` by the deadline, or the worker died: typed `worker_no_result`, carrying the last
    `progress` phase. For an apply that is the `indeterminate` outcome (§5.5): writes may or may
    not have happened and the count is unknown. There is no automatic retry.
  - Deadlines are per tool, set from measurements at build time. The apply deadline has to cover
    connect, three captures (about 5 s each on the bodies measured), and the writes. MCP clients
    have their own request timeouts, which differ by client and are unverified (§8.2): the
    deadline must be shorter than the client's, or the server sends progress notifications when
    the client supplies a progress token.
  - **Cancellation.** A client cancellation of a read-only or plan call kills the worker. For an
    apply it is honoured only before the worker reports the phase `writing`; after that it is
    ignored, because killing mid-write leaves a half-applied camera. If the client times out or
    gives up while the worker finishes, the agent's likely retry of the apply finds every outcome
    already unchanged, so its digest differs and it fails as `stale_plan` (tested, §7).
- **Calls queue inside the server.** An agent may issue camera tool calls in parallel (two bodies,
  or plan plus check). The MCP server serialises them with an in-process FIFO mutex before
  spawning workers, so `camera_busy` means a holder **outside** this server (CLI, GUI, another
  worker), never the server's own concurrency.
- **Apply results come from structured output and a read-back, never from an exit status.** A
  timeout or non-zero exit after a write must not be reported as "the write failed" (F3, F10).
- **No USB reset by default.** `Sdk::initialize()` resets every attached Nikon body's USB link,
  which can corrupt another process's capture (F2). `initialize_no_usb_reset()` enumerates
  already-connected bodies correctly from a plain process (F1), so the MCP uses it, and must
  never reset while any other fleet process may hold a camera. The worker never calls
  `initialize()`.
- **Cross-process lock.** An advisory lock taken by the MCP worker, the CLI and the GUI alike (a
  CLI/GUI change, so Phase 0 work), serialises SDK sessions. It lives at a **fixed per-user local
  path** (`~/Library/Application Support/nikon-fleet/camera.lock`, created if absent; an env
  override exists for tests), **not in `--data-dir`**. The lock protects the USB bus, not a data
  dir: the CLI, GUI and MCP can each be given a different `--data-dir` and a lock inside it would
  then not exclude them, and `flock` on a network share is unreliable. It is an `flock`/`fcntl`
  lock **held on an open fd**, never a lock file or PID file: the watchdog kills workers, and the
  kernel releases an fd lock when its holder dies, whereas a stale file would wedge every later
  call. One global lock: whether a per-camera lock is enough is unproven (F12), and the decision
  is to stay global (§9). Contention surfaces as a typed `camera_busy` error, never a retry loop;
  so does an SDK open failure caused by a holder outside the lock. The lock cannot exclude other
  USB clients (NX Studio, Image Capture, other PTP software). It reduces the hang (F3 (2) was a
  concurrency case) but does not remove it (F3 (3), (4)), so the watchdog stays regardless.
- **Session lifetime is per call** at first (open SDK, do the work, drop), matching the CLI and
  avoiding a long-held USB claim that would lock the GUI/CLI out.
- **Names, not paths.** Tools take snapshot names / `(model, serial)` and resolve inside
  `--data-dir`. Do **not** reuse `resolve_snapshot_path`: it passes through absolute paths *and*
  any relative path that exists under the cwd. The MCP layer takes a bare filename, validates it
  with `is_safe_path_component` (private to `main.rs` today; it moves to the library in Phase 0),
  canonicalizes (for a file to be created: its parent), and requires the result to sit under
  `data_dir` (symlinks included). An agent-supplied path must not read arbitrary JSON or write
  anywhere.
- **A target camera is always named, by USB serial.** Capture, check, plan and apply require a
  `serial` even when only one body is attached: the CLI's `snapshot` silently picks the first
  available body when none is given (F12), and an agent would not notice which one it got. The MCP
  matches `serial` against the **USB** serial only. The CLI's `serial_matches` also accepts the
  bare SDK id and the `id-N` fallback; SDK ids are assigned per session, so `id-1` can be a
  different body on the next call, and a digest or restore point keyed to one would silently
  retarget. A body with no readable USB serial, or a snapshot stamped `id-N`, cannot be a target
  for any camera-touching tool (`no_usb_serial`); `list_cameras` still shows it. Phase 0 gives
  `ops` a strict matcher; the CLI keeps `serial_matches`.
- **Refuse when two bodies of one model are attached.** For same-model bodies, `pair_devices`
  attaches serial and firmware to SDK devices by enumeration order (`main.rs`,
  `warn_if_ambiguous_pairing`); nothing in the SDK's device info can confirm it. With two Z 9s
  attached, a capture could stamp the wrong serial on a snapshot and an apply could write the
  wrong physical body. So `capture_snapshot`, `check_against_reference` and every plan/apply tool
  return a typed `ambiguous_pairing` whenever more than one attached body shares the target's
  model. The workaround is to attach one at a time; a verified pairing method is future work.
  `list_cameras` still lists them, flagged `pairing_ambiguous`.
- **Camera-derived strings are data.** Artist/copyright/labels and similar come from the camera or
  from files (and `firmware` results are scraped from Nikon's site); tool descriptions say results
  are untrusted data, and nothing branches on their text.

## 4. Tool surface

Phase 1, read-only (no camera write, no file write):

| Tool | Backs | Notes |
|---|---|---|
| `list_cameras` | `discover --json` (USB path) | model, serial, firmware from the USB descriptors (rusb, as `enrich_devices` already reads them): **no SDK session, no lock, no hang risk, no reset**. Snapshots key on the SDK's model name (`Z6_3`), which differs from the USB product string, so Phase 0 adds the mapping and a test. Flags `pairing_ambiguous` (§3). `available`/`connected_pid` already exist on `sdk::DeviceInfo` (`sdk.rs`) but are not needed here; exposing them in `discover --json` stays a CLI nicety, and their meaning while another process holds a body remains unverified (F12). A USB product string with no mapping is reported `unknown_model`, never passed through as a model name, and such a body cannot be a target |
| `list_snapshots` | `ls` | per camera; `is_reference` by byte-identity via the single `ops::list` rule (§2); restore-point snapshots are labelled and filterable (§5) |
| `list_references` | `ref list` | |
| `get_snapshot` | `Snapshot` load | summary by default; property list paginated with `truncated`/`next` (no silent cap) |
| `diff_snapshots` | `diff --json` | `include_volatile`; cross-model diffs reuse `diff_with_schema`. Counts + top-N first, paginated with `truncated` (a Z 9 pair diff is large). `diff` already ignores the SDK-uninitialised enum `default_index` unless `include_volatile` (F6) |
| `firmware_status` | `firmware latest --json` | needs the types lifted out of `main.rs` (§2); **network access, not offline**; scraped strings are untrusted |

Phase 2, live camera reads (through the worker) and one new-file write. None of these changes a
camera, so none needs `--enable-writes`:

| Tool | Backs | Notes |
|---|---|---|
| `capture_snapshot` | `snapshot` | `serial` required (a USB serial; `id-N` refused, §3); optional `label`, stored as `mcp-<label>`; refused on `ambiguous_pairing` (§3). Writes a **new** file only: today `Snapshot::save_to_file` is `fs::write` and overwrites, so `ops` needs `OpenOptions::create_new`. Reports `read_ok`, `read_err`, the failed codes and the no-get-bit count, and fails when the capture misses the model baseline (§5.5: a capability readable in the baseline is not readable now), not only when `read_ok == 0`. A capture that fails the gate writes no file |
| `check_against_reference` | `check` | live read, diff vs the camera's reference; same capture gate and pairing refusal. Writes no file |
| `transplant_plan` | `transplant --dry-run` | Inputs: source snapshot name, target `serial`, optional `allow_raw_code` (list of property names, validated against the snapshot before the camera is touched; same rule as the CLI flag). Live reads only. Returns the outcome report (§5) and a `plan_digest`. This alone answers "make my Z6III as close to this Z 9 as you can, and tell me what did not get copied" up to the apply step |
| `restore_plan` | `restore --dry-run` | Same shape; accepts `only` (§5.6) |

Phase 3, camera writes (only with `--enable-writes`):

| Tool | Notes |
|---|---|
| `transplant_apply` | Requires the plan's `plan_digest` **and an echoed `summary`** (§5.4). Takes a restore point, then writes (§5.5) |
| `restore_apply` | Same two-step shape (digest and echoed `summary`). `serial` is required and must equal the snapshot's serial (`serial_mismatch` otherwise; copying onto another body goes through `transplant`, which reports `same_model: true`). Takes its **own** restore point and goes through the same baseline gate, `no_baseline` and `ambiguous_pairing` refusals as `transplant_apply`, then reads back and reports the same four outcomes (§5.5) plus `residual`: properties that still differ from the snapshot afterwards. So an undo is itself undoable. Used to undo: `only` defaults to written plus side effects (§5.6). Skips the same never-copy properties |

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
| `literal_unconfirmed` | policy | **planned, not in the code yet:** a numeric literal scalar would change the target and was not opted in (§5.3) |

Report shape: counts per tag first, then the per-property list grouped by tag with the warn-worthy
tags first; each written or would-write enum carries its `ResolvedChoice` (option label or `code
N`, target index, what the target has now, unchanged or not). A paginated list sets `truncated`.
The report must also state what the SDK cannot read at all (**Picture Controls**, F5), because that
is invisible in a per-property list. When source and target are the same model it carries
`same_model: true` (the CLI's prose note "`restore` is simpler for a same-model copy" becomes this
field) and the plan still proceeds.

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
either, which is right for an undo but a change for a backup/firmware-rollback restore. Decided
(§9): keep one table, since a second one is a second source of truth. `ShootNoCard` joins it
(Phase 0): it flipped across a power cycle with nothing writing to the camera (F10), so it is
state-like or reset by the body, and omitting one property from a copy costs little. `CCDDataMode` was deliberately left out (its meaning is unclear). Tests and notes that quote `never-copied=9` become 10.

### 5.3 Raw-code opt-in

A raw-code enum match assumes both bodies number the same option the same way, which nothing
guarantees. It is skipped as `raw_code_unconfirmed` **when it would change the target**, unless the
property is named in `allow_raw_code` (CLI: `--allow-raw-code <property>`, repeatable). A match that
resolves to the option the target already has is a no-op and passes without opt-in. Names are
validated against the source snapshot before the camera is opened; an unknown name, or one that is
not a raw-code enum, is an error, so a typo cannot masquerade as a safe run.

**Numeric literal scalars get the same gate** (F9: `WBAutoType`, `MovieRecMicrophone`,
`ElectronicVR`, ...; an integer is no more self-describing than a raw code). A literal that would
change the target is skipped as `literal_unconfirmed` unless named in the same `allow_raw_code`
list. A literal equal to the target's current value is a no-op and passes. The plan already reads
the target live, so telling the two apart costs nothing extra. Not implemented yet (§6, Phase 0).

### 5.4 What a plan promises

A plan is **not a prediction.** `restore --dry-run` once said `written=119 errors=0` and the real
run wrote 115 with 2 errors and exit 0 (F4); the live reads in `transplant --dry-run` find
`no_matching_option` but cannot find a failing write. Plan and apply tool descriptions say "would
attempt", and the apply result reports actual written/skipped/error counts and codes.

`plan_digest` is a **staleness guard**: it proves the camera or snapshot has not changed since the
plan. It is **not** a consent mechanism: an agent can plan then apply back-to-back without a human
seeing the plan. The real consent gates are `--enable-writes` and the client's approval prompt,
and that prompt shows the tool's **arguments**. So `transplant_apply` takes, besides the digest,
an echoed `summary` copied from the plan result (target model and serial, the number of
properties that would change, the names of any `allow_raw_code` opt-ins), checked against the
recomputed plan; a mismatch is `stale_plan`. That puts what is being approved in front of the
human. A server-side single-use token was considered and rejected (decided, §9): an agent willing
to chain plan then apply receives the token too, so it adds nothing.

The digest input must be **stable**: hash(source snapshot bytes, target serial, `allow_raw_code`,
per-property `(name, code, tag, resolved target choice)` in `BTreeMap` order). Never hash SDK
error text (`TargetReadFailed`/`WriteFailed` carry it), the volatile/garbage fields (§4, F6), or
the target's *current* value of a property whose outcome does not depend on it: a writable
state-like property not on the never-copy table (`ShootNoCard`, F10, until it joins the table)
would otherwise cause a spurious `stale_plan`. Current values appear in the plan report, and enter
the digest only as the unchanged/changed bit of each resolved choice.

### 5.5 Apply sequence (all in one call on the SDK child)

1. Recompute the dry run on the live target (with the same `allow_raw_code`); compare
   `plan_digest`. Mismatch ⇒ refuse with `stale_plan`.
2. Capture the target's current state as a snapshot named `mcp-restore-pre-transplant-<timestamp>`
   (the **restore point**), written create-new. **Abort before any camera write** if the file
   write failed, the restore-point cap is reached (`data_dir_full`, §3), or the capture fails the
   **baseline gate**: every capability that was readable in the model's baseline is readable now
   (a superset check on the readable set). "Zero read errors" is not usable: a healthy Z6III
   always reports `ok=200 err=12`, a Z 9 `ok=196 err=11` (F5). Neither are counts or the
   failed-code set alone, which miss a capability that is absent rather than failing. A
   mid-capture USB reset produced captures like `ok=18 err=194` that exit 0 today (F2), which is
   exactly what this must catch. `ops::capture` therefore returns `read_ok`, `read_err`, the
   failed codes and the readable set, and evaluates the gate **in memory before writing a file**,
   so a failed gate leaves no file. Baselines are a per-model file in the repo, keyed by model.
   F5 records counts and failed codes, not the readable set, so the files are seeded from the
   readable sets of the existing Z6III and Z 9 A snapshots. A model with no baseline can be
   captured, but cannot be applied to (`no_baseline`). The gate is deliberately strict and has no
   override in the MCP: a body in a legitimately different state (card out, movie mode) may lose
   readable capabilities and fail it, and the CLI is the escape hatch. Whether firmware must be
   part of the key is open (§9).
3. Run the real transplant. Verify by read-back (a fresh capture held to the same baseline gate,
   diffed against the restore point) and report the diff, since the process exit status cannot be
   trusted (§3). The result carries one status:
   - `applied_verified`;
   - `applied_unverified`: writes reported success but the read-back capture failed the gate or
     did not complete;
   - `failed`: with how many properties were written before the failure;
   - `indeterminate`: the worker produced no `result` (killed at the deadline, or died; §3), so
     writes may or may not have happened and the count is unknown. The tool result names the last
     progress phase and tells the caller to capture or `check` to find out. It is never reported
     as `failed`.

   The result also carries the written property names, the restore point's name, and
   **`side_effects`**: properties that differ in the read-back against the restore point, were not
   written, and are not on the volatile list or the never-copy table (writing one property can
   change others: dependent option lists, locked settings). The `result` line is flushed before
   teardown (§3).
4. `restore_plan`/`restore_apply` on the restore point, with `only` = written ∪ `side_effects`,
   is the undo (§5.6).

### 5.6 Restore and the restore point: honest limits

The restore point holds only what the SDK let us *read*: write-only or unreadable properties
(including Picture Controls) cannot be restored. **A full restore writes every snapshotted
property that is not on the never-copy table, not only the ones transplant touched, so any change
made to the camera after the restore point was taken is overwritten.** For that reason the undo is
a **targeted restore**: `restore_apply(point, only=[...])` writes back only the properties the
apply reported as written **and** its `side_effects`, leaving later changes alone and not
re-writing about 110 untouched properties. Including the side effects matters: restoring just the
written set would leave a property that the write changed indirectly at its new value. `only` is
new `restore` functionality (Phase 0); without it, `restore` keeps its current behaviour. Writes
go in the same order as `restore` uses (the snapshot's property-name order). No dependency-aware
ordering is attempted, so the undo does not trust itself: `restore_apply` reads back and reports
`residual`, the properties that still differ from the restore point afterwards, rather than
claiming success. Fidelity is verified for one property only (F4, F10): a one-property
cross-model change was applied, read back, and undone by a **full** restore that left the camera
identical to the restore point. The targeted form writes a strict subset of that, but has not
itself been run on hardware (§8.2), and no side-effect case has been seen. Restore-point files
land in `snapshots/` under the `mcp-restore-` prefix (§3); check that nothing treats "latest
snapshot" as meaningful, or filter them from the default lists.

## 6. Phases

**Before Phase 0 — two spikes.** Neither depends on `ops`, and each settles a design question in
§3 that Phase 0 would otherwise be built on.

- **A. `rmcp` spike** on the pinned version: a stdio server with two tools, one annotated,
  structured output, conditional registration under a flag (§8.2), and a tool that sleeps past the
  usual client timeout, run under the real clients, to see what timeout and cancellation do and
  whether progress notifications help (§8.2).
- **B. Worker experiment on hardware** (needs the user): a small parent that spawns the existing
  CLI `snapshot` as the child with a result fd and a deadline, and kills with SIGKILL and reaps.
  Confirms: the SDK writes nothing to the result fd's channel; the parent can read a result
  written before a teardown hang; the camera is still enumerable after a SIGKILL of the child
  (also mid-capture); whether the binary loads the SDK when spawned by a parent launched from an
  MCP client (codesigning, §8.2).

  **Run 2026-10-27 (F14), with both bodies attached.** Passed: the extra fd stays empty while the
  SDK writes to **stdout** (so the dedicated fd is needed); the plain release binary loads the SDK
  from a clean environment, new session and cwd `/`; 8 SIGKILLs (early, mid-capture, and just after
  the snapshot file was written; both bodies) each left both bodies enumerable and the next capture
  at baseline. **Not covered:** a real teardown hang (none occurred in the 10 uninterrupted captures), a kill mid-*write*
  (deliberately not tried), and a parent that is a real MCP client.
**Phase 0 — extract `ops`** (library). Status of each item:

| Item | Status |
|---|---|
| Reject empty captures: `snapshot_capture_failed` is `read_ok == 0`; `ensure_capabilities_present` after every `connect` | **done** (`aaa20da`), in `main.rs` |
| Whole-label packed-string matching; `AmbiguousLabel`; placeholder labels; `ResolvedChoice` on outcomes | **done** (`1043c31`), in the library |
| `NEVER_TRANSPLANT`, raw-code gate, `--allow-raw-code`, device-free `plan_property` | **done** (`dff83cd`), in the library |
| `restore` uses the never-copy table (`restore_precheck`) | **done** (`08db008`), in `main.rs` |
| `diff` ignores the SDK-uninitialised enum `default_index` unless `include_volatile` | **done** (`aaa20da`) |
| Move `cmd_snapshot`/`check`/`restore`/`transplant`, `restore_precheck`, `ensure_capabilities_present`, device selection and the ambiguity refusal into `ops`, returning structured results. Also move what they call and the MCP needs: `capture_capabilities` + `CaptureStats`, `enrich_devices`, `select_device`, `snapshot_serial`, `open_sdk`, `snapshots_dir`/`references_dir`/`reference_filename`/`ensure_dir`, and `is_safe_path_component`. `ops` writes nothing to stdout; printing stays in the CLI | todo |
| Hidden `nikon-fleet worker` subcommand: runs one `ops` call on the process main thread; JSON lines on a dedicated fd (`progress` lines, then one `result` line flushed before the SDK is torn down); stdout discarded (§3) | todo |
| Same-model refusal (`ambiguous_pairing`) in `ops` for capture/check/plan/apply; CLI keeps its warning | todo |
| USB-only camera listing (no SDK) with the USB-name to SDK-model-name mapping, and a test of the mapping | todo |
| `restore --only <property>` (repeatable) and the `ops` equivalent, for the targeted undo (§5.6) | todo |
| Gate numeric literal scalars like raw codes: `SkipReason::literal_unconfirmed`, same opt-in list (§5.3) | todo |
| Add `ShootNoCard` to `NEVER_TRANSPLANT` (and its real-config name check) | todo |
| `Serialize` on `Strategy`/`SkipReason` as closed tags (exhaustive match) and on outcomes | todo |
| `--json` on `ls`, `ref list`, `check`, `snapshot`, `transplant`, `restore`; `discover --json` gains `available`/`connected_pid` | todo |
| `ops::capture` returns `read_ok`/`read_err`/failed codes, uses create-new writes | todo |
| Per-model baseline file (readable-capability sets, seeded from F5) and the superset gate (also useful to the CLI: a partial capture still exits 0) | todo |
| One `ops::list` byte-identity rule, new for the CLI. The GUIs keep their copies; parity test only (§2) | todo |
| Cross-process `flock` on an open fd at a fixed per-user local path (not `--data-dir`), taken by CLI, GUI and MCP worker (§3) | todo |
| A small camera-port trait (`capabilities`/`read_capability`/`write_capability`) so plan/apply and the restore-point logic are testable without a `Device` (`plan_property` covers translate+gate only) | todo |
| Lift `firmware latest --json`'s types out of `main.rs` | todo |
| Strict USB-serial matcher in `ops` (refuses `id-N` and bare SDK ids, `no_usb_serial`); the CLI keeps `serial_matches` (§3) | todo |
| `mcp-` / `mcp-restore-` label prefixes and the two per-camera caps (§3) | todo |
| Read-back in `ops`: transplant returns `side_effects`, restore returns `residual` (§5.5-5.6) | todo |
| Capture gate evaluated in memory before the file is written (a failed gate leaves no file) | todo |

Phase 0 touches `src/main.rs`, `lib.rs` and `gui/`; the firmware session's earlier edits there are
committed, so nothing blocks it.

1. `mcp/` scaffold (the `rmcp` spike has already run, above), then the Phase 1 tools. Everything
   here is offline or USB-only: no worker yet.
2. The worker + watchdog + lock, then `capture_snapshot`, `check_against_reference`,
   `transplant_plan`, `restore_plan`.
3. `transplant_apply`, `restore_apply` behind `--enable-writes`.
4. Optional: persistent SDK session (cheap once the worker exists); `vendor_read`.

## 7. Tests (threshold / ambiguity list, written before code)

Already in the repo (do not re-derive): packed-string enum fixtures from the real Z 9 and Z6III
arrays with hard-coded expected indices; boundary/ambiguity/malformed cases for label matching;
never-copy table integrity and its real-config name check; the raw-code gate matrix; `--allow-raw-code`
validation, including a no-hardware CLI test; `restore_precheck` ordering; `diff` ignoring the enum
`default_index`; the empty-capture predicate.

To write for the MCP:

- Without `--enable-writes`: `transplant_apply` and `restore_apply` are absent (or refuse with
  `writes_disabled`), while the plan tools, capture and check are present and change no camera.
- `list_cameras` works with no SDK bundle present and takes no lock.
- `ops` (and so the MCP process) writes nothing to stdout during any operation.
- Two bodies of one model attached ⇒ `ambiguous_pairing` for capture, check, plan and apply, even
  with a `serial`; two bodies of different models do not trigger it. `list_cameras` flags both.
- A worker killed while holding the lock releases it (the next call is not `camera_busy`).
- Baseline gate: a capture missing a capability that the baseline reads (absent, not failing)
  fails; a capture with the baseline's known failures passes; a model with no baseline can be
  captured but `*_apply` returns `no_baseline`.
- Targeted undo: `only` restores exactly the named properties; a property changed on the camera
  after the apply and not in `only` is left alone. (Hardware, below.)
- `transplant_apply` with a `summary` that differs from the recomputed plan (refuses as stale).
- Result status: read-back capture failing the gate after successful writes ⇒ `applied_unverified`,
  not `applied_verified` and not `failed`.
- Numeric literal that would change the target ⇒ `literal_unconfirmed`; equal to the target's
  value ⇒ passes; named in `allow_raw_code` ⇒ written.
- Retention: `capture_snapshot` past the capture cap ⇒ `data_dir_full`, and nothing is deleted (restore-point cap: see the caps test below).
- Path outside `data_dir` (absolute, `..`, symlink, **relative path that exists under the cwd**)
  rejected for every path-taking tool.
- Two cameras attached and no `serial` ⇒ refused for capture and transplant; a `serial` is
  required even with one camera attached.
- `transplant_apply` with: matching digest (proceeds), digest differing by one outcome (refuses),
  missing digest or summary (refuses), restore-point capture failure (no write happens), restore-point
  file write failure (no write happens), **restore-point capture with partial read errors, e.g.
  `ok=18 err=194` (no write happens)**, **restore-point filename already exists (no overwrite, no
  write happens)**, `allow_raw_code` differing between plan and apply (refuses as stale).
- A process that finishes its write and then hangs/times out: the result is read from the
  read-back diff and reports success, not failure; a process that hangs *before* the write: no
  write is reported.
- Digest stability: two dry runs with identical state but different `WriteFailed`/`TargetReadFailed`
  text, or different volatile/garbage fields, produce the same digest.
- Empty snapshot properties; all-skipped plan (apply is a no-op and says so); source model equals
  target model (proceeds, with `same_model: true`); non-Z-series model
  rejected (existing guard).
- Pagination: exactly-N and N+1 properties around the page size; `truncated` correct at both, for
  `get_snapshot`, `diff_snapshots` and the plan list.
- Outcome tags: every `SkipReason` and `Strategy` variant maps to a distinct tag (exhaustive match;
  a new variant must fail to compile until mapped).
- Parity: CLI `--json` and MCP tool results come from the same `ops` value; `is_reference` agrees
  across `ops::list`, the Rust GUI and `fleet_lib.py` for the same fixtures.
- Live (needs the user and hardware): Z 9 ↔ Z6III plan/apply/undo on real bodies beyond the single
  enum-by-label property already done (F10): a range, a literal, an opted-in raw-code enum, and
  the **targeted undo** (`restore --only`) of each, with an unrelated property changed by hand in
  between to show it is left alone.
- Lock: the worker and a CLI run with a **different** `--data-dir` exclude each other (the lock
  is at the fixed path, not under the data dir).
- Fake worker (`--worker` points at a test binary): result then hang ⇒ the result is used, the
  worker is SIGKILLed and reaped, and the call succeeds; hang before any result ⇒
  `worker_no_result` (`indeterminate` for an apply); noise on the worker's stdout is ignored;
  garbage on the result fd ⇒ a typed error; non-zero exit after the result ⇒ the result is used;
  exit with no result ⇒ `worker_no_result`.
- Cancellation: before the `writing` phase ⇒ aborts, no write; after it ⇒ ignored and the apply
  completes. Client gives up, then retries the apply ⇒ `stale_plan`, not a second write.
- Two camera tool calls issued concurrently by one client queue in order and neither gets
  `camera_busy`; a holder outside the server ⇒ `camera_busy`.
- `serial` forms: a USB serial is accepted; `id-N` and a bare SDK id are refused
  (`no_usb_serial`) by capture, check, plan and apply; a snapshot stamped `id-N` cannot be a
  target.
- Side effects: a fake camera where writing A also changes B ⇒ B is in `side_effects` and in the
  default `only` of the undo; a property the undo fails to restore appears in `residual`.
- `restore_apply`: `serial` differing from the snapshot's ⇒ `serial_mismatch`; it takes its own
  restore point (failure of that ⇒ no write); `no_baseline` refuses.
- Caps and prefixes: MCP-created files carry `mcp-` / `mcp-restore-` regardless of the label the
  agent gave; the restore-point cap refuses an apply before any write; neither cap deletes
  anything.
- A capture that fails the baseline gate writes no file.
- `list_cameras` with a USB product string that has no mapping reports `unknown_model` and does
  not pass the string through as a model name.

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
| The SDK dylib writes nothing to stdout | **no**: it prints `InitializeSDK Execution duration: N` itself | F14 |
| The SDK dylib leaves an inherited extra fd alone | yes (0 bytes in every run) | F14 |
| The plain `nikon-fleet` release build (linker-signed, no entitlements) loads the SDK when spawned from a clean env, new session, cwd `/` | yes, both bodies | F14 |
| SIGKILL of a capture child leaves the body enumerable and the next capture at baseline | yes: 8 kills, both bodies | F14 |

### 8.2 Still unverified (review these)

- **`rmcp`, checked 2026-10-27 against crates.io (not yet compiled against).** Verified: the
  crate is the official `modelcontextprotocol/rust-sdk`; `ToolAnnotations` has `read_only_hint` and
  `destructive_hint`; features `server`, `macros`, `transport-io` exist; minimum Rust 1.88. Also
  found: 3.8.0 is one day old, with many 3.x minors since July, so the version is pinned (§3).
  **Still unverified:** structured tool output, and **registering tools conditionally at startup**
  (the `--enable-writes` design wants it; the §3 fallback is a handler-side `writes_disabled`).
  Spike A (§6) settles both.
- Tool annotations (`readOnlyHint`, `destructiveHint`): assumed advisory only. The server-side
  `--enable-writes` gate does not rely on the client honouring them.
- The worker behaves as assumed as a **separate process running the SDK on its main thread**. That
  is the CLI's configuration, so it should hold, but nothing has yet run it with the parent
  holding a pipe/fd for results and killing it on a deadline, or confirmed that the SDK dylib
  never writes to the result fd's channel (hence a dedicated fd, §3), or that a SIGKILL of a
  worker holding a body leaves it enumerable (F3's hangs were ended with SIGTERM/alarm). Spike B
  (§6, F14) settled the fd and SIGKILL questions: the SDK writes to stdout, not to an inherited
  fd, and 8 SIGKILLs left both bodies healthy. **Still open:** a parent that reads a result and
  then kills a worker in a real teardown hang (not provoked in the 10 uninterrupted captures), and a kill mid-write.
- Whether `available` / `connected_pid` (already read into `sdk::DeviceInfo`) mean "held by
  another process" while that process holds a body. No longer needed by `list_cameras` (USB-only),
  so this is a CLI nicety, not a blocker.
- Whether the USB-only camera list and the SDK's list agree on model and serial for every
  attached body, including the USB product string to SDK model-name mapping (`Z6_3`).
- **Targeted restore (`only`)** on hardware: verified only as a subset of the one-property full
  restore (F4, F10).
- The teardown hang's rate and trigger; in particular whether it follows "first open after the body
  was attached or woken" (replug or power-cycle, then capture several times).
- Whether a **per-camera** lock is enough (F12 is inconclusive).
- A reset-initialised open while another process is mid-*write* (not attempted).
- Restore and transplant fidelity beyond one enum property; restore after a power cycle; range,
  string and boolean property round trips.
- The Z 9 B, and same-model pairs (`warn_if_ambiguous_pairing` says serial pairing there is by
  enumeration order). Until a way to verify pairing exists, the MCP refuses these (`ambiguous_pairing`, §3), so
  this is a limit on use, not a risk.
- Whether the judgement-call entries in the never-copy table are the right set, and whether
  `ShootNoCard` (flipped across a power cycle) belongs in it.
- **MCP client timeouts and cancellation.** How the clients in use (Claude Desktop, Claude Code)
  time out a long tool call, whether they send a cancellation, and whether progress notifications
  extend the timeout are all unverified. The worker protocol (§3) is designed around plausible
  answers, not measured ones. Spike A (§6).
- **Codesigning of the worker.** `scripts/setup-sdk-runtime.sh` and `AGENT-README.md` say
  SDK-loading binaries need ad-hoc signing (`sdk-entitlements.plist`, `disable-library-validation`).
  The plain release build, which is linker-signed with no entitlements, loaded the SDK from a clean
  environment, new session and cwd `/` (F14), so no extra signing step was needed. Launching it
  from `fleet-mcp` under a real MCP client is still untested.
- Whether the readable set of a healthy body shifts with body state (card removed, movie mode,
  live view), which would make the strict baseline gate (§5.5) refuse a good camera. Seen stable
  across about 20 captures per model; not varied deliberately.
- Whether writing one property changes others on real bodies (the `side_effects` case, §5.5).
  None seen in the one-property tests.

### 8.3 Known defects and gaps accepted for now

- The CLI `snapshot` still accepts a *partial* capture (e.g. `ok=18 err=194`) with exit 0 (F2).
- Numeric **literal** scalars (`WBAutoType`, `MovieRecMicrophone`, `ElectronicVR`, ...) are copied
  as-is today, with the same unverified-meaning risk as raw codes and no gate (F9). The gate is
  decided (§5.3) and is Phase 0 work; until it lands, the MCP does not ship `*_apply`.
- `restore --dry-run` reports counts that real runs can miss (F4).

## 9. Decisions

### Decided in the 2026-10-27 spec review (recommendations accepted by the user)

1. **Digest vs token (§5.4):** digest only, as a staleness guard, plus an echoed `summary`
   argument so the client's approval prompt shows what is being approved. No server-side token.
2. **Lock granularity (§3):** one global `flock` on an open fd. Revisit per-camera only with
   evidence (F12 is inconclusive). Its location is a fixed per-user local path, not `--data-dir`
   (third review).
3. **Never-copy table (§5.2):** one table for undo and for backup/rollback restore (a second table
   is a second source of truth). The four judgement calls stay. `ShootNoCard` is added.
4. **Numeric literals (§5.3):** gated like raw codes (`literal_unconfirmed`). `*_apply` does not
   ship before the gate does.
5. **Per-model capture baselines (§5.5):** a per-model repo file of readable capabilities, seeded
   from F5, superset gate; a model with no baseline can be captured but not applied to.
6. **GUI migration onto `ops::list` (§2):** not in Phase 0; parity test only.
7. **Source model equals target model:** proceed, with `same_model: true` in the report.
### Decided in the 2026-10-27 third review (folded in at the user's instruction)

8. **Lock location (§3):** a fixed per-user local path, not `--data-dir`.
9. **Targets by USB serial only (§3):** `id-N` and bare SDK ids are refused by the MCP tools.
10. **Worker result protocol (§3):** result flushed before teardown and final once read; SIGKILL
    and reap; `indeterminate` when there is no result; progress lines; an apply cannot be
    cancelled once it is writing.
11. **In-process FIFO queue (§3):** `camera_busy` means a holder outside this server.
12. **Undo set (§5.6):** written plus read-back side effects; the undo reports `residual`.
13. **`restore_apply` shape (§4):** serial must equal the snapshot's; takes its own restore point;
    same gates and outcomes as `transplant_apply`.
14. **MCP-created file prefixes and caps (§3):** `mcp-` for captures, `mcp-restore-` for restore
    points, separate per-camera caps.
15. **Order of work (§6):** the `rmcp` spike and the worker experiment come before Phase 0.

### Still open

- Whether the baseline key must include firmware (F5 suggests it may). Cheapest path: key by
  model, record the firmware in the file, and make `no_baseline` fire when a capture's firmware
  differs from the baseline's *and* the readable set differs.
- The two retention caps' numbers (§3; captures and restore points separately), and whether
  restore points should ever be pruned by a human command.
- The per-tool worker deadlines, the apply one especially, and their relation to the clients'
  request timeouts (§3, §8.2).
- A verified way to pair same-model bodies, which would lift `ambiguous_pairing` (§3).
- Whether `rmcp` supports conditional tool registration and structured output (§8.2), decided by
  spike A (§6).

## 10. Out of scope

Any dependence of the CLI/GUI on this crate; remote/HTTP transport; Windows camera access;
automating the firmware licence; FTP profile writes; firmware flashing.
