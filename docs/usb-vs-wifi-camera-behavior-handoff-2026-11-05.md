# USB vs WiFi: is the camera the same device on both? (handoff, 2026-11-05)

Written at the end of a session on the Mac Studio so the work can continue from the MacBook. Nothing here has
been run on a camera yet: it is a desk survey of what the repo already knows, plus a list of cheap experiments.
The survey was done by research agents reading the repo; the line references below were spot-checked for the
`0xD0B4` discrepancy and the `ptp_usb.rs` module comments only. Re-check a citation before leaning on it.

## The question

After the 2026-11-04 Z6III NX Field session (docs/nx-field-session-2026-07-09.md, section "Z6III over NX Field"),
the question was: how similar is the **camera's own behavior** when driven over USB versus over WiFi (PTP/IP, as
NX Field does it)? Same properties accepted or refused? Same response codes? Same mode-dependence and side effects?
(This is *not* a question about our code or API vocabularies; see "Code side" for the short answer to that.)

## Short answer

**Probably the same device on both, with real (if indirect) evidence.** The first desk pass concluded the repo had no
like-for-like data. That was wrong: the gitignored July USB `GetDevicePropDesc` baseline for the Z6III
(`references/z6iii-ptp-baseline-2026-07-09.txt`) lists the writable flag and the allowed values for the same
properties NX Field touched in November, and it agrees with every WiFi outcome I compared (section A2). Caveats: the
two were taken months apart in different camera states, and GetSet is state-dependent. It is not a paired experiment.
What else exists is vendor-op replay on a Z6III (A), parallel property sets (B), and a few contradictions (C).

Abbreviations: **NXS** = docs/nx-field-session-2026-07-09.md, **HF** = docs/mcp-hardware-findings.md,
**PU** = src/ptp_usb.rs.

## Code side (for context)

- USB settings reads/writes go through Nikon's **MAID Remote SDK** (`src/sdk.rs`; `ops/port.rs`), keyed by SDK
  capability id (e.g. `Aperture` 0x8113), many of them index-based enums. A Z6III snapshot has 200 capabilities.
- **Raw PTP over USB** is a separate, narrow module (`src/ptp_usb.rs`, "completely separate path from `sdk.rs`'s
  MAID FFI bridge"). It implements `0x1001` GetDeviceInfo, `0x943B` vendor-property read (param `0x10000|propcode`,
  raw hex out, no decoding; CLI `vendor-read <code>`) and `0x90E8`/`0x90EE` FTP-profile write. There is **no**
  `0x1015`/`0x1016` (Get/SetDevicePropValue) and no `0x9413` write.
- **No PTP/IP client exists.** `Transport::Ptpip` (snapshot.rs) is an enum variant only. The WiFi work is docs,
  captures and `scripts/rpi-ap-setup.sh`.
- No repo code maps MAID ids to PTP property codes. Likely candidates (name-based, not verified):
  Aperture 0x5007↔`Aperture`, ExposureComp 0x5010↔`ExposureComp`, Metering 0x500B↔`MeteringMode`,
  ExposureProgram 0x500E↔`ExposureMode`, ISO 0xD0B4↔`Sensitivity`, Shutter 0xD100↔`ShutterSpeed`.
  `sdk-runtime/DC_PTP_Config.config` (a Nikon file; the repo never reads it) has a `<model:Z6_3>` block listing these
  PTP codes in decimal with value→enum-index tables, which is probably the SDK's own translation layer. Not traced.
- Write verification (write, re-capture, diff → `applied_verified`/`applied_unverified`) exists only on the MAID path
  (`ops/apply.rs`, `ops/readback.rs`). The raw-PTP FTP write has no read-back.

## (A) Seen on both transports

- **Vendor ops, Z6III.** `0x943B`, `0x9413`, `0x90E8`, `0x90EE` were captured over WiFi from NX Field, then replayed
  over raw USB PTP and returned `0x2001`. The earlier "WiFi-gated" belief came from our wire-format bugs, not the
  camera (NXS "USB-transport test").
- **FTP profile write on USB** took effect on the body and was reverted; visually confirmed (NXS).
- **`0x200A` Property Not Supported** is returned on both transports, for different properties: USB `0x943B` read of
  `0xD0B4` on a Z6III; WiFi writes of `0xD07F` and `0xD1F0` on the Z6III.
- **Z9 vs Z6III over WiFi:** `0xD07F`/`0xD1F0` accepted on the Z9 (July), refused `0x200A` on the Z6III (November).
  The `0xD07F` result agrees with the USB-derived "Z9-only" list (todo.md, 2026-07-09 baseline pass).

## (A2) Same body, USB descriptors (July) vs WiFi outcomes (November)

Source: `references/z6iii-ptp-baseline-2026-07-09.txt`, a `gphoto2` dump of `GetDevicePropDesc` for the Z6III over
USB. **`references/` is gitignored, so this file is not in the remote**: copy it from the Mac Studio or regenerate it
(it also carries the body's serial number; the Z8 file carries a third party's name, so do not commit any of them).
State at capture: exposure program A, F-number reads f/0 (so probably no lens), release mode CH. `ro`/`rw` below is
the descriptor's GetSet flag.

| Property | USB descriptor (July) | WiFi outcome (Nov) | Agree? |
|---|---|---|---|
| `0x500E` exposure program | ro, u16, [1,2,3,4,0x8010,0x8050,0x8051,0x8052] | write → `0x200F` (3) | yes |
| `0xD100` shutter | ro, u32, [65537] (in A mode) | write refused `0x200F` in A (11/11), accepted in M (3/3) | yes for A; M descriptor not captured |
| `0xD1A6` (LiveViewSelector) | ro, u8, [0,1] | write → `0x200F` (2) | yes |
| `0x5013` release mode | **rw**, u16, [1,2,0x8010,0x8011,0x8019,0x810F,0x811E,0x813C,0x8178] | values seen = exactly this list; NX Field never wrote it | list matches; see note 1 |
| `0x500B` metering | rw, u16, [2,3,4,0x8010] | all four set OK | yes, identical list |
| `0xD148` secondary slot | rw, u8, [0,1,2,4] | 0,1,2,4 set OK; 3 "not seen" | yes, identical list |
| `0xD068` CL speed | rw, u8, [1..7] | 7 and 5 set OK | yes |
| `0xD0B4` ISO | rw, u32, [50 … 204800] | 2000 and 100 set OK | yes |
| `0xD060` IPTC profile | rw, u8, range 0–13 (libgphoto2: "[Unknown Property]") | set to 1 OK | yes |
| `0xD053/54/72/73/90/91` | all rw | all OK | yes |
| `0x5010` exp. comp | rw, i16, ±5000 in 1/3 steps | −2000, +1666, 0 OK | yes |
| `0x5007` aperture | **ro**, [0] (f/0) | writes OK | no — see note 2 |
| `0xD07F`, `0xD1F0` | absent from the Z6III list | write → `0x200A` | yes |

Notes:
1. NXS and this doc said release mode is "read-only from the phone". That is an NX Field limitation (it has no
   control and sent nothing when tried). The camera's descriptor says writable; whether the camera would accept a
   write is untested.
2. `0x5007` is the one mismatch: ro with a single value in July, written successfully in November. The likely
   explanation is camera state (no lens, or A mode, in July), which would also show GetSet is state-dependent.
   Not tested.
3. `0xD025` (libgphoto2 name "White Balance Preset Value 0") is in the Z6III's vendor property list but its descriptor
   query returns `0x200A` in the baseline, while NX Field read and wrote it through `0x943B`/`0x943C` as the
   pre-release burst setting. So some vendor properties are only reachable via `0x943B`/`0x943C`, not
   `GetDevicePropDesc`. Also: `0x1D025` is simply `0x10000 | 0xD025`, the **same parameter form `ptp_usb.rs` already
   uses** for `0x943B`. The NXS note "`0x1D000` plus a low byte" and the discrepancy listed under C below are artefacts
   of reading it that way; the pre-release burst is property `0xD025`, and `0x1D03D` is `0xD03D` (a rw u8 in the
   baseline).

## (B) One transport only

**WiFi only (Z6III), no USB counterpart:**
- `0x500E` exposure program write → `0x200F` Access Denied (3 of 3). `0xD1A6` → `0x200F` (2).
- `0xD100` shutter write: accepted in manual (3 of 3), `0x200F` in aperture priority (11 of 11). The mode at each
  write is **inferred** from dial events and the session log, not read from the camera.
- `0x5013` release mode read-only from the phone; changed on the body.
- `0x4006` events follow writes; `0x400C` plus a burst of change events around extended release modes.
- Backup (`0xD148`=1) write answered OK with no second card inserted; whether it applied is unchecked.
- Needs remote mode ("Connect to other cameras"), port 15740 otherwise closed. ~10 s probe (`0x0d`/`0x0e`); camera
  closes after one missed probe.

**USB/MAID only (Z 9 and Z6III), HF F4–F25:**
- ~100 writes of obscure settings, no side effects seen. These do not overlap the WiFi-tested set.
- 76 "read-only" properties come from SDK capability bits. Whether that equals PTP's GetSet flag: not determinable.
- Real Z6III write rejections: `LiveViewStatus`, `CurrentItemID` fail with -125 (`src/transplant.rs`). The mapping of
  MAID error codes (-125, -126, -117, -106) to PTP response codes is not determinable.
- A blind write to a stepped-range property was accepted but ignored; reading the range first made it work (HF F18
  addendum). Same "OK is not proof" pattern as the WiFi Backup write. Whether the SDK or the camera dropped it: unknown.
- Reads: Z 9 reads 196 and fails 11; Z6III reads 200 and fails 12. Option lists are state-dependent.
- Session behavior: leaving a session open does not wedge either body (60 reopens); a body can vanish from SDK
  enumeration and need a replug; raw PTP `OpenSession` gives `0x201E` if a session was left open, `clear_halt` clears
  a stalled pipe. Body must be in PTP/MTP mode, not USB LAN.
- A second `0x90EE` in one USB session hangs unless `0x90E8` is sent first (n=1; the WiFi capture only shows first
  writes, so the other transport is untested).

**Z 5:** USB only, never on WiFi. GetDeviceInfo lists 89 operations vs 146 on the Z9/Z6III.

## (C) Contradictions and open anomalies

1. **`0xD0B4` is not Z9-only; the cause of the USB `0x200A` is still open.** NXS ("USB-transport test") explains
   the USB `0x200A` as "Z9-only per the todo.md per-body table". That is wrong: todo.md lists only `d0b0`–`d0b3` as
   Z9-only, NX Field wrote `0xD0B4` on the Z6III over WiFi, the SDK config lists it for `Z6_3`, and the July USB
   baseline shows it as `ExposureIndexEx (d0b4 rw u32)` with the full ISO list on the Z6III itself. So the property
   exists and is writable, and the `0x200A` came from the `0x943B` read of it. The param form is not the culprit
   (see A2 note 3). Remaining candidates: `0x943B` serves only some vendor properties, or the camera's state at the
   time. Experiment 1 settles it.
2. **`0xD1F0`:** listed Z5-only in the USB property list, yet a Z9 accepted a WiFi write to it (July). USB enumeration
   may not show everything settable over WiFi.
3. **Events:** the July Z9 notes say `0x4006` goes silent after connect; the November Z6III notes say it follows
   writes. Body difference or an over-broad earlier claim. USB event delivery: not examined.
4. **Z 5 vendor reads:** two comments in `ptp_usb.rs` disagree on whether they work (one says they do; another
   mentions an OperationNotSupported reply).
5. **State-dependent `0x200A` on the Z9:** (todo.md 2026-07-09) some codes read fine the first time and returned
   `0x200A` the second time, same camera state. Possibly a libgphoto2 artifact, possibly real.
6. NX Field sends each write twice, ~0.1 s apart. Whether USB needs the same: unknown.
7. Whether USB and remote-mode WiFi can be up at once: untested.
8. Value encodings have not been compared side by side. MAID uses SDK enum indices; PTP uses spec values (e.g.
   `0x500E`: 1=M). Inference only: the two spaces differ.

## gphoto2 and where it fits

`gphoto2` 2.5.32 is installed on the Mac Studio (`/opt/homebrew/bin/gphoto2`; check the MacBook). The repo used it
for the 2026-07-09 baseline (todo.md): it issues `0x1001`, `0x90CA`, and loops `GetDevicePropDesc` `0x1014`, and
names hundreds of `0xD0xx` codes. `ptp_usb.rs` copies its wire framing. The repo never calls it at runtime.

**Source-checked 2026-11-05** (libgphoto2 `master`, `camlibs/ptp2/{ptp.h,config.c,ptpip.c}`; read, not run). Both
libgphoto2 (LGPL) and the CLI (GPL) are open source, so any claim below can be checked in the source:
- `ptp.h` independently names our WiFi-decoded properties with the same codes: `0xD0B4` ExposureIndexEx,
  `0xD100` ExposureTime ("Shutter Speed"), `0xD148` Slot2SaveMode, `0xD068` D1ShootingSpeed ("continuous speed
  low"), `0xD060` IPTCPresetSelect, `0xD053` EnableCopyright, `0xD072` ArtistName, `0xD090` ImageCommentString.
  This corroborates the property identification in the NX Field doc from a second, independent source.
- **Coverage limit, checked** (`library.c` ~L646–680): after `GetDeviceInfo`, if the camera lists Nikon op `0x90CA`
  (GetVendorPropCodes) among its operations, libgphoto2 calls it and **appends** the result to `DeviceProps`; it does
  the same for `GetVendorCodes`. There is no de-duplication, so any code in both lists appears twice. That, not
  only the disabled suppression block, accounts for todo.md's "listed twice" anomaly (the later `0x200A` on the Z9's
  second pass is still unexplained). A property in neither list is unreachable by name, and the listing loop silently
  skips (`continue`) any property whose `GetDevicePropDesc` fails. Checked against real data: of 20 properties NX
  Field touched on the Z6III, 18 are in the July baseline; the two absent (`0xD07F`, `0xD1F0`) are the ones the
  camera refused over WiFi with `0x200A`. So in practice the limit cost us nothing for this set. (A caveat from
  `library.c` comments: GetVendorPropCodes is known to crash some Nikon 1 bodies, which is why libgphoto2 gates it.)
- `config.c` has a generic "Other PTP Device Properties" fallback (`/main/other/<hex>`): every property listed in the
  camera's DeviceInfo gets a widget via `GetDevicePropDesc`, typed from the descriptor's form (none/range/enum).
  Unnamed ones are labelled "PTP Property 0xNNNN". The widget is marked read-only when the descriptor's GetSet is
  Get (`config.c`, end of the fallback loop). The loop only covers properties listed in the camera's DeviceInfo
  `DeviceProps` (Nikon's vendor list is evidently merged in; not checked), and the "suppress already-decoded
  properties" block is `#if 0`, so named properties also show up a second time here. That plausibly explains
  todo.md's "codes listed twice" anomaly; it does **not** explain the later `0x200A` on the second pass.
- Write side (`config.c`, "Generic property setter"): by hex name, `--set-config-value d0xx=<n>` (or `--set-config`).
  It re-reads `GetDevicePropDesc` first and **refuses client-side** ("currently read-only") unless GetSet is
  writable, so the camera never sees the write. Then it sends a standard `SetDevicePropValue` (`0x1016`) with the
  descriptor's data type (ints parsed with `%ld`, strings as-is) and reports a non-OK response code with its number.
  Consequence for experiment 3: gphoto2 cannot reproduce the `0x200F` refusal if the descriptor already says
  read-only; use a raw `0x1016` path in `ptp_usb.rs` for that. Ranges that are small, step-1 integers are shown as
  menus. The code has a TODO noting the set path ignores some return values.
- Tracing: `usb.c` logs each PTP operation (code, name, params), data phase and response at debug level, and
  `ptp_debug` dumps data bytes. The installed CLI has `--debug`, `--debug-loglevel=[error|debug|data|all]` and
  `--debug-logfile=FILE`. Events on the interrupt endpoint were not checked.
- `ptpip.c` implements PTP/IP: address `ptpip:IP[:port[:eventport]]`, default port 15740, and the init command sends a
  16-byte GUID. The GUID is generated randomly once and stored in gphoto2's settings (`ptp2_ip`/`guid`), with a
  comment that Nikon WU-1 adapters may use a fixed GUID. So the handshake is a plain init-with-GUID; whether a
  Z6III in remote mode accepts a stranger's GUID or wants pairing is still **untested**.

What it gives that MAID doesn't (the rest is general knowledge of libgphoto2, **not verified in this repo**; check
against the installed version):
- `GetDevicePropDesc` for any property: type, GetSet (writable) flag, default, current, range/enum.
- Properties with no MAID capability (artist, copyright, IPTC, the `0xD0xx` codes), by hex code; unnamed ones appear
  under their hex code and probably settable when writable.
- `--debug --debug-logfile=…` logs every PTP operation, response code and event: a free USB-side protocol trace.
- A PTP/IP port driver (`--port ptpip:<ip>`). **Untested whether a Z6III in remote mode accepts it**; NX Field may do
  pairing/handshake that gphoto2 doesn't. If it connects, identical code can read the same property over both
  transports, which is the closest thing to a like-for-like comparison.

**What gphoto2 reaches that MAID doesn't, and what neither does** (source-checked 2026-11-05 against libgphoto2
`ptp.h`/`config.c`/`ptp.c`, plus the July Z6III baseline; nothing run against a camera today):
- **Artist / copyright: yes over USB.** `config.c` has `artist` (`0xD072`) and `copyright` (`0xD073`) settings for
  Nikon, both using the string setter `_put_STR`. The baseline lists Artist Name, Copyright Information, Image
  Comment (`0xD090`) and the flags `0xD053`/`0xD091` as rw, and todo.md records the fields working on all four
  bodies. `0xD053` has no named setting but is reachable as `other/d053`. MAID has no capability for any of these.
  Not checked: what `_put_STR` does with the fixed-length, space-padded fields (36/54 chars). Write a value, read it
  back, and check before relying on it.
- **FTP profile: no, and no vendor has working support in libgphoto2.** (An earlier version of this note said Canon
  and Sony were supported; that was wrong.) Canon's `EOS_SetWftProfile`/`GetWftProfile`/`SetWFTData`/etc. (`0x9122`,
  `0x9123`, `0x9187`, …) and Sony's `FTPSetting*` properties appear only as `#define`s in `ptp.h` and as name
  strings in the opcode/property name tables in `ptp.c`, used for debug logs. Nothing in `config.c`, `library.c` or
  `usb.c` calls them. Nikon `0x90E8`/`0x90EE` are not even named (the only repo hit for `0x90e8` is a camera-notes
  file for a D100). Why upstream stops at names is not recorded: searching the project's GitHub issues for Nikon
  FTP profiles, WftProfile and Wi-Fi settings returned nothing. Reasonable inference only: its scope is tethering and
  file transfer, not configuring a camera's network-upload profile. `ptp_usb.rs` is the only working write path for
  Nikon, and the profile still has no read-back.
- **IPTC contents: no.** Only the preset selector `0xD060` is named (baseline: rw, range 0–13). `0x9412`/`0x9413`
  are absent; reading or writing the IPTC fields remains `ptp_usb.rs` work.
- **`0x943B` / `0x943C`: no.** Neither op is in libgphoto2's op list. gphoto2 reaches properties through standard
  `GetDevicePropDesc`/`Get`/`SetDevicePropValue`, so properties only served via the vendor ops (e.g. `0xD025`, the
  pre-release burst; see A2 note 3) are out of its reach.

What MAID gives that gphoto2 doesn't: labeled enums/ranges/steps, capability dependencies, snapshot/restore/verify,
and (probably) Nikon's vendor ops like `0x943B`/`0x9413` as named features.

## Experiments, cheapest first

Items 1, 2, 4 and 5 are read-only. Item 3 writes a property (a no-op write of its current value).

1. **`0xD0B4` on the Z6III.** Over USB: `vendor-read` of `0xD0B4`, then `0xD053` as a control (known-good), then a
   plain `GetDevicePropValue` on `0xD0B4`. Repeat in two body states (idle, then in playback/menu). Settles C1.
2. **GetSet flag over USB.** Partly answered by the July baseline (A2): the flag matched every WiFi refusal. What is
   still worth doing is the *state* test: `gphoto2 --list-all-config` with the dial in M and then A, with a lens on,
   and look at `d100` and `5007` (expect `d100` rw in M; explains the `5007` mismatch). A fresh dump also gives a
   same-day pair with a WiFi write if one is done in the same session.
3. **Mode dependence over USB.** Write the current `0xD100` value back in M and in A; compare `0x200F` with the WiFi
   result. Needs a small `0x1016` path in `ptp_usb.rs` (does not exist yet).
4. **Events over USB.** Log the interrupt endpoint while changing release mode and the exposure dial on the body;
   look for `0x4006`/`0x400C`.
5. **Same-instant comparison.** MAID snapshot plus USB PTP reads of `0x5007`, `0x5010`, `0x500B`, `0x500E`, `0x5013`,
   `0xD0B4`, `0xD100` with the body in a known state.
6. **gphoto2 over PTP/IP.** Z6III in remote mode on the Pi network; `gphoto2 --port ptpip:<ip> --list-all-config`
   against the USB output. Do a plain USB run first.
7. **Z 5:** retry a `0x943B` read; settle C4.
8. Existing backlog (NXS "Open items"): still↔video switch side-effect check; `GetDevicePropDesc` on
   `0xD25B, 0xD038, 0xD039, 0xD03A`.

## Practical notes for the MacBook

- Experiments 2–5 need a camera attached by USB, in PTP/MTP mode (not USB LAN). Quit anything that grabs the camera.
- Experiment 6 needs the Raspberry Pi AP from `docs/rpi-ap-capture-setup.md` and the camera in "Connect to other
  cameras" → remote mode. Set the phone's Auto-Lock to Never if NX Field is also in use.
- Local `master` was fast-forwarded to `origin/master` (`ce5d57a`) at the start of this session; that is the base
  for this note.
