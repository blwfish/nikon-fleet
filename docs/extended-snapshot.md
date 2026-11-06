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
| each IPTC slot, `0x9412(n)` for `n` in 1..=count (the directory `0x9412(0)` gives the count) | raw-only: the whole record, hex; decoded when shown by `src/iptc.rs` (name + 14 fields). Slots of another record kind are kept and shown as such |

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

## Restore

`restore-extended <snapshot> [--dry-run] [--only strings|iptc|switches|ftp] [--serial S] [--expect-digest D
--expect-summary T] [--json]`, in `src/ops/restore_extended.rs`. It is its own operation, raw USB only (no SDK): the
MAID `restore` does not touch the extended section. One raw session reads the body, plans and writes. The shape is the
MAID restore's: a pure plan with a digest and a summary the caller echoes back, a restore point before the first write,
and a read of the body afterwards to check what was written.

| category | write | read-back |
|---|---|---|
| `strings` (Artist, Copyright, Image comment) | `SetDevicePropValue` | the value, by the write path |
| `iptc` (each preset slot) | `0x9413` | the slot read again, byte for byte |
| `switches` (EnableCopyright, IPTC preset) | `SetDevicePropValue`, one byte | the value |
| `ftp` (the whole list) | `0x90EA` delete-all, then `0x90EE` adds last-first (an add lands at the front) | each add read back; the list read again at the end |

Order: strings, IPTC, switches, FTP (the preset switch comes after the presets). The plan is one line per item: `same`,
`WRITE old -> new` (never a password), `skip` with a closed reason, or `not selected`. The digest covers every item's
action and the hash of what would be written, so a body that changed under a reviewed plan is refused (exit 5), and a
plan whose overwritten value changed is stale even when the count of changes is the same. Apply writes the restore point
first, as a snapshot with no MAID properties and the extended section as it was (label `restore-ext`); restoring from
it undoes the restore. Status: verified only if every write's read-back matched and the body, read again, matches the
snapshot for every item written; indeterminate (exit 4) if any write's data may have gone out and the reply failed.

Skips (each a typed reason, never silent): the snapshot has no value (an older snapshot, a body without it); the body
lacks it; the body's current value was unreadable; a value the camera cannot hold (text outside Latin-1); a model the
write is not confirmed on; an IPTC slot of another record kind; a snapshot slot the body lacks; a truncated or
undecodable FTP list; a list longer than the body's limit. Not possible: choosing the active FTP profile (no op that
selects one was found).

**Verified on hardware (2026-11-06):** strings and the `EnableCopyright` switch on a Z 5, through the whole sequence:
dry run, apply with the echoed digest, restore point, undo from the restore point, restore again, final dry run `0 items
would change`. Write paths verified earlier: strings on Z 9 / Z6III / Z 5, IPTC slot writes on Z 9 and Z6III (read back
byte for byte), FTP adds and deletes on Z 9 and Z6III.
**Not yet run on hardware through `restore-extended`:** the IPTC slots and the FTP rebuild (delete-all then reversed
adds, and that the list ends in the snapshot's order), and `EnableCopyright` on a Z 9 or Z6III (the gate allows only
the Z 5 until then). The `0xD060` preset switch is allowed on a Z6III only (a Z 9 refused it while its presets were
empty). Surfaces: the CLI only; the MCP and the GUI do not offer it yet.

## Not done

- **IPTC write is understood and used by the restore.** Layout (Z 9 and Z6III, confirmed by writing a preset to an
  empty slot and reading back the same bytes): a 12-byte header `01 00 00 00 c9 00 01 00 00 00 00 00`, then 15 entries
  `[u32 length incl. NUL][UTF-8][NUL]` (empty: a zero length), the first the preset's name, then Caption, Event ID,
  Headline, Object name, City, State, Country, Category, Supplemental categories, Byline, Byline title, Writer/editor,
  Credit, Source. An empty preset is 72 bytes. Slots 1..10 are presets; 11..13 are a 20-byte record of kind 2 whose
  meaning is unknown. The Z6III capture notes' "two leading entries, `""` and `1`" are this header's last zero u32 and
  the preset's name `1`; omitting the 8 header bytes before them is what a Z 9 refused with `0xA210`.
- **IPTC write experiments on the Z 9 (2026-11-06; empty slot, baseline restored byte for byte).** Writing the 72 bytes
  just read: accepted, unchanged. A 16-byte header (one u32 too many): accepted and ignored. The 12-byte header: stored.
  Selecting a preset (`0xD060`) was `AccessDenied` while every preset was empty; not retried with a preset in use.
- **Other bodies.** A Z 9, a Z6III and a Z 5 have been captured with this section (CLI and worker identical on each).
  A second Z 9 would show whether the section differs between bodies of one model.
- **MCP and GUI surfaces for the restore.**
