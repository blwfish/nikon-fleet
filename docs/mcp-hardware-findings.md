# MCP plan: hardware findings log

What was measured on real cameras while checking `docs/mcp-plan.md` against the code, 2026-10-27.
The plan ([mcp-plan.md](mcp-plan.md)) states what is decided and what is still unverified; this file
is the evidence behind it. Entries are grouped by topic, in the order the work happened. Where a
finding led to a code change, the commit is named.

## Conditions

- **Bodies:** Z6III serial 0000003014527, firmware 2.00 (SDK name `Z6_3`); Z 9 "A" serial
  0000003023668, firmware 5.31. A second Z 9 ("B", serial 0000003029610) was never attached.
- **Binary:** release build of `nikon-fleet`, macOS, plain CLI process (no AppKit run loop), SDK
  bundle in `sdk-runtime/`. Almost everything used `--no-usb-reset`.
- **Method:** read-only unless stated. Scratch data directories (not the repo's `snapshots/`)
  except where noted. Hang detection used a watchdog (macOS has no `timeout`; a
  `perl -e 'alarm shift; exec @ARGV'` wrapper was used) and `sample <pid>` to capture stacks.
- **Sample sizes are small** and are quoted with the numbers. Rates are indications, not
  estimates.
- **Script note:** one concurrency loop had an off-by-one in a shell array under zsh (arrays are
  1-indexed there); the offsets that actually ran are stated in F12.
- **Code changes that came out of this work** (all on `master`): `aaa20da` (empty-capture failure,
  ignore the uninitialised enum `default_index` in `diff`), `1043c31` (packed-string enum matching),
  `dff83cd` (never-copy table, `--allow-raw-code`), `08db008` (`restore` shares the never-copy
  table).

## Index

| # | Finding | Status |
|---|---|---|
| F1 | `--no-usb-reset` enumerates already-connected bodies | verified |
| F2 | A reset-initialised open during another process's capture yields empty or partial snapshots | empty case fixed (`aaa20da`); partial case open |
| F3 | The SDK can hang in `DisconnectDevice` at teardown, solo and concurrent, including after a write | open; shapes the design |
| F4 | Restore-point round trip works for one property; dry runs do not predict write failures | verified (one property) |
| F5 | Unreadable-capability baselines per model; Picture Controls unreadable on both bodies | measured |
| F6 | Two noisy properties on an untouched camera; `MovieWBFluorescentType` `default_index` is SDK garbage | mitigated (`aaa20da`) |
| F7 | Z 9 A baseline | measured |
| F8 | Packed-string enums were matched by single bytes, writing wrong values | fixed (`1043c31`) |
| F9 | State properties and raw-code matches in a transplant plan | addressed (`dff83cd`) |
| F10 | First cross-model write on hardware, and its undo | verified (one property) |
| F11 | `restore` stops attempting state properties | fixed (`08db008`) |
| F12 | Two bodies attached together | verified for selection; concurrency inconclusive |
| F13 | Still unverified after all of the above | open |
| F15 | Phase 0 live checks: another teardown hang, then the SDK stopped enumerating a bus-visible body | open |
| F18 | Stepped ranges read `value` 0.0; the position is `value_index` (a transplant would have zeroed 33 settings); a blind range write is ignored | fixed; one stepped range written and undone on a Z 9 |
| F17 | The `worker` subcommand live: result delivered before a teardown hang; one SIGSEGV after a result | verified (one body) |
| F16 | Targeted undo (`restore --only`) on the Z 9: restores the named property, leaves a later change alone | verified (two enum properties) |
| F14 | Spike B: worker-style spawn, fd cleanliness, SIGKILL of a capture child | verified (small samples) |

---

## F1. USB reset vs enumeration

**Verified (Z6III, fw 2.00).** `--no-usb-reset discover` enumerates the already-connected body from
a plain CLI process: 3 runs, about 1.8-3.7 s, the same as with the reset. So
`initialize_no_usb_reset()` is viable as the MCP default, and the concurrent-reset hazard in F2 can
be avoided.

## F2. A reset-initialised open during another process's capture

**Observed (read-only; 2 trials, `discover` WITH the reset while a no-reset `snapshot` ran).**

- Reset at +1.0 s: the snapshot completed normally (200 ok / 12 err), but took 8 s instead of about 5 s.
- Reset at +2.5 s: the snapshot's `connect` succeeded, then it printed **"Reading 0 capabilities"**,
  wrote a 232-byte empty snapshot and **exited 0**, because `snapshot_capture_failed` was
  `read_ok == 0 && read_err > 0` and a 0/0 capture slipped through.

Consequences: the MCP must never reset while any other fleet process may hold a camera; and any
"capture succeeded" gate must treat `read_ok == 0`, and anything below the model baseline (F5), as
failure.

**Fixed in `aaa20da`.** `snapshot_capture_failed` is now `read_ok == 0`, and a new
`ensure_capabilities_present` aborts `snapshot`, `check`, `restore` and `transplant` right after
`connect` when the device reports 0 capabilities, before anything is read, written or saved.
Re-run on the Z6III with the reset landing at +2.4-2.7 s: 3 of 6 trials hit the 0-capability race,
and each now exits 1 with an actionable message and no file written.

**Still open: partial captures.** The other 3 of those 6 trials showed a second failure mode. The
reset landed a little later, and `snapshot` captured a *partial* snapshot (`ok=18 err=194`,
`ok=77 err=135`, `ok=101 err=111`), wrote it, and exited 0. `read_ok > 0` is not enough to call a
capture good: it has to be compared with the model baseline (`ok=200 err=12` on the Z6III,
identical failed codes). The CLI `snapshot` still accepts such a capture; a warning or non-zero
exit is worth adding once a per-model baseline exists.

## F3. Teardown hang in `DisconnectDevice`

All instances had the same stack: `Drop for Device` -> `DisconnectDevice` ->
`MaidCloseObjectEntryPoint`, parked in `CFRunLoopRunSpecific` on the main thread, burning CPU
slowly until killed. In every case the process had already finished its work (and written its
file, or completed its write). The camera was healthy afterwards each time.

1. **Z6III, concurrent (manual, 2 trials):** one `snapshot` ran while a second process opened the
   SDK (`discover`, no reset). Both returned correct results, but in one of two trials the
   snapshot process hung until SIGTERM (about 2 min).
2. **Z6III, concurrent (loop, 10 pairs, `discover` started 0.3-3.8 s after the snapshot, 30 s hang
   limit):** 1 of 10 hung, at the earliest offset (0.3 s: `discover` opening the SDK while the
   snapshot was still initialising or connecting); offsets 0.8-3.8 s were clean. Combined with
   (1) that is **2 hangs in 12 overlapping runs (about 17%, wide uncertainty)**, not purely
   determined by start offset: the first hang came at about 2.0 s, while 2.0 s was clean in the loop.
   The hang was always in the process that held the camera when the second open happened, never in
   the newcomer.
3. **Z 9 A, SOLO:** the first capture of the session hung for the full 120 s watchdog with **no
   other process using the camera.** The next 12 runs (2 + 10 solo) were normal: 1 solo hang in
   13. That first capture also read different `LiveViewZoomArea` options
   (`[0,256,512,1024,2048]` instead of `[0,320,640,1280,2560]` in the other 12), so the body's
   state differed too. A link with "first SDK open after the body was attached or woken" is
   plausible but **unproven**.
4. **Z6III, SOLO, AFTER A SUCCESSFUL WRITE:** during the first cross-model write (F10) the process
   printed `Transplant complete: written=1`, then spun until the 90 s watchdog killed it (exit 142).
   The write had landed. First solo hang on the Z6III, and the first after a write.

Consequences for the design:

- "Fails cleanly rather than wedges" is false; the failure mode is a hang in the process holding
  the camera, not an error in the newcomer.
- Every SDK call needs a watchdog. A call stuck inside the SDK cannot be cancelled from its own
  thread, so the worker has to be a **child process** the MCP can kill and respawn (or the MCP
  must mark itself poisoned and exit); otherwise one hang blocks the whole stdio server.
- Because solo hangs occur, a cross-process lock does not remove the need for that watchdog.
- **A timeout or non-zero exit must never be read as "the write failed".** The apply result has to
  come from structured output and a read-back/diff, not the process exit status.

## F4. Restore-point round trip, and dry runs vs real runs

**Verified for one property, same-model (Z6III fw 2.00).** Baseline snapshot B0. A hand-edited
one-property snapshot changed `HighlightBrightness` (a playback display threshold) from index 26 to
25 via `restore` (`written=1 errors=0`). A fresh snapshot B1 read the new value back (diff B0 vs
B1: only that property, plus the garbage field in F6). A **full** `restore` of B0 then followed,
and a fresh snapshot B2 was **identical to B0 ("no differences")**. So undo-by-full-restore returned
the changed property to its prior value, and re-writing the other ~114 properties did not disturb
anything detectable.

Not covered: any property class beyond one enum, unreadable properties (F5), and a restore after a
power cycle.

**The dry run does not predict failures.** `restore --dry-run` said `written=119 errors=0`; the real
run wrote 115, skipped 2 as `unsupported-type`, had 2 errors (`CurrentItemID` 0x8351 and
`LiveViewStatus` 0x823e, `SetCapability` -125: runtime state) and still **exited 0**
(`restore_completely_failed` needs `written == 0`). So a plan can only promise "would attempt"; the
apply result must report actual written/skipped/error counts and codes. (The state-property part of
this is fixed: F11.)

## F5. Unreadable-capability baselines

- **Z6III (fw 2.00):** 229 capabilities; `ok=200 err=12 skipped(no-get-bit)=17 unknown-schema=5`,
  identical on every run. The 12 failures: eleven `-126` (including `PictureControlDataEx2`
  0x8501, `MoviePictureControlDataEx2` 0x8502, `PortraitImpressionBalanceDataSet`,
  `SpecialShootingSettingInfo`, `GetStorageInfoDataSet`, the four SB attr caps) and one `-117`
  (`ResetWBMode`).
- **Z 9 A (fw 5.31):** 227 capabilities; `ok=196 err=11 skipped(no-get-bit)=20 unknown-schema=5`,
  identical on every run. The 11 failures are all `-126` and are the **same 11 codes as on the
  Z6III** (`AcceptDiskAcquisition`, the four SB attr caps, `PictureControlDataEx2`,
  `MoviePictureControlDataEx2`, `PortraitImpressionBalanceDataSet`, `SpecialShootingSettingInfo`,
  `GetStorageInfoDataSet`, `GetStorageIDS`); the Z6III additionally fails `ResetWBMode`.

Consequences: **Picture Controls are unreadable through this SDK path on both bodies**, so they
cannot be diffed, restored from a restore point, or transplanted, and the user-facing "what did not
get copied" report must say so explicitly, not only list per-property skips. The baselines differ
per model (200/12 vs 196/11), so any "capture is healthy" gate needs one per model, probably keyed
by firmware too.

## F6. Noise on an untouched camera

Seven Z6III snapshots compared: two properties differ.

- `AngleLevel` (live level sensor): differs every capture. Expected; already in the volatile list.
- `MovieWBFluorescentType`: its **`default_index` is garbage** (observed 0, 1, 26, 72350,
  50331648) while `value_index` is stable.

**Root cause:** the SDK never writes `NkMAIDEnum.ulDefault` for this capability (stale heap). Both
the GET_VALUE and GET_ARRAY structs carry the same junk within a call, and 26 is another
capability's real default; no capability operation bit distinguishes it from the 69 fine enums;
re-reading cannot detect it. It cannot be fixed at the source.

**Mitigated in `aaa20da`:** `diff` (and so `check`) ignores an enum's `default_index` unless
`--include-volatile`. The field stays recorded raw in snapshots so old and new files remain
comparable. Range values' `default_index` is not ignored (no garbage seen there). After the
change, an untouched Z6III diffs empty. **The garbage does not occur on the Z 9** (F7); it is
specific to some bodies. Anything hashed into a plan digest must still exclude volatile and garbage
fields.

## F7. Z 9 A baseline

No Z 9 SDK snapshot existed anywhere before this (the repo's `snapshots/` held Z6III and Z 5
files; `references/` only had a July PTP text dump). 13 read-only captures:

- Shape: see F5. About 4-5 s per snapshot, 196 properties.
- Volatile on an untouched Z 9: `AngleLevel` and `AngleLevelPitch` (both hidden by the existing
  `AngleLevel` substring). Ten of ten solo captures diff clean against each other.
- The first capture of the session differed and then hung: F3 (3).

A fresh Z 9 A baseline was saved to the repo's `snapshots/` (label `z9a-baseline-2026-10-27`;
`snapshots/` is gitignored). It was not set as the reference.

## F8. Packed-string enums matched by single bytes

Found with a Z 9 A snapshot -> Z6III `transplant --dry-run` (read-only). **`transplant`'s
enum-by-label path matched single characters, not labels, and would have written the wrong value
for 8 of the 17 label-matched properties.**

**Cause.** Packed-string enums (`elem_type` 7) come back from the SDK as one NUL-delimited byte
buffer with `elem_bytes=1` and `elem_count` equal to the total byte count (for example 178). The
`(7, bytes)` branch of `sdk::decode_enum_values` assumes `elem_bytes` is a per-string stride and
emits **one array element per byte** (`["J","P","E","G"," ","B","a",...,"",...]`, `""` standing for
each NUL). `value_index` indexes the *strings* (Z 9 and Z6III `MovieSensitivity` both read `'100'`,
at indices 2 and 0), so `values[value_index]` was an arbitrary single character, and
`translate_enum` then took "the first occurrence of that character in the target's byte list".
Unit tests missed it because they used synthetic whole-string arrays with `elem_bytes` 8.

**Measured on that pair.** Correct string-level outcome: 14 no-change, 2 change (`BracketingVary`
'AE & Flash' -> 'AE Only'; `MovieISOAutoHiLimit` '51200' -> '25600'), 1 skip (`Sensitivity`: the
Z 9's '64' does not exist on the Z6III). The old code would have written a wrong index for 8:
`CompressionLevel` idx 48 (only 13 real strings, so out of range), `ISOAutoHiLimit` idx 12 ('3200',
should stay '25600'), `MovieSensitivity` idx 3 ('200', should stay '100'), `FlashISOAutoHighLimit`,
`ISOAutoShutterTime`, `MovieWBMode`, `MovieISOAutoHiLimit`, and `Sensitivity` (written where it
should skip). The dry run printed all of them as plain "would write via enum-by-label".

**Not affected:** `restore` and same-model round trips (they copy `value_index` unchanged, which is
why the F4 restore test passed), `diff`, `check`. Snapshots still contain the full byte list, so
strings are recoverable (modulo `from_utf8_lossy` on non-ASCII bytes; the real captures contain
none).

**Fixed in `1043c31`** (option b: no snapshot format change). `translate_enum` rebuilds the option
strings from the stored byte list (`packed_string_options`: `elem_bytes == 1` -> join runs between
`""` elements, a final unterminated run counts; `elem_bytes > 1` -> elements are already strings;
anything else is rejected, not guessed) and matches whole strings. Two real-data cases forced extra
rules:

- **Duplicate labels** (`ISOControlSensitivity` lists most ISO values twice) are skipped as
  `SkipReason::AmbiguousLabel` if the label repeats on either side, instead of silently taking the
  first.
- **The SDK's `"Resource String not found"` placeholder** (every `EVInterval` option on the
  Z6III; all but the first `BracketingFactor` option) is never a label: as a source it is
  `EnumUndecodedBySdk`, as a target it simply never matches.

Dry runs print the resolved option (`via enum-by-label: "25600" (#21), unchanged` /
`"AE Only" (#1), was "AE & Flash" (#0)`), and the summary counts `skipped(ambiguous-label)`.
`TransplantOutcome` gained a typed `choice: Option<ResolvedChoice>`. Tests use a fixture of the real
arrays (`tests/fixtures/packed_string_enums_z9_z6iii.json`, 19 properties) with hard-coded expected
indices for all 17 label properties. Re-run on the same hardware pair: 16 label enums resolve
correctly (14 unchanged, 2 real changes), `Sensitivity` is now a skip, and the wrong writes are
gone.

## F9. State properties and raw-code matches

The same dry run (before the F8 fix: 107 properties would be written, 90 of them already matching
the Z6III) showed two further risks.

- **Runtime or state capabilities that should never be copied:** `LockCamera`, `LiveViewStatus`,
  `CurrentItemID`, `ExternalFlashStatus`, `RawJpegTransferStatus`, `ChangeMonitorOffStatus`,
  `ShootingMode`, `Viewmode`, `LiveViewPhotoShootingMode`, `CCDDataMode` (the first two already
  failed -125 on a real Z6III restore).
- **46 enums matched by raw numeric code**, which only means something if both bodies number their
  options identically (`PictureControl`, `MeteringMode`, `ShootingMode`, `AFSubjectDetection`,
  `MovieFileType`, `ImageColorSpace`, ...). For example Z6III `ShootingMode` 0 -> Z 9 2 and
  `MeteringMode` 1 -> 0 would change the camera on an unverified assumption.
- Skips were: 3 `no_matching_option` (`EVInterval`, `LockExposureEx`, `SensitivityInterval`), 1
  unreadable on the target (`FlashSyncTime`, -106), 2 `unsupported value shape` (`WBPresetName`,
  `MovieWBPresetName`), 76 read-only, 7 absent.

**Addressed in `dff83cd`.**

- `NEVER_TRANSPLANT` is one table of (code, name, reason), 9 entries, checked first in `precheck`
  (before any live read or write; no override): `LockCamera`, `LiveViewStatus`, `CurrentItemID`,
  `ExternalFlashStatus`, `RawJpegTransferStatus`, `ChangeMonitorOffStatus`,
  `LiveViewPhotoShootingMode`, `ShootingMode`, `Viewmode`. `LiveViewStatus` and `CurrentItemID` have
  hardware evidence (-125). The other seven rest on their names, since the SDK config does not
  classify state vs setting; the code marks four of them (`ChangeMonitorOffStatus`,
  `LiveViewPhotoShootingMode`, `ShootingMode`, `Viewmode`) as explicit judgement calls. A
  real-config test checks every (code, name) pair against the Z-series sections. (`CCDDataMode` was left out: its meaning is unclear.)
- A raw-code enum match is skipped as `SkipReason::RawCodeUnconfirmed` **when it would change the
  target**, unless named with `--allow-raw-code <property>` (repeatable; short or full name;
  validated against the snapshot before the camera is opened, so an unknown or non-raw-code name is
  an error). A match that resolves to the option the target already has passes without opt-in
  (a no-op). Dry runs list each gated property with what it would set.
- Tests are device-free (`TransplantOptions`, `plan_property`); 12 mutants of the new logic were
  killed; a CLI test covers the flag wiring without hardware.

**Measured on the real Z6III after `dff83cd`; the offline prediction held exactly:**
`written=90 literal=17 enum-by-label=16 enum-by-raw-value=36 range-clamped=21`,
`never-copied=9`, `raw-code-unconfirmed=7`, errors 0. The seven gated were `AEBracketingStep`,
`AEBracketingType`, `AFSubjectDetection`, `ImageColorSpace`, `MeteringMode`, `MovieFileType`,
`MovieScreenSize`; 36 raw-code enums already matched and passed. A dry run with
`--allow-raw-code MeteringMode --allow-raw-code kNkMAIDCapability_ImageColorSpace` moved those two
into the write list (`written=92`) and left the other five gated. A before/after snapshot diff
showed the Z6III unchanged by the dry runs.

**Not covered: numeric literal scalars** (`WBAutoType` 3 -> 2, `MovieWBAutoType`,
`MovieRecMicrophone` 0 -> 5, `ElectronicVR`, `MovieWindNoiseReduction`) are copied as-is. An integer
is no more self-describing than a raw code, so they carry the same unverified-meaning risk and
arguably want the same gate (it needs a live read of the target to tell a no-op from a change).

## F10. First cross-model write, and its undo

Z 9 A source -> Z6III (fw 2.00). The source was the Z 9 baseline cut down to one property
(`BracketingVary`). A restore point
(`snapshots/Z6_3_0000003014527_pre-transplant-2026-10-27_20261027T172600.688812Z.json`) was taken
first: `ok=200 err=12`, the model baseline.

- Dry run: `"AE Only" (#1), was "AE & Flash" (#0)`. Real run: `written=1 errors=0`, then the
  teardown hang of F3 (4).
- A fresh snapshot differed from the restore point in exactly that one property (`value_index`
  0 -> 1).
- Undo by **full `restore` of the restore point**: `written=115`, 2 errors (`CurrentItemID`,
  `LiveViewStatus`, -125), exit 0; the next snapshot was **identical to the restore point ("no
  differences")**.

So the plan -> apply -> verify -> restore-point-undo loop works end to end for an enum-by-label
change on real hardware.

**Side observation: `ShootNoCard` flipped false -> true across a power cycle**, between the
snapshot taken after the earlier dry runs and the restore point. Nothing run here wrote to the
camera in between. It may be a card-slot-derived or otherwise state-like property; it was in the
transplant would-write list as a literal. Worth a look for the never-copy table.

## F11. `restore` and the never-copy table

`restore` attempted `CurrentItemID` and `LiveViewStatus` on every full restore and reported 2
errors, so `errors == 0` meant nothing for a restore point.

**Fixed in `08db008`.** `restore` reads the same `NEVER_TRANSPLANT` table through a device-free
`restore_precheck` (policy skip first, then absent, then read-only) and reports
`skipped(never-copied)`. Measured on the Z6III with the restore point: dry run `written=110
never-copied=9 errors=0`; real restore `written=108 read-only=81 unsupported-type=2 never-copied=9
errors=0` (was `written=115 ... errors=2`); the camera afterwards was identical to the restore
point. 5 mutants of the precheck were killed; the restore loop's `continue` needs a live device and
is covered only by this hardware run.

Behaviour change to note: `LockCamera` and the four judgement-call entries
(`ChangeMonitorOffStatus`, `LiveViewPhotoShootingMode`, `ShootingMode`, `Viewmode`) are now also
not restored. That is right for an undo but is a change for a backup or firmware-rollback restore,
so the list should be reviewed with both uses in mind.

## F12. Two bodies attached together (Z6III + Z 9 A)

Read-only.

- **Enumeration and selection behave correctly.** `discover` lists both (Z6_3 id 1, Z 9 id 2,
  correct serials and firmware; the same-model serial-pairing ambiguity does not arise with
  different models). `transplant --dry-run` with no `--serial` **refuses** ("2 Nikon cameras are
  available; pass --serial ..."): `transplant_target_ambiguous` on real devices, not only in the
  unit test. With `--serial` it targets the right body. `restore` picks its target from the
  snapshot's serial (Z 9 snapshot -> Z 9, restore point -> Z6III). **`snapshot` with no
  `--serial` silently picked the first available body (the Z6III)**, as documented; an MCP capture
  tool must refuse instead, because an agent would not notice which body it got.
- **`discover --json` still has no `available` or `connected_pid`**; whether `available` flips
  while another process holds a body is not observable from the CLI output and is unverified.
- **Concurrent captures of different bodies:** 8 simultaneous pairs (16 processes), no hangs,
  every capture complete (Z6III `ok=200 err=12`, Z 9 `ok=196 err=11`), enumeration intact after
  each pair. The Z 9 process started +0 (twice), 0.3, 0.6, 1.0, 1.5, 2.0 and 2.5 s after the Z6III
  one. Pairs took 4-7 s against 4-5 s solo. **Do not over-read this:** the Z6III-holding
  processes were exposed to a second SDK open exactly as in F3, and at the earlier ~17% rate a
  clean run of 8 such processes has roughly a 1-in-4 chance by luck, so it does not show that
  different-body concurrency is safe. It is compatible with "the hazard is a second process
  touching the *same* body", and it shows two bodies can be read at once without partial
  captures. A per-camera lock is plausible but unproven; a global lock plus the watchdog remains
  the conservative design.

## F14. Spike B: spawning the CLI as a worker, and killing it

**Conditions.** Both bodies attached throughout (Z6III fw 2.00, Z 9 A fw 5.31). The release CLI
(`target/release/nikon-fleet`, linker-signed ad hoc, no entitlements) was run by a Python parent
with a minimal environment (`PATH=/usr/bin:/bin`, `HOME`), cwd `/`, a new session, stdin
`/dev/null`, stdout and stderr piped separately, and an extra pipe passed as fd 3. Command:
`--no-usb-reset --data-dir <scratch> snapshot --serial <S>`. Read-only against the cameras.

- **Clean spawn.** Z6III: exit 0, 4.66 s, `ok=200 err=12`. Z 9: exit 0, 4.81 s, `ok=196 err=11`.
  Both match the F5 baselines, so no codesigning or entitlement step was needed for the plain
  build in this launch context.
- **fd 3 stayed empty (0 bytes in both captures); the SDK writes to stdout.** The line
  `InitializeSDK Execution duration: 202` appeared on stdout. It is not in `src/`; the string is
  in the SDK binary. So the plan's dedicated result fd is needed, and the worker's stdout has to
  be discarded or kept off the protocol channel.
- **SIGKILL of the child, 8 trials, then recovery.** Kills at +0.5, +2.0 and +3.5 s (Z6III) and
  +2.0 and +3.5 s (Z 9), against a normal run of about 4.6 s; and three kills the moment the
  snapshot file appeared (about 2.9-3.0 s; Z6III twice, Z 9 once), i.e. after the result existed
  and before teardown finished. Every child exited -9. After each kill, `discover` listed both
  bodies and a full capture matched its baseline (4.56-4.74 s, exit 0, no hang).
- **Not covered.** No teardown hang occurred in the 10 uninterrupted captures (plus 8 recovery `discover`s) (F3's solo rate was about 1 in 13),
  so "read the result, then kill a hung worker" was exercised only as the file-written kills.
  A kill during a *write* was deliberately not tried. The parent was a script, not an MCP client.
  Phase timing of the kills is inferred from elapsed time, not from the child's progress.

## F15. Phase 0 live checks (2026-10-27, Z 9 A only)

The Z6III was not attached during this session.

- **Numeric-literal gate, live.** A read-only `transplant --dry-run` of the Z 9 baseline onto the
  Z 9 itself: 16 literals, none blocked (all equal to the camera's value), `never-copied=10`. With
  `WBAutoType` edited from 2 to 3 in a scratch copy of the snapshot, the dry run blocked exactly that
  property: `would set 3, was 2`, where 2 is the value the camera really holds. After the replug
  (below), the same dry run with `--allow-raw-code WBAutoType` listed `WBAutoType` as `would write via
  literal`: `literal=16`, `literal-unconfirmed=0`. That is a dry run; the real opted-in **write** has
  still not been run on hardware.
- **Another teardown hang (F3).** The first dry run completed its work and then sat in
  `DisconnectDevice` (same stack as F3) for over five minutes, consuming CPU, after a **read-only**
  run with no other process using the camera. It was ended with SIGKILL; the next capture was at
  baseline (`ok=196 err=11`). Its output was lost because it sat in a pipe buffer: a caller must
  read results from a file or fd as they are produced, as the worker protocol does (mcp-plan §3).
- **Enumeration stopped.** After that, one more dry run completed normally, and the next run
  found `no Nikon cameras detected`, as did every later `discover`, with and without the USB
  reset, over several minutes, while `system_profiler` still listed the Z 9 (version 5.31, serial
  0000003023668). No fleet process was running. `ptpcamerad`, `icdd` and Photos were running; they
  were running before and I did not stop them. Cause unproven: it may or may not be related to the
  SIGKILL of the hung process (the capture after that kill worked, and so did one more run).
  **A replug or power cycle of the body cleared it:** the next `discover` (no USB reset) listed the
  Z 9 at once and the following dry run completed with no hang. One observation, so a correlation,
  not a diagnosis.
- **Second occurrence, with no kill involved.** Later the same day, after the last successful run
  (a dry run that exited normally) and some idle time, `discover` again found no cameras while
  `system_profiler` listed the Z 9 and no fleet process was running. Nothing had been killed or hung
  since the replug, so the SIGKILL of the hung process is not the trigger. An idle body is a suspect but
  unproven; the second clearance was a physical replug, so sleep was neither confirmed nor ruled out. The practical rule stands: when the SDK reports no cameras for a body the USB bus
  still lists, no software step tried here (waiting, the USB reset) helped; the user has to replug.

- **Wake or replug cleared it again, and the baseline gate ran live.** The user reported "woke/replugged"
  the Z 9 without saying which; the user then confirmed it was **physically replugged / power-cycled**,
  not merely woken from sleep. So a replug is what cleared it both times; whether waking a sleeping body
  would also clear it is untested.
  Afterwards `discover` (no USB reset) listed the Z 9 at once. A capture through the gate
  (`ok=196 err=11`, the baseline's own numbers) passed silently and was written. To exercise the
  failure path on the same body, a throwaway binary (built in a scratch target dir, file restored
  afterwards) had a Z 9 baseline listing one extra capability the camera does not have: the capture
  failed with `1 capability readable in the Z 9 baseline ... could not be read`, exit non-zero, and the
  snapshot count did not change; with `--allow-partial` the same capture printed the warning and was
  written. Neither run hung at teardown.

## F16. Targeted undo with `restore --only` (2026-10-27, Z 9 A, fw 5.31)

Run with the user's go-ahead, writing to the body. Scratch data directory, release CLI.

1. **B0**: baseline capture, `ok=196 err=11`.
2. Two hand-edited copies of B0, each with one `value_index` moved one step:
   `HighlightBrightness` 26 to 25 (a playback display threshold, as in F4), and `HiResZoom` 0 to 1
   (chosen after `FlashRangeDisplay` turned out to be read-only on this body, which the new verdict
   reported correctly: `1 of 1 named property was not written ... (read_only)`, exit non-zero).
3. `restore <edit> --only <name>` for each: `written=1 ... skipped(not-selected)=195 errors=0` both
   times.
4. **B1** capture; `diff B0 B1` showed exactly two changes, those two properties, and nothing else:
   no dependent property moved.
5. **Undo**: `restore B0 --only HighlightBrightness` (`written=1`). **B2** capture; `diff B0 B2`
   showed exactly one change, `HiResZoom` 0 to 1: `HighlightBrightness` was back at 26, and the property
   changed after the restore point was taken was **left alone**, which is what a full restore of B0
   would not have done (not run).
6. **Put back**: `restore B0 --only HiResZoom`. **B3** capture; `diff B0 B3` showed `(no differences)`;
   with `--include-volatile` only `AngleLevel` and `AngleLevelPitch`, the known live sensors.

Teardown hangs (F3): 2 of the 12 SDK sessions in this run (the B2 and B3 captures) printed their
result and then sat in teardown until the harness killed them, 6 s after the result line; the writes
and captures were complete each time and the camera was healthy afterwards. Every other session,
including all three that wrote to the body, exited normally.

Not covered: ranges, literals and strings; a cross-model undo; a write that changes another
property; the MCP's own sequence (digest, restore point, baseline gate, read-back) end to end.

## F17. The `worker` subcommand on the Z 9 (2026-10-27)

`nikon-fleet worker --result-fd 3 capture --serial 0000003023668 --label worker-live`, with fd 3
redirected to a file by the shell (so the "channel" is a file, not a pipe), stdout discarded.

- **Success run.** Progress `starting`, `sdk_ready`, `connected`, `captured`, `written`, then an ok
  result: `property_count=196`, `read_ok=196 read_err=11`, `gate: passed`, a new file with the label
  `mcp-worker-live` (snapshot count 5 to 6). **The process then hung in teardown** and was still running
  4 s after the result line, so the harness killed it (exit 137). The result had already been written and
  flushed: this is the F3 hang happening for real, with the protocol's "result before teardown" rule
  doing its job. It is the first run in which the worker, not a script, was the thing that hung.
- **Typed errors.** A serial that no body has: `not_found`. `id-1`: `no_usb_serial`, before the SDK was
  opened (progress was only `starting`). Both were exit status 1.
- **One SIGSEGV after a result.** The `not_found` run exited **139 (SIGSEGV)** after it had delivered its
  error result, while the SDK session was being dropped without a device ever having been connected. Three
  repeats of the same command exited 1, and the plain CLI `snapshot --serial <wrong>` exited 1 three times,
  so it is intermittent and was not reproduced. No heimdall integration exists in this project to check
  (CLAUDE.md crash-diagnosis rule: a no-op here). Consequence for the design is the same as for the hang:
  the result line, not the exit status, is the answer, and a crash after it is not a failure.

Not covered: a worker killed before its result; two workers racing for the lock on real hardware (the busy
path is tested without hardware); the Z6III; the MCP server as the parent.

## F18. Stepped ranges: `value` is 0.0, the position is `value_index` (2026-10-27, Z 9 A)

Found by the first live `transplant-plan`, a read-only dry run of the F16 snapshot onto the Z 9 itself.

- **Symptom.** The plan reported three range properties (`WBTuneColorTempEx`, `MovieWBTuneColorTempEx`,
  `MovieRecMicrophoneValue`) as *changing* in a copy of a body onto itself, when the F16 diff had shown only
  two enums differing.
- **Cause.** The SDK fills a range's `value` field (`lfValue`) only for continuous ranges. Of the 39 range
  properties in a Z 9 capture, **33 are stepped and every one reads `value: 0.0`**; the setting is in
  `value_index`. The other six only look consistent because their real value happens to be 0.0 (for
  example `ExposureComp` at index 15 of -5..5). Examples: `WBTuneAuto` index 612 of 1225 over 0..1224,
  `WBTuneColorTempEx` index 250 of 751 over 2500..10000 (5000 K), all of `WBTune*` and `MovieWBTune*`.
- **Consequence had it shipped.** `translate_range` clamped the junk 0.0 into the target's bounds and derived
  the index from that: `WBTuneAuto` 612 to 0, `WBTuneColorTempEx` 5000 K to 2500 K, for about 21 settings
  (the `range-clamped` bucket of every earlier dry run), even from a body onto itself. The post-write check
  read back the same junk field, so it would have passed wherever the intended value was 0. `restore` was
  never affected (it writes back the whole stored struct). No range has ever been written to hardware: F10
  wrote one enum.
- **Fix** (`2068310`). `range_value()` derives a stepped range's number from its index; translation, the
  per-write check, the plan's change bit and the apply read-back use it, and a translated range carries the
  target's index and the value at that index. Re-run live: the same plan reports only the two real changes
  and all 22 `range_clamped` outcomes as unchanged.
- **Still unknown.** Which field the camera honours when a range is *written* (the SDK struct carries both
  `lfValue` and `ulValueIndex`). The fix writes them coherently, which is correct either way, but a real range
  write has not been run. This is the first thing to confirm on hardware, with a restore point and a
  one-property plan, before any broad apply.

### F18 addendum: the first range write, and the undo that did not take (2026-10-27, Z 9 A, fw 5.31)

Run with the user's go-ahead. Property: `WBTuneAuto` (code 0x8119, 612 of 0..1224 in 1225 steps). Scratch
data directory, release binary, worker ops with their result channel on a file.

1. **Pre-check.** `check` against the old F16 baseline showed one difference: `LiveViewZoomArea`'s option
   list, `[0,256,512,1024,2048]` in the reference and `[0,320,640,1280,2560]` live, same selection (F3 (3)
   saw the same variation). Not a setting. A fresh baseline (196 properties, gate passed) was captured and
   used as the reference for the rest of the test.
2. **Plan.** A source snapshot holding only `WBTuneAuto` at index 613. `transplant-plan`: copied 1,
   `changes: true`, summary `Z 9 0000003023668: 1 property would change; opted in: none`.
3. **Apply** (`transplant-apply` with the digest and summary): phases `plan_verified`,
   `restore_point_written`, `writing`, `read_back`; **`applied_verified`**, written `[WBTuneAuto]`, no write
   errors, no side effects, `not_taken` and `unverifiable` empty. The restore point (196 properties) holds
   index 612. An independent CLI capture diffed against the baseline showed exactly one change, `WBTuneAuto`
   612 to 613.
4. **Undo through `restore --only WBTuneAuto` from the restore point: no effect.** Both runs printed
   `written=1 errors=0`, and a following capture still read 613. So a restore of a range can report success
   and do nothing.
5. **Isolating it.** The same change in the other direction through the worker's `transplant-apply`
   (`applied_verified`, 613 to 612) took effect. `nikon-fleet transplant` (612 to 613) took effect. Both of
   those *read the capability before writing it*; `restore` wrote blind. After `restore` was changed to read
   a stepped range first (`626950c`), `restore --only` took effect (613 to 612). A read before the write is
   sufficient; why is unknown.
6. **End state.** A capture diffed against the pre-test baseline: `(no differences)`; with volatile
   properties only `AngleLevel` and `AngleLevelPitch`. 20 snapshot files in the scratch directory, two of
   them restore points.

Writes to the body in this test: four that took effect (612 to 613 by apply, 613 to 612 by apply, 612 to 613
by `transplant`, 613 to 612 by the primed `restore`) and two blind `restore` writes that were accepted and
ignored. The setting was moved one step and back each way; it ended where it started. No session printed a
teardown hang.

What this settles and what it does not: a stepped range write through the SDK struct, with `value` and
`value_index` coherent, changes the setting, and the read-back and an independent capture agree. It does not
say which field the camera reads (both were always coherent). Continuous ranges, other stepped ranges, a
cross-model range translation and ranges with a dependent effect are still unwritten. The ignored blind
write is a general warning: **success from a write call proves nothing; only a read-back does**, which is why
`restore_apply` must report `residual` (`ops::readback::residual`). It now does: the CLI's restore and the ops
`restore_flow` both read the body back, and a body that accepts and ignores a write comes out
`applied_unverified` in tests on a fake body (`ignoring_writes`, and `ignoring_blind_writes` for the range case). Not yet
confirmed on hardware.

Also from the same runs: the digest of two real plans taken minutes apart was identical, and the summary
counts three boolean/string-valued properties as "would change" because they are written without a read
(their current value is unknown), so it says 5 when the real number is 2.

## F19. `restore-plan` / `restore-apply` on the Z 9 (2026-10-27)

First run of the worker's restore ops on hardware, with permission, one harmless stepped range
(`WBTuneAuto`, 33049; index 612 to 613 and back). A fresh baseline was captured (gate `passed`, 196
properties); a source snapshot was made from it with `value_index` 613; then:

1. `restore-plan --only WBTuneAuto`: `would_change` 1, one step `would_write` with `changes: true`.
2. `restore-apply` with that digest and summary: phases `plan_verified`, `restore_point_written`, `writing`,
   `read_back`; `applied_verified`, `residual` empty, no side effects; restore point
   `mcp-restore-pre-restore-20261027T223626.793299Z`.
3. The undo went through the same ops, `restore-plan` then `restore-apply` on that restore point with
   `--only WBTuneAuto`: `applied_verified`, empty residual, its own restore point.
4. A final capture diffed against the baseline: no differences.

What this settles: the restore flow's priming read works against the real body (the write took, where a blind
range write would have been ignored, F18), the read-back agrees with an independent capture, and an undo is
itself a restore. Two writes in all, both on this one property. The residual path for an ignored write is
still only tested on a fake body (nothing on this camera ignored a primed write). The worker again hit the
post-result teardown hang on the apply (killed after the result line was read, F3).

## F20. `fleet-mcp` through a real client (Claude Code 2.1.308, 2026-10-28)

`fleet-mcp` was registered with Claude Code (`claude mcp add`, local scope, data dir = the repository, no
`--enable-writes`) and driven by a headless `claude -p` run that was told to call `list_cameras`,
`capture_snapshot` and `transplant_plan` and not to call any apply tool. The Z 9 was attached.

- All three calls succeeded on the first attempt, through the worker for the last two. Claude Code
  loaded the tools lazily (its tool search) and then called them with correct arguments, including the
  USB serial it had just read from `list_cameras`. Only the three named tools were called; the server had
  no apply tools to call. 5 turns, 28 s in all, nothing written to the camera.
- `list_cameras`: one Z 9, `complete: true`. `capture_snapshot`: gate `passed`, 196 properties, a new file
  in `snapshots/` (so a capture does add a file to the repository's gitignored data directory).
  `transplant_plan` of the 2026-06-02 Z6III snapshot onto the Z 9: 33 copied, 6 of them would change, 4
  `literal_unconfirmed`, 10 `never_copied`, 67 `malformed_source`, digest and summary returned.
- The client's own report questioned two things, both explained by what is already known and neither a
  defect in the plan: the 67 `malformed_source` come from that snapshot being from before the packed-string
  fix (F8): all 98 of its enum properties have an empty `values` list; and the 11 failed reads in the capture
  are the Z 9's healthy baseline (196 read, 11 not, F5).
- **Usability gap this showed:** the capture result lists the 11 failed reads and `gate: passed` but does
  not say that 11 is the expected number for this model, so a cold reader (here, the agent) cannot tell a
  healthy capture from a degraded one without the findings file. Worth adding an `expected_unreadable`
  count or list from the baseline to the capture result. Likewise `malformed_source` could say that old
  snapshots need re-capturing.
- Not covered: Claude Desktop, any apply through the server, a client cancellation of a live camera call.

## F21. The same Claude Code run, with server instructions (2026-10-28)

`fleet-mcp` now sends `InitializeResult.instructions` (a bounded render of its notes, from the `mcp-agent-notes`
design) and serves the full notes through `get_usage_guidance`. The F20 prompt was run again, headless, with the same
three calls and the same source snapshot (the agent was also allowed to call `get_usage_guidance`; it did not).

- The two things the agent had questioned in F20 were now read correctly from the handshake alone: it called the
  capture's 196 read and 11 unreadable "the expected Z 9 profile", and it attributed the 67 `malformed_source` skips to
  the 2026-06-02 source being older than the packed-string fix, citing the instruction text, and advised capturing
  again. Same plan digest as before (`80ef8847…bdad36`), same counts.
- Still unexplained to it, so candidates for new notes or for result fields: `unknown_schema: 5` in the capture stats,
  `source_label: null` in the plan, and `changes: null` on `EnableBracketing`, `IsoControl` and `MovieISOControl`
  (written without a prior read, so whether they change is unknown).
- One run, one model, one prompt that asked it to say what looked wrong. It shows the instructions are read and used;
  it does not show that a weaker model follows the plan-then-apply rules (still untested, and no apply was involved).

## F22. The CLI on the shared ops path (2026-10-28)

`nikon-fleet transplant` and `restore` now plan and apply through `ops::plan` / `ops::apply` and
`ops::restore_flow`, the code the MCP worker runs, so they take a restore point first, hold it and the read-back to
the model's baseline, and report a status (`--allow-partial` waives the baseline for a restore point that is
incomplete or has no baseline). `snapshot`, `check`, `transplant` and `restore` gained `--json`, printing the same
values the worker returns. Read-only, on the Z 9:

- `transplant --dry-run --json` of the 2026-06-02 Z6III snapshot printed digest `80ef8847…bdad36`, the same digest the
  MCP's `transplant_plan` returned for the same source and body (F20, F21): one plan, two front ends.
- `snapshot --json` and `restore --dry-run --json --only WBTuneAuto` returned the worker-shaped values; the human
  `restore --dry-run` text is unchanged apart from two added lines (`Plan:` and `Digest:`).
- **A defect this found, present before the change:** the SDK prints `InitializeSDK Execution duration: N` to stdout
  itself, so every `--json` command that opened the SDK (including `discover --json`) printed that line before its
  JSON. `json_out::JsonOut` now moves stdout to stderr for the rest of the process and writes the JSON to a saved
  handle; stdout is pure JSON for all five commands (checked on the camera for `discover` and `transplant`).
- **Then, with permission, a real write through the CLI (Z 9, one property, `WBTuneAuto` 33049, a stepped range):**
  1. `snapshot --json` baseline (gate `passed`, 196 properties; `value_index` 612).
  2. `transplant --dry-run --json` of a one-property same-model source with `value_index` 613: 1 property would change,
     digest `2e80d9af…25f5`.
  3. `transplant --json` of that source: exit 0, `applied_verified`, restore point
     `…mcp-restore-pre-transplant-20261028T004857.247951Z…json`, read-back gate `passed`, nothing not-taken or
     unverifiable, no side effects. An independent `snapshot` afterwards, diffed against the baseline, differed in
     that one property only (612 to 613).
  4. `restore --json --only WBTuneAuto <that restore point>`: exit 0, `applied_verified`, verdict `verified` with an
     empty residual, its own restore point, no side effects.
  5. A final `snapshot` diffed against the baseline: no differences. Two writes in all, both on that property.
  What this settles: the CLI apply path writes, reads back and undoes correctly for one stepped range, and its
  JSON matches the worker's shape. What it does not: any other property type, a cross-model write, an opted-in
  raw-code enum, a failing or ignored write, or `--allow-partial` against a real body. The CLI exited cleanly after
  each of the three mutating or capturing runs except one read-only `snapshot` (the independent post-write
  capture), which was still alive 150 s after printing its result and was killed (the post-result teardown hang, F3);
  none of the writing runs hung.

Behaviour changes for CLI users: a real `transplant` or `restore` writes a restore point (`mcp-restore-pre-...`) into
`snapshots/`, refuses (`no_baseline`, `restore_point_incomplete`) unless `--allow-partial`, and exits non-zero on
any rejected write (before, only when every write failed) or when a written property does not read back. `restore`
across models, which used to warn and write anyway, is now refused with a pointer to `transplant`.

## F23. Both bodies attached: the Z6III's identity and gate, and the first live cross-model plan (2026-10-28)

The Z6III (serial `0000003014527`, firmware 2.00) was plugged in next to the Z 9. All read-only.

- **The model spelling is settled.** `discover --json` (SDK path) and the worker's `list-cameras` (USB descriptors only)
  agree on both bodies: `Z6_3` and `Z 9`, same serials and firmware, `known_model: true`, `pairing_ambiguous: false`
  for both (two bodies of different models do not trigger the refusal), `complete: true`. Until now `Z6_3` rested on
  the firmware survey; it was last read live before the identity code existed.
- **The Z6III baseline gate passes live.** A CLI `snapshot --json` of the Z6III read 200 properties and failed 12, gate
  `passed`: the 200/12 recorded in F5, now confirmed on the shipped baseline file and the current code, not only on the
  snapshots it was seeded from.
- **The motivating request, as a plan.** `transplant --dry-run --json` of a fresh Z 9 snapshot onto the Z6III: "7
  properties would change". Of 200 outcomes: 84 copied (36 by raw value, 21 range-clamped, 16 enum-by-label, 11
  literal) and 116 skipped: 76 read-only, 10 never-copied, 7 absent on the target, 7 `raw_code_unconfirmed`, 5
  `literal_unconfirmed`, 4 `no_matching_option`, 2 unsupported value shape, 1 `target_read_failed`. No malformed
  sources, since the source is a current capture (compare the 67 against the June snapshot in F20). Nothing was written.
- The read-only `snapshot` hung at teardown after printing its result (killed after 150 s; F3 again), the Z6III this time.
- **Then, with permission, a cross-model write (Z 9 to Z6III, one property).** From the plan above, the one `range_clamped`
  property whose two bodies were a single step apart with identical bounds and step count:
  `ISOAutoShutterTimeAutoValue` (33584, range -2 to +2 in 5 steps, the auto-ISO shutter-speed bias): Z 9 `value_index` 3,
  Z6III 2. A one-property source was made from the Z 9 snapshot (its own model and serial).
  1. Fresh Z6III baseline: gate `passed`, 200 read and 12 not, `value_index` 2.
  2. `transplant --dry-run --json` onto the Z6III: "1 property would change", digest `6452279…98c0`.
  3. `transplant --json`: exit 0, `applied_verified`, restore point
     `Z6_3_…mcp-restore-pre-transplant-20261028T005722.883599Z…`, read-back gate `passed`, nothing not-taken or
     unverifiable, no side effects. An independent `snapshot` diffed against the baseline: that property only, 2 to 3.
  4. `restore --json --only ISOAutoShutterTimeAutoValue <that restore point>`: `applied_verified`, verdict `verified`,
     empty residual, no side effects.
  5. A final `snapshot` diffed against the baseline: no differences. Two writes in all, on one property.
  The undo run printed its complete result and then hung at teardown (alive 150 s later, killed): F3 (4) again, after a
  write this time; the result had been written and the final snapshot confirmed the state. The apply run exited cleanly.
  What this settles: a cross-model write (`range_clamped`, different `Z 9` source and `Z6_3` target, no translation needed
  beyond the clamp) lands, reads back and undoes through the CLI. What it does not: an enum by label or by raw code, a
  literal, a range whose steps or bounds differ between the bodies, a multi-property apply, or the same through the MCP.
- **Still not run:** an opted-in raw-code enum, a literal, a range with different bounds, and any apply through the MCP.

## F24. The teardown hang is an unserviced run loop (2026-10-28, Z 9)

F3's hang in `DisconnectDevice` (the main thread parked inside the SDK's own `CFRunLoopRunSpecific`, every SDK thread
on a condvar) was reproduced and isolated with 248 read-only CLI `snapshot`s of the Z 9, run in randomised order on a
scratch build with env-var hooks (branch `f3-experiments`, not on master). A run counted as hung if it was still alive
25 s after start; a healthy `DisconnectDevice` takes 1.52 to 1.58 s in every arm, so the two are unmistakable.

| Arm (teardown condition) | Hung |
|---|---|
| control (as shipped before) | 14/40 (round 1: 8/24, round 2: 6/16) |
| sleep 0.3 s before `DisconnectDevice` | 16/16 |
| sleep 1 s | 16/16 |
| sleep 2 s | 24/24 |
| connect and disconnect, no reads | 0/24 |
| no in-process `rusb` (libusb) | 3/24 |
| pump the main run loop 0.2 s before disconnect | 0/16 |
| pump 0.5 s | 1/16 |
| pump 1.5 s | 1/40 |
| pump after every read, nothing at the end | 4/16 |
| pump after every read, plus 0.2 s at the end | 0/16 |

- **Any idle gap before the disconnect hangs it, every time.** Sleeping 0.3 s was enough. The control's 35% is the
  time spent between the last read and the disconnect (building and writing the snapshot).
- **Servicing the main thread's default-mode run loop for 0.2 s immediately before `DisconnectDevice` removes it**
  (2 hangs in 72 pump-before-disconnect runs, against 35%). Pumping during the reads alone does not (4/16). Working
  explanation, not proven (the SDK is closed source): events queue on the main run loop while we are idle, and the
  SDK's close then waits on a reply that depends on them. The in-process libusb thread may add a little (3/24 against
  8/24 in round 1, weak) and is not the cause.
- **The sleep arms do exactly the control's camera I/O and only add idle time**, and they hang 100%: so the trigger is
  the idle gap, not anything done to the camera. The experiments were all reads.
- **Fix:** `Device::drop` pumps the run loop for 300 ms (`PRE_DISCONNECT_PUMP`, above the shortest length tried) and
  then calls `DisconnectDevice`. The ordering is pinned by a unit test; whether the pump is *enough* is only a hardware
  question. **Verified on the Z 9:** 30 snapshots with the shipped binary, 30 exited cleanly in 3.6 to 5.0 s, none hung.
- **Z6III: 28 of 30 clean, 2 hung.** The same 30 snapshots with the shipped binary, `Z6_3` (id 2), a few minutes after
  it was plugged back in: runs 1 and 3 hung (killed at 25 s), runs 2 and 4 to 30 exited cleanly in 4.7 to 4.9 s. Both
  hangs were at the start, so this may be the "first opens after attach" effect F3 suspected rather than the idle-gap
  hang; 2 of 30 is also not distinguishable from a rate of a few percent. Neither reading is established. The control
  rate was only ever measured on the Z 9, so there is no Z6III baseline to compare against. Camera healthy afterwards
  (`discover` listed both bodies). **So the pump is not shown to remove the hang on the Z6III.**
- **Z6III repeat, settled: 26 of 30 clean, 4 hung** (runs 7, 17, 22 and 28), spread through the batch, clean runs
  3.4 to 4.9 s. So the first batch's early hangs were not an attach effect. **Across both batches the shipped fix leaves
  6 of 60 (10%) Z6III runs hung, against 0 of 30 on the Z 9.** The pump removes the Z 9's hang and only reduces or
  does not touch the Z6III's; whether that is a lower rate or the same rate as before the fix is unknown, since no
  Z6III control was run. The Z6III's hang may have a different or additional cause.
- **Z6III experiment arms (80 runs, the scratch build, randomised):** control 3/16, sleep 0.3 s **16/16**, pump 0.2 s
  0/16, pump 1.5 s 1/16, pump after every read plus 0.2 s 0/16; healthy disconnects 1.52 to 1.58 s as on the Z 9. So the
  Z6III has the same mechanism: any idle gap before `DisconnectDevice` hangs it every time, and servicing the run loop
  prevents most hangs. What does not line up is the shipped fix: 300 ms of pump before the disconnect and nothing else
  hung 6/60 on the Z6III, while the 0.2 s and 1.5 s arms of the experiment hung 1/32 together. With these counts
  (1/32 against 6/60) that is not distinguishable from chance, and the experiment build also printed trace lines around
  the disconnect, so the arms are not byte-for-byte the shipped code. Pooled over both bodies the best arm is the
  one that pumps after every read *and* before the disconnect: 0/16 on the Z 9 and 0/16 on the Z6III, against 4/16 for
  per-read pumping alone on the Z 9. **That arm is not what shipped.** Candidate next step: pump the run loop (zero
  duration) after each capability read as well, then repeat the 60 Z6III snapshots; the cost is one cheap call per read.
- **Per-read pump added and measured (not kept).** `read_capability` was made to pump the run loop (zero duration) after
  every read, on top of the 300 ms pump before the disconnect (the best experimental arm), and 60 Z6III and 30 Z 9
  snapshots were run with that binary: **Z6III 2/60 hung** (runs 30 and 52), **Z 9 3/30 hung** (runs 1, 13 and 29), clean
  runs 3.5 to 5.0 s. Against the end-pump-only fix that is Z6III 6/60 to 2/60 and Z 9 0/30 to 3/30, so across both
  bodies 6/90 to 5/90: no measurable gain, and neither difference is distinguishable from chance. The change was not
  committed (the patch is not in the repo). So the per-read pump is not the missing piece, and the arm that looked best
  (0/16 on each body) was probably a lucky sample.
- **What the 330-odd runs support, and what they do not.** Established: an idle gap before `DisconnectDevice` hangs
  both bodies every time (sleep arms: 16/16 and 16/16 and 24/24). Established: pumping the run loop first removes that
  *deterministic* hang. Not established: that anything we tried takes the residual rate to zero. The shipped code
  sits at about 7% over 90 snapshots (6 hung), where the pre-fix control was 17 of 56 (30%). The residual hangs may be
  a second cause, or load on this machine at the time (the Z 9 hung on run 1 of its batch, straight after a Z6III batch).
- **Skip `DisconnectDevice` and `_exit` (read-only experiment, both bodies).** A scratch build (master plus a hook; the
  patch is not in the repo) left the session open after the snapshot was saved, flushed stdout and stderr and called
  `_exit`, so neither `DisconnectDevice` nor any exit handler ran. 30 snapshots per body per arm, shuffled with the
  shipped fix as the control: **skip 0/60 hung** (Z 9 0/30, Z6III 0/30), all exit 0, **1.8 to 3.1 s** per run against
  3.6 to 4.9 s for the shipped fix (which hung 1/60: Z6III 1/30, Z 9 0/30). All 120 snapshots in the batch were
  complete (Z 9 196 properties, Z6III 200). **Every run connected normally, 60 consecutive opens after a session that was
  never closed, with no replug and no USB reset** (`--no-usb-reset`), and `discover` listed both bodies afterwards. So
  an open session left behind by an exiting process does not wedge either body, matching what killed hung workers
  already showed (F3, F14). Not tested: a write followed by a skipped disconnect, and a longer idle gap between
  processes. The cost saved is the fixed 1.5 s the SDK takes to close.
- **Shipped: read-only commands exit without closing the session.** `snapshot`, `check` and the worker's `capture`,
  `check`, `transplant-plan` and `restore-plan` now flush and `_exit` as soon as their output is written
  (`process_exit::exit_now`); both applies, `list-cameras` and every failure path keep the ordinary disconnect with the
  300 ms pump. Verified on hardware with the release binary: **CLI `snapshot` 30/30 per body exited normally** (Z 9 1.7
  to 3.1 s, Z6III 2.8 to 3.0 s), **worker `capture` 30/30 per body** (Z 9: exit 0, 196 properties, gate passed, 2.9
  to 3.1 s; Z6III: exit 0, 200 properties, gate passed, 1.7 to 3.0 s). **120 runs, no hang**, every open succeeded
  without a replug, and `discover` listed both bodies afterwards. The pre-fix control was 17/56 hung (30%) and the
  300 ms pump alone about 7%. Not exercised: `check` and the two plans through the worker (same code path and same
  `Teardown`, but only `capture` and CLI `snapshot` were run), and any write path, which is unchanged.
- **Not yet verified:** the residual-hang cause on write paths, a write path (`transplant`/`restore`),
  and the worker process. The supervisor's post-result grace and SIGKILL stay as a backstop regardless; the hang was
  never shown to be impossible, only rare after the pump.
- Camera state was healthy after every kill, as in F3 and F14.

## F25. The MCP server's untested paths on the Z6III, including a write (2026-10-28)

Z6III (serial `0000003014527`, fw 2.00), Z 9 also attached and untouched. The server was driven through its real binary
over stdio by a small Python JSON-RPC client (not through Claude Code or Claude Desktop). Writes were approved for one
property only: `ISOAutoShutterTimeAutoValue` (33584), the same property as F23, Z 9 `value_index` 3 onto Z6III 2.

- **Worker read-only ops on the Z6III, 3 runs each:** `check` (gate passed), `transplant-plan` (7 properties would change
  from a Z 9 source), `restore-plan` (0 would change from a fresh Z6III snapshot). All 9 exited 0 at the moment the result
  arrived, 2.7 to 2.9 s, so the exit-after-result path (F24) works for these ops too.
- **Read tools through the server, on real data:** `list_cameras`, `list_snapshots`, `capture_snapshot` (1.6 s, 200
  properties, **`expected_unreadable: 12`, `unexpected_unreadable: 0`** live for the first time), `get_snapshot` (paged),
  `diff_snapshots` (0 differences between two captures), `check_against_reference` (gate passed, 2.9 s),
  `transplant_plan` with `limit: 3` (196 outcomes in total, a 3-entry page with `next: 3`, same summary), and
  **`firmware_status` for the first time** (1.0 s; the Z 9 reported as `behind` at 5.31, the download `gated`).
- **An apply and its undo through the server, with writes enabled (13 tools):** `transplant_plan` ("1 property would
  change"), `transplant_apply` with the plan's digest and summary: **`applied_verified`**, 5.0 s, restore point stored
  first, `undo_set` the one property. Then `restore_plan` on that restore point with `only` = `undo_set` ("1 property
  would change"), `restore_apply`: **`applied_verified`**, 5.05 s, empty `residual`, verdict `verified`. An independent
  `capture_snapshot` and `diff_snapshots` against the pre-write baseline: **no differences**.
- **Teardown of the write path (still the ordinary disconnect, with the 300 ms pump): 24 more apply/undo cycles at the
  worker level, then stopped.** Each cycle: `transplant-plan`, `transplant-apply`, `restore-plan`, `restore-apply`.
  **All 48 worker applies were `applied_verified`, and none of the 96 worker runs hung** (applies 3.9 to 5.1 s wall; plans
  2.5 to 2.7 s). With the two through the server that is 50 applies, 0 hangs. This is evidence the write path is not
  worse than the read path was, not proof of a zero rate (0 of 50 would be unlikely at the 7% the pump alone left on
  reads, about 3%, but the paths differ).
- **The restore-point cap works on hardware.** The 25th `transplant-apply` was refused with `data_dir_full` ("50 MCP
  restore points are stored for this camera (cap 50)") **before any write**, which ended the loop. The 50 restore points
  were the 2 from the server run plus 2 per cycle. Nothing was deleted automatically.
- **Final state:** an independent CLI `snapshot` of the Z6III diffed against the baseline taken before the first write:
  **no differences** (`value_index` 2). Both bodies still enumerate. About 100 setting writes were made in all, all to
  that one property, all undone.
- Test-harness note: my first cycle script lost descriptor 3 whenever `os.pipe()` returned it as the read end, so every
  worker exited 2 immediately; fixed before any of the numbers above. Not a product defect.
- **`enum_by_label` on a write, Z 9 source onto the Z6III (later the same day).** Two single-property cycles with the
  worker ops, each: fresh Z6III snapshot, plan ("1 property would change", strategy `enum_by_label`), apply, independent
  snapshot and diff, undo from the restore point, snapshot and diff.
  - `MovieISOAutoHiLimit` (33874): **51200 (index 24) to 25600 (index 21)**. The Z6III's option list is longer than the
    Z 9's (150 against 132 packed bytes: it ends in `Hi 0.3` to `Hi 2.0`), and the label matched in the shared prefix.
    `applied_verified`, 5.1 s; the independent diff showed that one property, index 24 to 21; the undo was
    `applied_verified` with an empty residual and the diff against the "before" snapshot showed no differences.
  - `BracketingVary` (33063): **"AE & Flash" (index 0) to "AE Only" (index 1)**, lists identical. Same result: apply
    `applied_verified` 5.0 s, diff exactly that property, undo `applied_verified`, no differences afterwards.
  - A diff of the final Z6III state against the first baseline of the session: **no differences**. These two examples
    are labels that sit at the *same* index on both bodies; a label that sits at *different* indexes on the two bodies has
    not been written yet.
- **Not written, and why: `ShootingLimit` (33086).** The plan wants to change it (`range_clamped`), but the two bodies'
  *defaults differ*: Z 9 `default_index` 0, Z6III `default_index` 200 (range 0 to 200, 201 steps on both), and each body is
  currently at its own default. `translate_range` maps by the number (`lower + index * (upper - lower) / (steps - 1)`),
  so it would move the Z6III from its default to the other end. Whether the two bodies mean the same thing by that range
  is unknown, and the write could not be judged afterwards because a stepped range reports `value: 0.0`. This is a
  candidate defect in the range strategy, not a result: **a range whose defaults differ between source and target may not
  be safe to copy by number.** Unwritten and untested.
  - **Read-only follow-up (why the defaults differ is not known).** Across the 39 range properties in the two F25 baselines,
    38 agree on `default_index` (sensible values: 612 of 1225 for the `WBTune*` scales, 0 EV for exposure compensation),
    and the default is not just the current value echoed back (`MovieRecMicrophoneValue`: Z 9 at 2, default 14), so the
    default index looks trustworthy for ranges; `ShootingLimit` is the one that differs. The SDK's own files name it:
    `sdk-runtime/MaidLayer.config` labels it "Continuous Shooting Limit" (frames per burst) and
    `sdk-runtime/RangeValue.config` gives range 1 to 200, step 1, **default 200** (identical in all five sections, so it
    does not separate the bodies). The Z6III (index 200) matches that default. The Z 9 reports 0 as value and default,
    which is outside the config's 1 to 200 and disagrees with its default. Reading: 0 is probably a "no limit / off"
    position the Z 9 uses as its default; **that is a guess, nothing read says what 0 means or whether the bodies treat it
    alike.** Copying by number would set the Z6III to 0; harmless if 0 means off, a possible burst blocker if it means zero
    frames. Settling it needs the two bodies' continuous-shooting-limit menu entries read (no write).
  - **What the user read at the cameras (2026-10-28).** It is Custom Setting d2, "maximum shots per burst": **infinity on
    the Z 9, 200 on the Z6III, which can also be set to infinity.** I took that to mean index 0 is "infinity" on a shared
    scale and withdrew the concern. **That was wrong, and the check refuted it:** the user then set the Z6III to infinity
    and I read it twice read-only: `value_index` **1** (`default_index` still 200), against the Z 9 at infinity reading
    **0**. Same menu setting, different index. So the concern **stands again**: a range copied by number may not be a
    faithful copy. Possible readings, none verified: the Z6III encodes infinity as index 1 (so index 0 on the Z6III is
    something else or invalid); the Z6III was at 1 shot, not infinity, when read (the camera display was not checked); or
    the two bodies order the options differently. Mapping it needs the user to set known values on the Z6III (for example
    2, 199, infinity) with a read after each. Unwritten on hardware.
  - **The series (user set d2 on the Z6III, each read read-only, 2026-10-28).** Z6III: **1 reads index 1, 17 reads 17,
    200 reads 200, infinity reads 1.** The user confirmed at the display that it was infinity when it read 1. So on the
    Z6III the index follows the number, and **infinity is indistinguishable from 1 through the SDK range**. The Z 9 at
    infinity reads index 0 (its other settings were not measured). Consequences, each reasoned from these readings and
    **not tested**: (1) a transplant Z 9 to Z6III copies index 0, a position the Z6III has not been seen to report, so the
    result is unknown (rejected, clamped to 1 shot per burst, or infinity); (2) a **restore** of a Z6III snapshot taken at
    infinity writes back index 1, which would likely leave it at 1 shot per burst instead of infinity; (3) the field the
    camera honours on a range write is itself unknown (F18), so (2) may also be ignored. `ShootingLimit` is therefore
    not safely copyable or restorable on the Z6III as built. The Z6III was left at 17 by the series.
  - **The Z5 (read-only capture, serial 0000003021501, firmware 1.43; the capture gate was `no_baseline` /
    `unknown_model`, 165 properties).** `ShootingLimit` is a **1 to 100 range in 100 steps, `value_index` 99,
    `default_index` 99**; the user confirmed the Z5's d2 shows **100**, so index 99 is 100 shots (index = number minus 1
    on this body) and the default is the top of the scale. The Z5 has no 0 position and no infinity. Three bodies, three
    scales (Z5 1 to 100; Z6III 0 to 200 with index = number; Z 9 0 to 200 with infinity at 0). Consequence, reasoned and
    **not tested**: a Z 9 at infinity (index 0, number 0) transplanted onto the Z5 is clamped to 1 and would leave the
    Z5 at 1 shot per burst with no skip or warning in the plan.
- **Waiting on the user's naming (the opt-in rule):** the plan holds five `literal_unconfirmed` skips
  (`ElectronicVR`, `MovieRecMicrophone`, `MovieWBAutoType`, `MovieWindNoiseReduction`, `WBAutoType`) and seven
  `raw_code_unconfirmed` skips (`AEBracketingStep`, `AEBracketingType`, `AFSubjectDetection`, `ImageColorSpace`,
  `MeteringMode`, `MovieFileType`, `MovieScreenSize`). An opted-in write of one of each is still the untested part of
  the strategy list.
- Harness notes: the restore-point cap (50) also stopped my second round before any write until I moved the scratch
  restore points aside by hand, and my script first read the wrong key (`verification`; an apply reports `readback`,
  a restore reports `verification`) and stopped after a write that had landed, which I undid at once from the restore
  point. No product defect in either; the second is one reason the scripts now stop at anything unexpected.
- **Still not run:** Claude Desktop and other clients, an apply through Claude Code, a label that moves index between
  bodies, and an opted-in raw-code or literal write.

- **What was done about it (2026-10-28).** The user decided to translate only verified ranges. `ShootingLimit` is now on the
  never-copy table (transplant and restore), and ranges between two different models are default-deny: a range is copied
  only when it is on `range_policy::VERIFIED_RANGES` for both models, with a live scale check on top (plan §5.3a). The table
  came from the scan above (38 ranges). A read-only plan of a Z 9 snapshot onto the Z5, run with the new binary, planned 21
  range copies, 2 of which would change (`ISOAutoShutterTimeAutoValue` and `MovieRecMicrophoneValue`, the two settings that
  genuinely differ between the bodies), and no `range_unverified` or `range_scale_differs`. Not exercised on hardware: any
  range write across models, and the models nobody has captured (ZR, Z8, Z6II, Z30).

## F13. Still unverified

- Whether `available` / `connected_pid` can be obtained, and what they mean while another process
  holds a body.
- The teardown hang's remaining rate after the F24 pump: Z6III, write paths, and the worker.
- Whether the SDK worker behaves as assumed inside a child process / dedicated thread (the CLI has
  only ever run it on its main thread).
- A reset-initialised open while another process is mid-*write* (deliberately not tried; it could
  wedge a body).
- Restore fidelity beyond one enum property; restore after a power cycle; a range, string or
  boolean property round trip.
- Cross-model transplant beyond a single enum-by-label property: ranges, raw-code opt-ins,
  literals.
- The Z 9 B, and same-model pairs (the serial-pairing ambiguity `warn_if_ambiguous_pairing`
  mentions).
- Whether the judgement-call entries in the never-copy table are the right set, and whether
  `ShootNoCard` belongs in it.
- Ranges on models nobody has captured (ZR, Z8, Z6II, Z30): a range between two different models is refused
  (`range_unverified`) until both are on `range_policy::VERIFIED_RANGES`; the first table covers the Z 5, Z 9 and Z6_3 only,
  by scale, not by meaning (F25).
