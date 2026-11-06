# The extended section of a snapshot

A snapshot holds the MAID properties (`properties`) and, since 2026-11-06, an optional `extended` section: what a
body keeps that the MAID SDK does not expose. Code: `src/extended.rs` (model and reader), `src/extended_diff.rs`
(comparison), `src/ops/render_extended.rs` (display). The section is optional, so `format_version` is still 1: a
file without it loads as before, and a snapshot with none writes no `extended` key.

## What is read, and what happens to each item

| item | disposition |
|---|---|
| `0xD072` Artist, `0xD073` Copyright, `0xD090` Image comment | extracted: text (`GetDevicePropValue`, 0x1015). A value that is not a valid PTP string keeps its bytes (`undecodable`) |
| `0xD053` EnableCopyright, `0xD060` IPTCPresetSelect | extracted: the bytes, hex. No meaning is claimed for the values |
| each stored FTP profile, `0x90ED(n)` for `n < count` | raw-only: the whole blob, hex. The fields are decoded when shown (`decode_ftp_profile`), so the password exists in one place |
| the FTP list header (`0x90EB`: count, limit, active slot, device name) | extracted |
| FTP profiles past 32 | dropped, with `truncated: true`: the camera reports a limit of 20, a larger count is not trusted |
| the blob's timestamps, structural tag bytes and IPv6 placeholders | in the stored blob; not in the viewer's table (nothing a person sets) |
| IPTC fields (`0x9412` read, `0x9413` write) | **not captured yet**: see below |

Each property is recorded in exactly one state: `text`, `bytes`, `undecodable`, `not_supported` (the body answered
`DevicePropNotSupported`) or `read_failed` (with `PtpUsbError::tag()` and the detail). FTP is `read`, `not_on_model`
or `read_failed`. A raw session that will not open records every item as `read_failed` with the same cause.

## How a body is asked

DeviceInfo cannot say what a body has. Measured on a Z 9 (fw 5.31, 2026-11-06): it lists 21 device properties, none
of the five `0xD0xx` ones, and its operation list has none of `0x90E8`..`0x90EF` (it does list `0x9412` and
`0x9413`), yet it reads all five properties and serves four FTP profiles. So:

- the properties are asked for on every model; a body that lacks one answers `DevicePropNotSupported`;
- FTP is skipped for the models in `FTP_ABSENT_MODELS` (the Z 5: owner-confirmed, and its `0x90E8` setup times out
  after 5 s) and asked for on every other model, including ones this code has not seen.

(An earlier note here called the Z 5's missing FTP ops proof that it has no FTP profiles. The Z 9 lacks them in the
list too, so that was not evidence; the owner's statement and the timeout are.)

## Capture order

The raw-PTP session and an open SDK session cannot share the USB interface, so the extended read comes first and its
session is released before the SDK opens. In the CLI (`snapshot`, `--no-extended` skips it) it runs before
`open_sdk`; in the MCP worker (`capture`) it runs inside `with_body_after`, after the serial, bundle and schema are
validated and before the SDK opens. A failure never fails the capture. The section is attached only if it was read
from the body being captured (model, and USB serial when both sides have one); otherwise it is dropped and the
result's `extended.dropped` says why.

Verified on a Z 9 (z9a): CLI and worker captures both read 196 properties (11 unreadable, the baseline's number) with
5 properties and 4 FTP profiles, and the two snapshots diff as identical. The worker uses no USB reset; the raw
session before it did not disturb the SDK on this body.

## Secrets

Passwords and the Wi-Fi key field are stored in the clear (the point is to be able to restore them) and shown masked:

- `show`: asterisks; `--reveal` shows them. `show --json` omits them unless `--reveal`.
- the GUI viewer: masked, with a "Show passwords" toggle per window (off whenever a window opens).
- the MCP `get_snapshot`: always masked.
- `diff` and `check`: a changed secret is reported as changed, with no value on either side.
- `Debug` and `serde` cannot leak one: `render_extended::Secret` has no `Serialize` and prints its length only.

## Comparison

`diff` compares the section: properties by name, FTP profiles by identical blob first (a profile that only moved slot
is not a change), then by name (field-level changes), the rest as added or removed, and the active profile. A snapshot
with no section against one with a section is a single entry. `check` leaves the section out (`skip_extended`): its
live capture is MAID only, so a reference's section would otherwise show as a difference every time.

## Not done

- **Restore and transplant do not write the section.** The write paths exist separately (`vendor-set-string`,
  `vendor-write-ftp-blob`, both verified by read-back), but rebuilding a stored FTP list needs a design: a write lands
  at the front and shifts the rest, `0x90EA` deletes everything, and the Z 5 has none of it.
- **IPTC fields.** `0x9412` works over USB on a Z 9. `0x9412(0)` returned 732 bytes: a u32 count (13) and then 13
  fixed-size records, each `c9 00` and a u32 kind (`1` for the first ten, `2` for the last three) followed by zeros.
  `0x9412(1)` returned 72 bytes: `01 00 00 00 c9 00 01 00 00 00` and zeros. That body held no IPTC text, so the field
  layout is not decoded. Needs one slot with known text (every field labelled with its own name, as in the July
  Z6III capture). `vendor-read-iptc --slot N` prints the bytes.
- **IPTC write experiments on the Z 9 (2026-11-06; all empty-slot, baseline restored byte for byte).**
  `vendor-write-iptc` writes a slot; `vendor-set-iptc-preset` selects `0xD060`.
  - Slots 1..10 read as 72 bytes (`01 00 00 00 c9 00 01 00 00 00` then zeros); slots 11..13 read as 20 bytes
    (`01 00 00 00 c9 00 02 00 00 00` then zeros): two record types, not one layout.
  - Writing the 72 bytes just read: accepted, unchanged. Writing the Z6III-capture layout (two leading entries, `""`
    and `"1"`, then 14 length-prefixed names): refused, `0xA210`. Writing the 72-byte header plus 14 length-prefixed
    names: accepted, **ignored** (slot still empty).
  - `0xD060` set to 1: `AccessDenied` (0x200F), though its descriptor says read-write, 0..13. Probably the camera
    only selects or accepts a registered preset, and all thirteen here are empty.
  - So the write layout is still unknown. The way forward is ground truth: one preset typed in at the camera, then read
    with `vendor-read-iptc`, then a modified copy written back.
- **Other bodies.** Only a Z 9 has been captured with this section. Z6III (expect 5 properties, FTP) and Z 5 (expect
  `not_supported` for the IPTC preset at least, FTP `not_on_model`) still need a run each.
