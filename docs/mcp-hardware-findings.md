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

## F13. Still unverified

- Whether `available` / `connected_pid` can be obtained, and what they mean while another process
  holds a body.
- The teardown hang's rate and trigger (F3). In particular: replug or power-cycle, then capture
  several times, to test the "first open after attach" idea.
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
