# NX Field / Z9 Capture Session — 2026-07-09

Raw data: [`captures/2026-07-09/`](../captures/2026-07-09/) (gitignored — pcaps, `ftpd.log`, `timestamps*.log`, uploaded photos/WAVs).

## Setup

RPi5 AP method per [rpi-ap-capture-setup.md](rpi-ap-capture-setup.md): `nikon-snoop` at `192.168.66.1`,
Z9 at `192.168.66.87`, iPad (NX Field) at `192.168.66.97`. tcpdump on `wlan0` (no ARP-MITM needed —
brcmfmac has no hardware-bridging fastpath in AP mode).

Four capture files across the session (connection drops required restarts a few times):
1. `session-20260709-193356.pcap` — original Phase 2 sweep (AF area/AF-S-AF-C/ISO/aperture/shutter/mode/expcomp/WB), steps 8–9 (sync release, FTP) skipped.
2. `session-20260709-201758.pcap` — redo with tighter start/done timestamp brackets; isolated the AF-area-mode and early FTP-status wire traffic.
3. `session-20260709-203300-filtered.pcap` — host-filtered (camera+iPad only) FTP-profile-edit attempt.
4. `session-20260709-204600-ftptest.pcap` — the big one: real FTP server, real transfers, IPTC profile write, FTP profile write, voice-memo failure.

**Method note:** tshark's default `ptpip` dissector desyncs on large data-phase transfers (image bytes
get misread as packet headers). All property/opcode decoding here used `tshark -z follow,tcp,raw,<n>`
(proper reassembly) + a hand-rolled length-prefixed PDU parser per the PTP/IP spec. Transaction IDs get
reused heavily within a session — matched requests to data phases in strict arrival order per transaction
id, not by id alone.

## Standard PTP property mappings confirmed

| Code | Name | Notes |
|---|---|---|
| 0x5005 | WhiteBalance | |
| 0x5007 | FNumber | value/100 = f-number (0x0230→f/5.6, 0x0190→f/4.0) |
| 0x500A | FocusMode | |
| 0x500C | FlashMode | fires as a side effect of ExposureProgramMode changes |
| 0x500D | ExposureTime | |
| 0x500E | ExposureProgramMode | 1=M, 3=A, 4=S (matches PTP 1.1 spec table) |
| 0x500F | ExposureIndex (ISO) | **never seen as a SetDevicePropValue request** — only as an event; ISO commit likely goes through a vendor op, not 0x1016 |
| 0x5010 | ExposureBiasCompensation | milli-EV units |
| 0x5011 | DateTime | PTP String: 1-byte char-count prefix (incl. null) + UTF-16LE |
| 0x5018 | BurstNumber | |

## Vendor opcodes decoded this session

- **`0x9413`** — IPTC profile write (iPad→camera, dataphaseinfo=2). Params `[slot, 0x14, 0x9]`, slot
  matches the profile's assigned index. Data phase: sequential `[uint32 length incl. null][UTF-8 bytes]`
  fields (length=0 → empty, no bytes), fixed field order = Title, Creator, Description, Event, Headline,
  City, State, Country, Category, Supplemental Categories, Authors Position, Caption Writer, Credit,
  Source (14 total, count-matches exiftool's XMP output on the resulting JPEG). Exact position of Title
  vs Creator not yet confirmed (both were empty in our test) — need a follow-up test with those two filled.
- **`0x90E8`** → **`0x90EE`** — FTP profile write. `0x90E8` is a shared setup/status step (also seen alone
  during read-only status checks); `0x90EE` carries the actual profile data phase. Mixed-encoding blob:
  SSID/network name fields in plain ASCII (`nikon-snoop`, `nikon5ghz`), host IP and profile name in
  UTF-16LE (`192.168.66.1`, profile name), port as a raw uint16, username/password in plain ASCII
  (`fleet`/`12345678`), trailing unused all-zero IPv6 placeholder fields.
- **`0x943B`** — generic vendor-property read wrapper: param = `0x10000 | propcode`. Cycles through ~15
  distinct property codes repeatedly as part of background chatter (see below) — very useful for finding
  *new* codes: anything with a global occurrence count of 1 across a capture is a real one-off, not noise.
- **`0x9472`/`0x9473`** — looks like a chunked binary-data fetch (24576-byte fixed chunks) tied to a
  specific filename reference (saw `FVN_5023.WAV` referenced just before it) — probably NX Field pulling
  waveform/audio data for its voice-memo playback UI. Not investigated further; unrelated to FTP/IPTC.

## Background noise, fully characterized

NX Field polls a fixed set every ~5.1s regardless of user action: `GetDevicePropValue` on
`0xD259, 0xD1F1, 0xD1B0, 0xD1B3, 0xD1B1, 0xD054, 0xD0B5, 0xD0B4` + vendor ops `0x938D`/`0x942B` (no
params). Filter this out before attributing any wire event to a specific user action.

**`DevicePropChanged` (0x4006) events are unreliable as a change-detection signal** — they clustered
right at connection time in every capture, then went silent for the rest of the session even though
`SetDevicePropValue` calls kept happening. If a design depends on this event stream to catch side
effects, it will miss things once the initial burst passes.

## Vendor property behavioral notes (not yet named)

- **`0xD05D`** — NOT aperture-specific (my first-pass guess in the original session was wrong). Reused
  across contexts: fired during an aperture-scroll test *and* during an AF-area-mode change, with values
  in the same numeric family (`0x10xx`–`0x1F80` range, `0x80` high bit set). Best read: a generic "current
  picker/list selection index" UI-state property, not a photographic setting itself.
- **`0xD0B4`** — scrolls through round numbers (100, 200, 400, 800, 1600, 3200, 6400, 12800, 25600,
  51200, 102400) during ISO-adjacent interactions — ISO-shaped but not confirmed as *the* ISO commit path.
- **`0xD100`** — heavy scrolling-picker traffic (10–15 SetDevicePropValue calls per single logical UI
  action) with values that decompose into two uint16 halves that look like shutter-speed numerator/
  denominator pairs. Not confirmed.
- **`0xD1AC`/`0xD1BC`/`0xD1A6`** — always fire together as a trio.
- **`0xD010`** — cycles 4→3→2→1 once at connection time only; not seen again after.
- **`0xD0B3`** — fires exactly once per capture, precisely when viewing the FTP status screen (via the
  `0x943B` generic-read wrapper). Best candidate for "FTP status/profile" property.
- **`0xD25B`** (already logged as `[Unknown Property]` in [todo.md](todo.md) — not a new code ID, but new
  behavior) — `SetDevicePropValue` on this returned `DevicePropNotSupported` (0x200A) three times in a
  row during a voice-memo-attach attempt, right before the one shot (`FVN_5022`) that never got its WAV
  companion. Strong candidate for the actual memo-attach mechanism; camera rejected it in this context.
  Neighbors `0xD038`/`0xD039`/`0xD03A` also rejected in the same burst — worth a `GetDevicePropDesc` pass
  on all four.
- **`0xD1F0`** (code already catalogued via an earlier USB/libgphoto2 pass, per todo.md — not new today)
  — got set on this Z9 despite todo.md flagging it Z5-only/absent-from-Z9-Z6III. Either that per-body
  diff table's source enumeration was incomplete, or "supported" there meant something narrower than
  "settable." Worth reconciling.
- **`0xD07F`** (also already catalogued, Z9-only per todo.md) — confirmed present/settable on this body,
  consistent with the existing diff table.

## FTP auto-send behavior (fully mapped, real transfers, real Z9)

Set up a minimal pure-Python FTP server (stdlib only — no internet route to install vsftpd; script at
`captures/2026-07-09/` is not kept, was scratch) to actually receive transfers rather than just watch
connection tests fail.

- Filenames: `FVN_####` prefix (not Nikon's usual `DSC_`) — either a custom naming setting or Z9-specific.
- **Collision-avoidance naming**: before each send, the camera checks `SIZE` for all four possible
  companion extensions (`.JPG/.NEF/.HIF/.WAV`) regardless of what was actually shot. If a name is already
  taken on the server (confirmed via a `SIZE` matching what it just sent), the *next* logical shot's
  filenames get a `-1` suffix rather than overwriting. This applies even when re-sending the *same* photo
  after adding a voice memo after the fact (confirmed via matching EXIF timestamps between `FVN_5023.JPG`
  and `FVN_5023-1.JPG`).
- **`REST 0` is attempted once per upload** (presumably to verify resume support) — our server doesn't
  implement it (502), and the camera just proceeds without retrying. Optional/best-effort on its end.
- **The camera auto-navigates into a subdirectory named after the FTP profile** on connect (`CWD
  <profile-name>`) — retried 6 times with `550 No such directory` before giving up and proceeding to the
  root on the 7th attempt, when we hadn't pre-created it. Fixed by creating the matching directory.
  Actual file uploads still went to the server root regardless of profile name, though — the CWD-on-connect
  behavior and the actual upload target directory are apparently independent.
- IPTC profile fields (Description/Event/Headline/Source) and Copyright/Artist both round-tripped
  correctly into the uploaded JPEG's embedded EXIF/XMP/IPTC (verified with `exiftool`). Copyright/Artist
  were **not** set during this session, though — traced every occurrence across all captures and confirmed
  the value pre-dated this session's IPTC/copyright test entirely (already on the body beforehand).
  Confirms Copyright Info (Setup Menu) and the IPTC profile mechanism (Photo Shooting Menu, sent from
  NX Field via `0x9413`) are genuinely separate camera features, not the same pipeline.

## MAID / SDK / PTP-IP relationship (important architectural clarification)

Confirmed by reading `src/sdk.rs` and `docs/todo.md` directly, not assumed:

- **MAID = Nikon's Remote SDK v2 settings/capability API** (`Maid3.h`/`NkMAID*` types). Its own docs
  already establish it's missing file naming, FTP settings, AF fine-tune, copyright — matches everything
  found this session.
- **The SDK is USB-only.** `src/sdk.rs` does `rusb` enumeration and USB connect — it never touches PTP/IP
  over WiFi. NX Field talks PTP/IP over WiFi/mDNS to port 15740. Two structurally separate transports to
  the same camera, not just a documentation gap — MAID cannot reach the WiFi-side vendor ops even in
  principle, regardless of what the firmware supports.
- None of today's discovered opcodes/properties (`0x9413, 0x90E8, 0x90EE, 0x943B`, and the `0xD0xx`
  behavioral notes above) appear anywhere in SDK-related files — confirmed by direct search.
- **Conclusion**: any tool built on today's findings should be a PTP/IP-over-WiFi client that's an
  *adjunct* alongside `src/sdk.rs`'s USB path, not a modification of the SDK/MAID layer. The project's own
  docs already anticipated this — newly-found vendor codes just need plain `GetDevicePropValue`/
  `SetDevicePropValue`, no SDK changes needed.

## Side-effect verification — current gap, and existing tooling

Observed pattern across everything decoded today: fire an operation, trust the response code, at most
spot-check the *one* value just written (immediate same-property readback, or a `SIZE` check after
`STOR`). Nothing checks whether *other* properties changed as an unintended side effect — that detection
relies entirely on `DevicePropChanged` events, which we confirmed go silent after the initial connection
burst.

`fleet snapshot` / `fleet diff` / `fleet ref set` / `fleet check` already exist (`src/snapshot.rs`,
`src/diff.rs`, `cmd_check` in `main.rs`) and give a real before/after diff — but only over the SDK's
USB-visible capability set, which doesn't include the WiFi-only vendor ops above. For the specific
suspicion driving this (**still↔video mode switch via NX Field having unlogged side effects**), a quick
free test with existing tooling: `fleet ref set` → switch to video and back via NX Field → `fleet check`.
That catches anything in MAID's scope. Catching anything WiFi-only-vendor-side would need the PTP/IP
adjunct client (above) doing the same before/after snapshot+diff, but over the full known `0xD0xx`/`0x5xxx`
vendor-property space instead of just what MAID exposes.

## Open items for next session

- [ ] Still↔video switch side-effect check (both via `fleet check` for MAID-scope, and via a raw PTP/IP
      before/after snapshot for WiFi-only vendor props, once the adjunct client exists)
- [ ] `GetDevicePropDesc` pass on `0xD25B, 0xD038, 0xD039, 0xD03A` (voice-memo-attach cluster)
- [ ] Confirm exact Title vs Creator field position in the `0x9413` IPTC blob (both empty in this test)
- [ ] Reconcile `0xD1F0`/`0xD07F` per-body support table in todo.md against what we saw settable on this Z9
- [ ] Decide: prototype the PTP/IP adjunct client in Python first (reuse today's parser code) or start
      directly in Rust as a new module alongside `src/sdk.rs`
- [ ] Phase 3 (body Custom Settings, starting with **c3 Standby Timer**) — never started this session
- [ ] Sync release mode and a real (non-status-only) FTP profile edit from NX Field's UI — both were
      attempted in the original session but never produced wire traffic distinguishable from background
      noise
