# Verifying a camera model's ranges (instructions for an AI assistant)

You are an assistant helping a person check one Nikon body so its numeric settings ("ranges") can be
copied to and from it safely. This file is the procedure. Read all of it before you start, do it in
order, and report what you found. **Everything here is read-only for the camera**, apart from the
person changing a few menu settings by hand on the camera and putting them back.

Why this exists: a range is a position on a scale, and the same position need not mean the same thing
on another body. Max shots per burst (`ShootingLimit`) has 0 to 200 on a Z 9 and a Z6III, yet the Z 9
reads "infinity" as position 0 and the Z6III reads it as position 1, where it cannot be told from 1 shot
(`docs/mcp-hardware-findings.md`, F25). So the tool copies a range between two different models only
when both are on a verified list (`src/range_policy.rs`, `docs/mcp-plan.md` §5.3a). The list covers the
Z 5, Z 9 and Z6_3. You are adding another model's evidence. The models the maintainer expects are the
**ZR, Z8, Z6II and Z30**.

For a new model, "verified" means the range's bounds, step count and default position read identical on
each listed model **and** the person confirmed at the camera that the positions mean what the menu says.
The script checks the first; only the person can give you the second (step 5). Be aware that the entries
already on the list (the first 36) were added on the first condition only: the bounds matched, and
`ShootingLimit` was the one range anyone checked by hand (and it failed; it is not on the list).

## Rules

- **Never** run `transplant`, `restore`, `vendor-write-ftp`, or anything that writes to the camera.
  You only run `discover` and `snapshot` (and `diff`, which reads files).
- **Do not edit** `src/range_policy.rs`, `src/transplant.rs` or any `*.rs`. If something looks wrong,
  say so in the report. Adding the model to the list is the maintainer's reviewed commit, not yours.
- **Snapshots contain the camera's serial number.** Keep them on this machine, outside any git
  repository, and do not send or commit them. The *report* (step 3) has no serial and is what you share.
- One camera attached at a time. If two bodies are attached, stop and ask which to use.
- If a step does not work as written, **stop and report the exact output**. Do not work around it,
  do not pass `--allow-partial`, `--no-usb-reset` or similar flags unless this file says to.
- You may change nothing the person has not asked for. Before the person changes a menu setting in
  step 5, record its original value; at the end the camera must be back as it was.

## 0. What you need

- This repository, built: `cargo build --release` gives `target/release/nikon-fleet`
  (`nikon-fleet.exe` on Windows). See `AGENT-README.md`.
- The vendored Nikon SDK staged in `sdk-runtime/` (`scripts/setup-sdk-runtime.sh`, or `.ps1` on
  Windows). If `sdk-runtime/` is missing and you cannot stage it, stop and tell the person.
- Python 3.9 or newer for `scripts/range_scales.py`.
- The camera on, connected by USB, in a normal shooting state (a card in it is fine). The person
  should know where its menus are.

Choose a scratch directory **outside the repository** for snapshots, for example `~/nikon-verify`,
and use it as `--data-dir` below.

## 1. Identify the camera

```bash
target/release/nikon-fleet discover --json
```

Record, for the report: the model string exactly as printed, the firmware, and `known_model`. Do not
record the serial. `known_model: false` is expected for these models (the MCP server's table only
knows three); the command line still works.

Then read the raw USB descriptors from the operating system, because the maintainer needs them to add
the model to the registry (`docs/firmware-archive-spec.md` §2) and to the MCP server's known models,
which are only ever added from a real probe, never from a guess. Record the **USB product string**
(for example `NIKON DSC Z 9`), the **idProduct** and the **bcdDevice**, and check that the firmware
`discover` printed is the bcdDevice read as decimal digits (`0x0531` is firmware `5.31`):

- macOS: `system_profiler SPUSBDataType` (Product ID is the idProduct; Version is the bcdDevice)
- Linux: `lsusb -v -d 04b0:` (04b0 is Nikon's vendor id)
- Windows: Device Manager, the camera's Details tab, Hardware Ids

Leave out the serial number. If you cannot read these on this machine, say so and give the `discover`
fields only; do not infer them.

## 2. Capture

```bash
target/release/nikon-fleet snapshot --data-dir ~/nikon-verify --serial <SERIAL> --label verify-start
```

Expect a note that the model has no baseline, and a property count in the high hundreds or lower
(the Z5 gave 165, the Z 9 196, the Z6III 200). Anything refused, or a very small count, is a finding:
report the message verbatim and stop.

Open the snapshot JSON and read `camera.model` and `camera.firmware`. **Use that exact `model` string
everywhere below.** The SDK's schema spells models inconsistently (`Z6_3` has no space, `Z 6_2` has one),
so do not guess a spelling or tidy it.

## 3. Make the report and compare

```bash
python3 scripts/range_scales.py dump ~/nikon-verify/snapshots/<the snapshot file> --out docs/range-scales/<model with spaces replaced by _>.json
python3 scripts/range_scales.py compare docs/range-scales/*.json
```

The report has the model, firmware and every range's bounds, steps and positions, and no serial.
`compare` prints three groups for the models together:

- **SAME**: the new model has the same scale as the models already listed. These are candidates.
- **DIFFERENT**: the scales disagree. Not a candidate. Copy the output into your report; do not interpret it.
- **ONE MODEL ONLY**: a range only your model has (or only one model has). Name each one, and ask the person
  what it is in the camera's menus.

Also note ranges the other models have that this model lacks (compare the lists; absence is normal).

## 4. Read what the camera currently says, once

Ask the person to read these menu entries aloud now, before changing anything, and write down each
against its property. This also tells you whether the index follows the menu number:

| Property | What to read on the camera |
|---|---|
| `ShootingLimit` | Custom Setting d2, "Max. continuous release" / max shots per burst (a number, or infinity) |
| `ExposureComp` | the exposure compensation currently set |
| `WBTuneColorTempEx` | the colour temperature shown when white balance is set to K (kelvin) |
| `MovieRecMicrophoneValue` | movie microphone sensitivity |
| `ISOAutoShutterTimeAutoValue` | the Auto-ISO "minimum shutter speed: auto" bias setting (five positions) |

Skip any the person cannot find, and write "not found". Do not guess a menu item.

## 5. Check the meaning at the camera (the part only a person can do)

Identical bounds are not enough (that is the `ShootingLimit` lesson), so for each property below the
person sets the listed values **one at a time** on the camera, and after each one you capture and read
the position:

```bash
target/release/nikon-fleet snapshot --data-dir ~/nikon-verify --serial <SERIAL> --label verify-<property>-<value>
python3 scripts/range_scales.py dump <that snapshot> | python3 -c "import json,sys; r=json.load(sys.stdin)['ranges']['<Property>']; print(r['value_index'], r['lower'], r['upper'], r['steps'])"
```

The expected number for a position is `lower + value_index * (upper - lower) / (steps - 1)`. For each
value, write down the number the menu shows, the `value_index` you read, and the number the formula
gives. They should agree, and the special entries (infinity, off, auto) are the ones to watch.

| Property | Values to set (record every one) | What to look for |
|---|---|---|
| `ShootingLimit` | 1, 2, a middle value (such as 100), the maximum, and **infinity** | Where infinity lands. It is informational (a transplant never carries this property between bodies) but needed for the record. If the maximum is 200 and the scale is 0 to 200, say what position infinity takes and whether 1 and infinity read the same |
| `MovieRecMicrophoneValue` | lowest, lowest + 1, a middle value, highest | whether the position follows the number from the bottom, and whether the lowest entry (for example "auto") is a special case |
| `ISOAutoShutterTimeAutoValue` | each of the five positions, in order | five distinct positions; which menu entry sits at which position |
| `ExposureComp` | 0, +1.0 EV, -2.0 EV | position 15 at 0 EV and 1/3 EV per position (a 1/2 EV body will differ) |
| `WBTuneColorTempEx` | one kelvin value, such as 5000 K, and one other | position = (kelvin - lower) / 10 on a 2500 to 10000 K scale |

Then, only if the person is willing and the model has them, spot-check **one** white-balance fine-tune
axis (set one step from neutral on one axis) and one of the other ranges the compare listed as SAME.
Say which. Do not try to check all 36; the list above covers the families that could hide a special
value.

For anything the person cannot find, write "not checked". An unchecked property is not a failure,
but it stays off the list.

**Put every setting back** to its recorded original value, then capture once more
(`--label verify-end`) and run `nikon-fleet diff` between the first and last snapshots. The only
differences should be volatile ones (battery level, the level sensor angle). Anything else means a
setting was not restored: tell the person which.

## 6. Report

Write one markdown file, `docs/range-scales/<model>-report.md`, with no serial number and none of the
snapshot files attached. It must contain:

1. The model string exactly as in `camera.model`, the firmware, the date, the `discover` fields, and the
   USB product string, idProduct and bcdDevice from step 1 (no serial).
2. The full `compare` output.
3. The step 4 readings and the step 5 table: property, value set, menu number shown, `value_index`
   read, number the formula gives, agrees (yes / no / not checked).
4. Anything unexpected, in the person's words and yours, kept separate.
5. Confirmation that every changed setting was restored, and the first-versus-last `diff`.
6. A sentence saying which ranges you believe could go on the list and **why**, so the maintainer can
   disagree. Do not edit the list yourself.

Tell the person to send `docs/range-scales/<model>.json` and `<model>-report.md` to the maintainer.

## For the maintainer (not for the assistant)

To add a model once its report is in: put the report in `docs/range-scales/`, add it to `REPORTS` in
`src/range_policy.rs` (tests), add the model string to each `VERIFIED_RANGES` entry that compare shows as
SAME and the person confirmed (a new set constant per distinct group of models), and run `cargo test`.
The tests check the table against the reports in both directions, so a range that matches but is left
off, or listed but not backed by a report, fails, with one exception: a scale shared by two models whose
code is on `NEVER_TRANSPLANT` or `NEVER_TRANSLATED` is accepted (the table test asks `never_skip`). So a range that matched but failed the
meaning check (infinity landing somewhere unexpected, for example) cannot just be left off: put it on
`NEVER_TRANSLATED` if only carrying it between bodies is the trouble (a restore of the same body still
works), or on `NEVER_TRANSPLANT` if it is state or a read-out and should never be copied.

Making the MCP server capture the model is separate: add a row to `docs/firmware-archive-spec.md` §2 from
the probe in step 1, then add the stripped USB product string to `KNOWN_MODELS` in `src/ops/cameras.rs`
and update its pinned tests (`the_known_models_are_exactly_the_surveyed_spellings`, the schema-section
test, and the baseline test, which needs either a shipped baseline for the model or the model named as
an exception). The schema's section names (`ZR`, `Z 8`, `Z 6_2`, `Z 30`) are candidates for the spelling,
not evidence: the Z6III is `Z6_3` and the Z6II is `Z 6_2`, so the spelling must come from the probe. The
Z8 PTP baseline in `references/` says `Model: Z 8`, which is PTP data, not the USB product string.
