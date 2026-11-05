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

## Lead: the FTP profile read op is probably `0x90ED` (do this first, offline, on the MacBook)

A write-only profile makes no sense (the camera shows it on its menu; NX Field knew "no active profile in camera"
before sending anything in November). Nikon's vendor ops come in read/write pairs at adjacent codes: IPTC
`0x9412` read / `0x9413` write (the November capture shows the read-back after the write), vendor properties
`0x943B` read / `0x943C` write. By that pattern the read for `0x90EE` is **`0x90ED`**. This is an inference from
the numbering, not captured fact.

Supporting evidence already in the repo: the July capture `session-20260709-203300-filtered.pcap` has the
FTP-profile-edit attempt running `0x90E8` → `0x90EB` → `0x90ED`, three times, byte-identical (NXS, "mining the
other pcaps"); a bare `0x90ED` over USB stalls the pipe (so it needs params or a data-out phase, like `0x9412`,
which took a slot parameter); `0x90EB` returned 59 bytes of device identity, not the profile. The November Z6III
captures (e.g. `session-20260709-235137-nxfield-ops.pcap`; the FTP edit was rejected by the app there) may show the
same sequence too.

Plan: the pcaps live on the MacBook. Walk the PTP/IP packets by hand as in the NXS method note (a TCP segment can
carry several packets; `tshark` was only used for counts), find each `0x90ED` request, record its params and any
data-out phase, and read the camera's data-in reply. If the reply carries the profile in a layout close to the
`0x90EE` blob (`encode_ftp_profile` in `src/ptp_usb.rs`), the read op is found. Only after that, a read-only USB
`0x90ED` on the Z6III using the real params; standing rule from the owner: **do not guess `0x90ED` params on a
camera, and no more `vendor-write-ftp` experiments until a read exists** (they overwrite the slot with no way to
see the old value).

### Result of the offline walk (2026-11-05): `0x90ED` is the FTP profile read, confirmed from captures

Method: a small PTP/IP walker (reassembles TCP per direction, groups command / data / response by transaction) run
over the four July pcaps in `captures/2026-07-09/` and the November Z6III pcap. No camera was touched. (The pcaps are
in the repo checkout on this Mac; the "they live on the MacBook" line above was wrong.)

- **`0x90ED` takes one param, the profile slot, and returns the profile blob as its data-in phase.** In
  `session-20260709-204600-ftptest.pcap` (Z9) the reply to `0x90ED(0x0)` is 325 B, and the `0x90EE(0x0,0x0)`
  data-out 16 s later is **byte-identical** (325 B). Later reads of 317 B are byte-identical to the 317 B write
  that preceded them (read-back after write), and three consecutive reads are identical to each other. So the read
  and write blobs share one layout, and `encode_ftp_profile` can be checked against any `0x90ED` reply.
- **Sequence NX Field uses:** `0x90E8` (data-out `01 00 00`) → `0x90EB` (57–59 B, identity + slot info) →
  `0x90ED(slot)`. The slot value comes from `0x90EB`: after the Z9 had an FTP profile it was `0`; with none it was
  `0xff`. `0x944C(0x90ED, 0)` (32 B of mostly zeros, `01 01 01 …`) is sent before the FTP screen; it looks like a
  per-op support query. Not decoded further.
- **Why the November Z6III "had no active profile":** there `0x90ED(0xff)` was answered **`0x201F`
  (Transaction Cancelled)** at 260 s and 363 s, no data phase. Same answer in the July Z9 filtered capture when no
  profile existed. So `0x201F` on `0x90ED(0xff)` is the camera saying "nothing in that slot" (**not** proof that no profile is stored: see the 2026-11-05 USB runs below), which fits
  NX Field refusing to edit.
- **`0x90EB` layout (inference, 3 body states):** `c8 00` | u32 `N` | u32 `0x14` | u32 slot | 16-byte GUID |
  `u8 nchars` + UTF-16LE name (the body's `<model>_<serial>` string, so do not commit it) | `f4 01`. `N` was 6 and
  slot `0xff` on the Z9 before it had a profile, `N` 7 and slot `0` after; `N` was 3 on the Z6III (slot `0xff`).
  The `0x14` (20) and `0xc8` (200) are possibly limits. Meaning of `N` unknown (could be a count of saved network
  profiles, not only FTP).
- **The 331 B blob** (the one the encoder does not model) is a read, not a write, and the decoder now explains it. It
  has a PTP string `07 "Claude" 00` between host and username (offset 153), and flag byte `01` instead of `05` before
  the port (offset 191). The 317 B write sent right after is that read with the string replaced by a single `00` (the
  empty string, which is the first byte of the encoder's `[00 15 00]` username tag) and the flag set to `05`; a test
  pins this byte for byte. The string is **probably the FTP destination folder** (unconfirmed; the camera's menu would
  say), and the flag byte probably holds options such as passive mode (unknown). The encoder can write neither.
- **Standing rule still holds** for hardware: no `0x90ED` on a camera until we choose to run exactly the captured
  sequence (`0x90E8` data `01 00 00`, `0x90EB`, then `0x90ED` with the slot that `0x90EB` reports). `0x90ED(0xff)`
  is known to give `0x201F` without side effects in 5 captured instances, but that was via NX Field.

**Done in code (2026-11-05, not run on a camera):** `src/ptp_usb.rs` now has `decode_ftp_profile` (strict, offset in
every error, never panics on truncation), `parse_ftp_profile_info` for `0x90EB`, and `PtpUsbSession::read_ftp_profile`
(`0x90E8` → `0x90EB` → `0x90ED(slot)`; when the slot is `0xff` it does not send `0x90ED`). The CLI has
`vendor-read-ftp [--serial=S] [--show-password] [--raw]`; the password is masked, and zeroed in the hex dump, unless
`--show-password`. Tests use the real 325/317/331 B blobs (round-trip through the encoder for the first two) and
`0x90EB` replies rebuilt with a dummy GUID and serial. Stale "cannot be read back" wording in the write path's
messages now points at the new command.

First hardware run, when wanted: `vendor-read-ftp` on the Z6III (expected: slot `none (0xff)`, no `0x90ED` sent), then on
the Z9 (expected: the profile; compare against its menu, which also settles what the extra string is). It is read-only,
but `0x90E8` is the same setup step the write path sends and has only been captured, never sent by this tool alone.

## Follow-up: live NX Field session and a PTP/IP profile read (2026-07-10/11)

Written up in `docs/nx-field-ftp-profile-session-2026-07-10.md`. In short: NX Field's profile editor
rewrites slot 0 only; its first write carried four mistakes that
produced the camera errors 31, 32 and 34; a PTP/IP client, `scripts/ptpip_read_ftp.py`, reads every
stored profile over Wi-Fi with the same `0x90E8` / `0x90EB` / `0x90ED` ops as USB; and the camera
accepts any client GUID that begins `01 00 4e 58`, with no pairing list. **A profile can be added**:
`0x90EE(count, 1)` over Wi-Fi inserts a new one at the front (it survived a power cycle); with second
parameter `0`, `0x90EE(N, 0)` replaces slot N and moves it to the front. This supersedes the
"adding a profile" gaps recorded above (the USB `0x90EE(1, 0)` stall was a write one past the end with the
second parameter at 0). Also recorded there: one out-of-range slot value hung the camera's network.
Later the same evening, over USB on the Z9: the `0x90EB` slot field is the index of the *selected*
profile, `0x90EF(n)` deletes profile n, `0x90EA` deletes them all, and `0x90E9` / `0x90EC` exist but are
unexplained (see section 6 of that document); `vendor-probe-op` is the command that sends them.

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
9. **`device-info` on a Z6II** (read-only; the Z6II is Vic's). Tests the Expeed 6 / Expeed 7 split with a second
   Expeed 6 body: expect an operation list like the Z5's (about 89, no `0x943B`/`0x943C`). **The Z8 is deferred**: no Z8 is
   available to the owner. The July Z8 file in `references/` on the Mac Studio is a gphoto2 property dump with no operation
   list (checked 2026-11-05), so it cannot stand in. The owner's stated position, not proven: the Z8 has the same hardware
   and a similar firmware base to the Z9, so treat it as Expeed 7 with the Z9's operation set (about 146, with `0x943B`/
   `0x943C` and the FTP profile operations) until a live read says otherwise. See "Z5 vs Z9 operation lists" below.
10. **Z5 Connect to PC operations**: capture Nikon's Wireless Transmitter Utility saving an entry to the Z5 over USB (check
    macOS USB capture first). The Z5 does not answer `0x90E8`–`0x90EE` and has FTP under Connect to PC; comparing
    DeviceInfo lists cannot find its operations (they are probably unlisted, as the Z9's are). See "Z5 tested" below.

## Practical notes for the MacBook

- Experiments 2–5 need a camera attached by USB, in PTP/MTP mode (not USB LAN). Quit anything that grabs the camera.
- Experiment 6 needs the Raspberry Pi AP from `docs/rpi-ap-capture-setup.md` and the camera in "Connect to other
  cameras" → remote mode. Set the phone's Auto-Lock to Never if NX Field is also in use.
- Local `master` was fast-forwarded to `origin/master` (`ce5d57a`) at the start of this session; that is the base
  for this note.

### First USB runs (2026-11-05) and a correction

`vendor-read-ftp` ran on the Z6III (`Z6_3`, `N`=3) and on two different Z9 bodies (`N`=1 and `N`=3). All three answered
`0x90E8` and `0x90EB` over USB and parsed cleanly, and all three reported slot `0xff`. The owner says the second Z9 holds
FTP profiles. So **slot `0xff` means "no active (selected) profile", not "no profile stored"**; the earlier reading
("no profile") was an inference from two July bodies and was wrong. The tool's output and a constant's name were
corrected to say "active". `0x90ED` has still not been sent over USB: with the slot at `0xff` the tool does not send it.

Open: how to read a stored-but-inactive profile. Options: make one profile active in the camera's menu and rerun (no
guessing); or send `0x90ED(0)` / other slot numbers (the only captured successful read is slot 0; this is the "guess
`0x90ED` params" the standing rule forbids until the owner says otherwise). Also unknown: what the profile count `N` counts.

### First profile read over USB (2026-11-05): `0x90ED(0)` works, and the "port" is not where the encoder puts it

With one profile made active in the camera's menu, `0x90EB` reported slot `0` and `0x90ED(0)` returned a 354-byte profile
from a Z9 over USB, so **the read works on hardware**. Layout corrections found from it (the layout rebuilds the 325 B and
331 B July blobs and this 354 B one byte for byte):

- The u16 between the (usually empty) folder string and the username is a **port**: 21 in the July blobs, 22 in this one.
  The old "username tag `[00 15 00]`" was the empty folder string plus the FTP port 21 as a constant.
- The u16 after the password, which `FtpProfile.port` and `vendor-write-ftp --port` write, is **8080 in all four blobs**
  (including one made on the camera with port 21 typed in). 8080 is the usual default proxy port. So the encoder's `port`
  almost certainly lands in a different field than the FTP port, and the FTP port is always written as 21. Unconfirmed
  against the menu: check the profile's server port and proxy settings on the body.
- The u16 before it ("option flags") was `0x0005`, `0x0001` and now `0x000d`; meaning unknown, probably protocol/passive
  bits. This profile used port 22 and flags `0x0d`.
- `decode_ftp_profile` now exposes `ftp_port`, `option_flags` and `second_port`; `as_encodable` is `None` unless the port
  is 21, the flags `0x0005` and the folder empty.

Not yet matched against the camera's menu: the profile's server type (FTP/FTPS/SFTP), port, passive mode, proxy and
folder. That comparison would settle the port, flags and folder meanings.

Slot semantics (superseded just below): the `0x90EB` slot looked like the active profile.

Process note: the first run printed the raw hex of a blob the decoder rejected, which included that profile's password
in clear. `vendor-read-ftp --raw` now withholds the dump when it cannot locate the password, unless `--show-password`.

### All stored profiles read over USB (2026-11-05, Z9 with 3 profiles)

The owner then said the Z9 holds several profiles, and approved `vendor-read-ftp --slot N` (read-only; `0x90ED(N)`).

- **`0x90EB` `count` is the number of stored FTP profiles** (3 here), and `0x90ED(N)` for `N` in `0..count` returns
  profile `N`: slot 0 = `PHONE FTP`, slot 1 = `NETSOL`, slot 2 = `PHONE_DIREC8`. The `0x90EB` "slot" field stayed `0` when
  the active profile changed from NETSOL to PHONE FTP, so it is **not** the active profile; `0xff` there is unexplained
  (it is what every body reported before a profile was selected/created, and 0 once one was). The earlier readings of
  "no profile" for the Z6III (`count` 3) and the other Z9 (`count` 1) were wrong: they hold 3 and 1 stored profiles. The
  tool now reads them with `--slot`.
- **`0x90ED(count)` stalls the USB pipe** (`Pipe error`; the session clears the halt and the camera answered the next
  run normally). NX Field, over PTP/IP, got `0x201F` for `0x90ED(0xff)`. `--slot` refuses `N >= count` before sending.
- New layout facts (rebuilds every blob seen, 325/331/354/319/329 B): the 2.4 GHz tag is `03 00 xx 00` with `xx` = `01`
  normally and `0x30` for a profile with no stored camera address; the 5 GHz-field tag is `xx 04` with `xx` = `04` or
  `06` (both seen on hotspot-style profiles); the camera-address block can be all zero; the "second port" is 0, not 8080,
  in `PHONE_DIREC8`; usernames and passwords can be empty (the two phone-hotspot profiles have no login, FTP port 2221,
  option flags `0x0007`). Option flags seen: `0x0001`, `0x0005`, `0x0007`, `0x000d`.
- **The "5 GHz SSID" field is very probably the Wi-Fi password** (July: `nikon5ghz` = the rig AP's passphrase; the
  phone-hotspot profiles carry a hotspot-style value). The CLI masks it, with the FTP password, unless `--show-password`.
  Two earlier outputs in the working session showed such values in clear before this masking existed.
- Still unconfirmed against the camera's menu: what the option flags, the `ssid24_kind`/`ssid5_kind` tags and the
  "folder" string mean. A per-profile comparison with the menu (protocol, passive, anonymous, proxy, security type) would
  settle them.

### The Z6III's profiles, and what the menu confirmed (2026-11-05)

`vendor-read-ftp --slot 0..2` on the Z6III (`count` 3, `0x90EB` slot `0xff`):

| Slot | Name | Host | Folder | Login | Port | Flags |
|---|---|---|---|---|---|---|
| 0 | CLAUDE | 192.168.66.1 | | fleet | 21 | `0x0005` |
| 1 | Home | 192.168.68.250 | `/Volumes/archive3/images/2026/incoming-ftp` | blw | 22 | `0x0009` |
| 2 | PHONE_DIREC10 | 10.46.210.82 | | none | 2221 | `0x0007` |

- Slot 0 is exactly what our encoder writes (camera address 192.168.66.87/24, port 21, second port 8080): a leftover from
  the earlier `vendor-write-ftp` tests. The old note that the USB write was "reverted" is wrong for this body.
- **Confirmed:** the string between host and username is the **destination folder** (`Home` carries a real path), and
  option bit **`0x08` is SFTP** (the owner confirmed `Home` is SFTP; `NETSOL`, the other port-22 profile, also has it).
- Option bits across the 8 profiles read: `0x08` = SFTP; `0x02` appears only on the empty-login (probably anonymous)
  profiles; `0x04` unexplained (set on `CLAUDE`, `NETSOL`, the phone profiles; clear on `Home` and the iPad-made one;
  maybe passive mode); `0x01` set on all but the iPad-made profile.
- The `FtpProfile.port` problem stands: the encoder writes its port to the second-port field and fixes the FTP port at
  21, no folder, no SFTP bit. A correct writer needs `ftp_port`, `folder` and the option bits as inputs.

### Z5 and the second Z9 (2026-11-05)

**Z5 (V1.43): DeviceInfo says nothing either way (see the Z5 test below: it does not answer `0x90E8`).** Its DeviceInfo lists 89 operations, 70 in the vendor range
(`0x90C0`–`0x90CF`, `0x9201`–`0x920F`, `0x9400`–`0x9535`), without `0x90E8`/`0x90EB`/`0x90ED`/`0x90EE` or `0x943B`/`0x943C`.
**That does not show the Z5 lacks the FTP operations**: a Z9 (146 operations) does not list `0x90E8`–`0x90EE` either, yet
answers all four, so they are hidden from DeviceInfo. (An earlier version of this note said the Z5 "says it does not support
them"; that was wrong.) `0x943B`/`0x943C` are different: the Z9 and Z6III do list them, so their absence on the Z5 is
likely meaningful and `vendor-read` there should answer OperationNotSupported (C4), but that is untested. Whether a Z5 has
FTP profiles at all, and whether it answers `0x90EB`, is unknown; a read-only `vendor-read-ftp` would find out (it could
answer `0x2005` or stall the pipe, as `0x90ED` past the end did on a Z9).

**Second Z9 (`Z_9_3023668`): `count` 1, one profile, left over from our own write tests.**

| Slot | Name | Host | Login | FTP port | Second port | Flags | Camera address |
|---|---|---|---|---|---|---|---|
| 0 | txid-test | 192.0.2.1 | txid | 21 | **21** | `0x0005` | 192.168.66.87/24 |

`vendor-write-ftp --port` defaults to 21, and this profile's second-port field holds 21 where capture-based profiles hold
8080. That is the encoder's `port` landing in the second-port field, as its code does; the FTP port stays at the hard-coded 21.
So `--port` has never set the FTP port on any body.

**Where every body stands** (`count` = stored profiles; `0x90EB` slot `0xff` = none selected as active):

| Body | `count` | Profiles |
|---|---|---|
| Z9 `…3029610` | 3 | PHONE FTP, NETSOL (SFTP), PHONE_DIREC8 |
| Z9 `…3023668` | 1 | txid-test (our write test) |
| Z6III `…3014527` | 3 | CLAUDE (our write test), Home (SFTP, with folder), PHONE_DIREC10 |
| Z5 | n/a | no FTP profile operations |

The two test profiles (`txid-test`, and `CLAUDE` on the Z6III) can be deleted from the cameras' menus.

### Write experiment on the first Z9 (`Z_9_3023668`, 2026-11-05)

With the owner's permission (the only stored profile there was a non-functional test one):

1. `vendor-write-ftp` with new field values (name `txid-edit`, host 192.0.2.2, `--port 8080`, ...) was accepted, and
   `vendor-read-ftp --slot 0` returned exactly what was sent (the first read-back of any write). `count` stayed 1:
   **`0x90EE(0, 0)` replaces slot 0**.
2. A new `vendor-write-ftp-blob` (writes a decodable blob unchanged, with chosen parameters; refuses a slot past the stored
   count) sent a throwaway profile with **`0x90EE(1, 0)`**, i.e. one past the end. Result: the data phase was sent, then
   `USB error: Pipe error` (a stall; the session cleared the halt). Afterwards `count` was still 1, slot 0 unchanged, and
   the camera answered normally. **No profile was added.**
3. So `0x90EE`'s first parameter is not simply "slot, with N == count adding a new one", at least not this way. NX Field
   only ever sent `[0, 0]` (replacing the one it edits). Untested alternatives: `[0, 1]`; other values in the `0x90E8`
   setup data (`01 00 00` in every capture); an operation in the hidden `0x90E_` range that creates a profile. Each
   untested value risks a stall (harmless so far) and none should be sent without the owner's say-so.
4. Copying profiles between bodies is therefore possible only as "replace slot 0" for now; adding to a camera that holds
   other profiles is unsolved. Reading, and replacing the single slot-0 profile, are solid.

### Z5 tested (2026-11-05): the FTP profile operations time out

`vendor-read-ftp` on the Z5 (V1.43) failed with `USB error: Operation timed out`. OpenSession had succeeded and a
DeviceInfo read right afterwards worked, so the first FTP operation (`0x90E8`, with its 3-byte data phase) got no reply at
all, rather than the clean `0x2005` OperationNotSupported. The session was poisoned as designed and the camera was fine.
So the Z5 does not implement these particular operations. **It does have FTP** (the owner knows it from use: the transfer
is very slow, about 15 or 150 Mbit/s). The Z5 is an Expeed 6 body and the Z6III/Z9 are Expeed 7, with a quite different
firmware set; on the Z5 the equivalents of FTP profiles live under **Connect to PC** in the menu, not under a
"Connect to FTP server" profile list. (An earlier version of this paragraph said the timeout was consistent with the Z5
having no FTP feature; that was wrong.) The Z5's operations for those settings are therefore unknown, and are probably not
`0x90E8`–`0x90EE`. The mechanism for reading or writing them over USB has to be found separately, for example by capturing
the traffic of Nikon's Wireless Transmitter Utility (the app the cameras kept asking for earlier in this work) while it
sets up a connection, or by comparing the Z5's vendor operation list with older bodies. Neither has been done. The error
does not say which operation timed out; that is inferred from the session opening normally.
This supersedes the "not tested" paragraph for the Z5 above; the point there that DeviceInfo says nothing
about the hidden `0x90E_` operations still stands.

### Z5 vs Z9 operation lists, and generations (2026-11-05)

Per the owner (recall, not checked here): the Z5 is Expeed 6, as are the Z6II/Z7II, Z30, Z50, Zfc and original Z6/Z7;
the Z8, Z9, Z6III and Zf are Expeed 7; the ZR is believed to be Expeed 7. The repo holds DeviceInfo data for only the Z5
(Expeed 6) and the Z6III and Z9 (Expeed 7); the Z8's is in `references/` on the Mac Studio. So "Expeed 6 vs 7 gives a
different protocol set" currently rests on one Expeed 6 body.

Vendor-range operations (`>= 0x9000`) advertised in DeviceInfo, read over USB:

- Z5 (V1.43): 89 operations in all, 70 vendor-range. Z9: 146 operations, 126 vendor-range.
- **68 are shared. Only two are on the Z5 and not the Z9: `0x90CA` (GetVendorPropCodes) and `0x9432` (unknown).** The Z9
  has 58 the Z5 lacks (`0x9401`, `0x9412`/`0x9413` IPTC, `0x942E`/`0x942F`, `0x9435`/`0x9436`/`0x9439`, `0x943A`–`0x9447`
  including `0x943B`/`0x943C`, `0x944B`/`0x944C`, `0x9450`–`0x9454`, `0x945C`–`0x9478`, `0x9483`, `0x9527`–`0x9529`).
  The Z5 is nearly a strict subset of the Z9's list.
- The Z9's own FTP profile operations (`0x90E8`–`0x90EE`) are not in its list, so the Z5's Connect to PC operations are
  probably unlisted too, and **comparing the lists cannot find them**. The only listed candidate is `0x9432`, and that is
  a guess.

Ways forward, none done:

1. Capture the traffic of Nikon's Wireless Transmitter Utility while it saves a Connect to PC entry to the Z5 over USB
   (macOS USB capture may need checking; the Pi captured Wi-Fi only).
2. A read-only `device-info` on a Z6II (another Expeed 6 body, available through a friend) and on the Z8: if the Z6II's
   list looks like the Z5's (about 89 operations, no `0x943B`/`0x943C`) and the Z8's like the Z6III's (about 146), the
   generation split gets a second data point on each side.

### `0x944C` probed on the Z9 (`Z_9_3023668`, 2026-11-05): it does not say whether an op exists

New read-only command `vendor-op-support <op>` sends `0x944C(<op>, 0)` and prints the response code and bytes verbatim (it
sends nothing else). Results, one query per run:

| op | response | data |
|---|---|---|
| `0x90ED` (what NX Field sends) | OK | 32 bytes: `01 01 01`, then zeros (matches the capture) |
| `0x90EE` | OK | the same 32 bytes |
| `0x90E8`, `0x90EB`, `0x943B`, `0x9413` (all known to work) | InvalidParameter `0x201d` | none |
| `0x90E9`, `0x90EA`, `0x90EC`, `0x90EF` | InvalidParameter | none |
| `0x9FFF` (control, should not exist) | InvalidParameter | none |

- The guess that `0x944C` reports whether the camera implements an op is **wrong**: four ops known to work get the same
  answer as one that does not exist. It answered OK for exactly the FTP read/write pair, with identical bytes for both.
  What `01 01 01` means is unknown. The second parameter has only ever been 0.
- It therefore cannot enumerate the hidden `0x90E_` range, and nothing new was found there. The `0x944C` guess in the
  other sections of this document is superseded by this table.

### Read-modify-write on the Z9's slot 0 works (2026-11-05)

On the leftover test profile (`txid-edit`, host 192.0.2.2). `vendor-read-ftp --save-blob <file>` now saves the true raw
bytes (mode 0600, refuses to overwrite). Using only `0x90ED(0)` to read and `0x90EE(0, 0)` to write back
(`vendor-write-ftp-blob`):

1. Writing the 323 bytes read, unchanged: the read-back was byte-identical.
2. Patching only the profile name at the byte level (`txid-edit` to `rmw-test`, so the blob became 321 bytes): the
   read-back was byte-identical to what was written. So the blob has no hidden length or offset fields that a length change
   breaks.
3. Writing the original back: byte-identical to the original (SHA-256 `dfb79e87…`).

Option flags (the `u16` after the password; `0x0005` in the original), each written then read back, then the original
restored:

| flags written | read-back |
|---|---|
| `0x04` (clear `0x01`), `0x07` (set `0x02`), `0x15` (set `0x10`, never seen before) | identical to what was written |
| `0x01` (clear `0x04`), `0x00` (clear all) | **2 bytes longer** (325 vs 323), flags as written |

- The camera stores unknown flag bits as given (`0x10` came back), so a writer that preserves unknown bytes is safe on
  this point.
- With bit `0x04` clear, the camera re-serialises the empty **folder** field: the single `00` (string length 0) comes back
  as `01 00 00` (a length-1 string holding only a terminator), and everything after it shifts by 2. Nothing else changed.
  A writer must not assume the blob length is fixed or that read-back equals what was sent; the existing decoder reads
  both forms. That bit `0x04` means something like "a folder is specified" is a guess.
- Not checked: what bits `0x01`, `0x02`, `0x04`, `0x10` do in the camera's menu or in an actual FTP transfer, adding a
  profile, deleting one, slots 1 and up (the Z9 holds only slot 0).
