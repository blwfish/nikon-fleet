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
- **Conclusion (superseded, see "USB-transport test" below)**: this originally said any tool built on
  today's findings should be a PTP/IP-over-WiFi client, since MAID (the SDK) can't reach WiFi-only
  vendor ops "even in principle." That's still true of *MAID specifically* — but the 2026-09-06
  follow-up below found `0x9413`/`0x90E8`/`0x90EE`/`0x943B` all work over plain raw USB PTP (not through MAID,
  through a separate hand-rolled PTP-over-USB client) once called with the correct wire format. The
  vendor ops were never actually WiFi-gated; the original assumption that they were came from
  under-tested USB attempts, not a real transport restriction. A WiFi/PTP-IP adjunct client may still
  be worth building for convenience/parity with NX Field, but it is not required to reach these ops.

## USB-transport test for WiFi-discovered vendor ops (2026-09-06 follow-up)

Answers the question the "MAID / SDK / PTP-IP relationship" section above left open: is the PTP
command dispatcher transport-agnostic for `0x9413`/`0x90E8`/`0x90EE`/`0x943B`, or are they gated to
the WiFi/PTP-IP session context? Tested directly against a Z6III on raw USB PTP (Bulk-Only
Transport container framing, Python + `pyusb`; see `docs/handoff-usb-ptp-vendor-op-test.md`, now
deleted, for the original task spec). macOS's `ptpcamerad`/`icdd` had to be killed (they re-claim
the interface as soon as it's touched — SIP blocks permanently unloading their LaunchAgents, so
this is a kill-immediately-before-claim race, not a one-time fix) and a stale open session
(`0x201e SessionAlreadyOpen`, left over from `ptpcamerad`'s own connection) had to be closed before
a fresh `OpenSession` would succeed.

**Result (revised after a second pass, same day): not gated at all — the two apparent failures were
wire-format bugs, not transport gating.**

- **`0x943B` (vendor-property read wrapper) works over USB.** First attempt used `0xD0B4` (from the
  handoff's example) and got `DevicePropNotSupported` (`0x200a`) — but `0xD0B4` isn't actually a
  code confirmed present on the Z6III (it's Z9-only per the todo.md per-body table), so that's the
  property being rejected, not the operation. Retried with `0xD053` (the copyright field, confirmed
  present/working on all four fleet bodies) and got a clean `OK` (`0x2001`) with 1 byte of real data
  back (`01`).
- **`0x9413` (IPTC profile write) works over USB.** First attempt used the originally-documented
  params `[slot, 0x14, 0x9]` (slot 1, then slot 0) and got a clean `ParameterNotSupported` both
  times. Re-analyzed the original 2026-07-09 pcap with a proper TCP-stream reassembly (scapy,
  sorted/deduped by sequence number — the earlier by-eye hex reading that produced `[slot, 0x14,
  0x9]` was simply wrong) and found the real wire format takes **one param, value `1`**, with no
  distinct "select/create profile" step beforehand — it's preceded only by NX Field's routine
  background polling. Replayed the exact 104-byte data-phase blob captured from a real, successful
  WiFi transaction (byte-for-byte, rather than a hand-built guess at the per-field layout) with the
  corrected single param: **clean `OK` (`0x2001`)**. Done with the user's explicit go-ahead, on a
  body where this camera's IPTC profile isn't part of their workflow (no revert needed/attempted;
  Description-equivalent field now holds leftover test text — harmless, ignorable).
- **`0x90E8` (FTP profile status) works over USB.** The very first attempt (no params, no data)
  timed out waiting for a response and then stalled the bulk pipe (recovered cleanly with
  `clear_halt()`, no power cycle needed) — but the same pcap re-analysis showed every real instance
  of `0x90E8` carries `dataphase=2` with a 3-byte data-out payload (`01 00 00`). The original test
  sent zero data, so the camera was correctly waiting for a data phase that never arrived — that's
  the entire explanation for the hang, not a WiFi-only gate. Retried with the 3-byte payload: clean
  `OK` (`0x2001`).

- **`0x90EE` (FTP profile write — real network credentials) works over USB too.** Re-derived its
  wire format the same way: `params=[0, 0]`, `dataphase=2`, a 325-byte data blob. Notably, in the
  captured instance used as a reference, **`0x90EE` was not preceded by a distinct `0x90E8` setup
  call at all** — just NX Field's ordinary background polling — so the "0x90E8 is a shared
  setup/status step for 0x90EE" claim in the original notes doesn't hold for this instance either;
  it can be sent standalone. Field boundaries for username (`fleet`, 5 ASCII bytes) and password
  (`12345678`, 8 ASCII bytes) were identified precisely by their 4-byte length prefixes elsewhere in
  the blob (mixed ASCII/UTF-16LE encoding, several fields still not fully mapped — SSID names,
  device-name-like UTF-16 fields, an unexplained ~100-byte prefix before the WiFi/host section, and
  three all-zero IPv6-string placeholder fields at the end). Rather than fully decoding all of it,
  took the real captured blob and substituted only the username → `FLEET` and password → `87654321`
  (both exactly the same byte length as the originals, so no other field or offset shifts) — **clean
  `OK` (`0x2001`)**. Done with the user's explicit go-ahead ("read the credentials, then mutate them
  in a known way, I can always re-set them"). There is no PTP-level read-back for this profile
  anywhere in the capture, so "read" here meant the values NX Field itself wrote during the original
  2026-07-09 session (pointing at a since-torn-down Raspberry Pi test AP: SSID `nikon-snoop`/
  `nikon5ghz`, host `192.168.66.1`, port `8080`) — not necessarily what was actually stored on the
  camera going into this test. **Current known state of this Z6III's FTP profile: username
  `FLEET`, password `87654321`, everything else per the values above.** Visible/resettable via the
  camera's own Setup Menu.

  **Revert attempt (same session, minutes later):** wrote the original blob back (`fleet`/
  `12345678`) to restore the pre-test state after the user visually verified the mutation on the
  camera's own menu. **This second `0x90EE` write hung twice in a row** (bulk-IN timeout, pipe
  stall, recovered each time with `clear_halt()`) — same failure mode as the very first `0x90E8`
  attempt earlier in the day. Sending a standalone `0x90E8` (3-byte payload, as derived above)
  immediately before the retry fixed it: clean `OK`. **So the original notes' "`0x90E8` is a shared
  setup step for `0x90EE`" claim holds, but only conditionally** — the *first* `0x90EE` write in a
  fresh PTP session can skip it (confirmed twice: the original WiFi capture's first write, and this
  session's first USB write), but a *second* write in the same session needs `0x90E8` sent
  immediately before it or the camera never responds. Profile now restored to `fleet`/`12345678`,
  matching the original 2026-07-09 capture. Not yet confirmed whether the requirement is "once per
  session" or "once per N writes" or something else — only tested first-write-skips-it /
  second-write-needs-it, n=1 each.

**Implication (revised): the PTP command dispatcher is transport-agnostic for all four vendor ops
tested.** There is no evidence of WiFi/PTP-IP-session gating for `0x943B`, `0x9413`, `0x90E8`, or
`0x90EE` — every failure traced back to an incorrect param count or a missing/incomplete data phase,
both fixable by re-deriving the real wire format from the existing capture rather than assuming the
op needs WiFi. This means a `fleet` USB code path is plausible for all four, pending the same
careful-pcap-reanalysis treatment for any other vendor op it wants to use (don't trust a
by-eye-read param list without a proper reassembly, per the mistakes above).

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
- [x] Build a `fleet` USB code path for `0x943B` vendor-property reads — done 2026-09-06:
      `src/ptp_usb.rs` (new module, `rusb`-based, no MAID SDK involved) + `fleet vendor-read <code>
      [--serial <serial>]`. Handles the macOS `ptpcamerad`/`icdd` reclaim race (kill -9 + re-assert
      `set_active_configuration` on every retry — a plain `pkill` without `-9` reliably lost the
      race in testing) and the stale-session-on-open case. Verified live: `fleet vendor-read 0xD053`
      returns the same `01` byte the Python prototype got.
- [ ] Extend `src/ptp_usb.rs` to `0x9413`/`0x90E8`/`0x90EE` writes — all three confirmed working over
      USB 2026-09-06 with the corrected wire formats (see above), but not yet ported into `fleet`;
      deferred until the blob layouts below get a proper decode pass, since a write API needs to
      construct arbitrary field content, not just replay/substitute into a captured blob
- [ ] Fully decode the `0x9413` data-blob layout (14 fields, `[uint32 len incl. null][utf8+null]`
      each per the original notes, but there's a ~18-byte preamble before the first field that
      doesn't fit that model cleanly — the 2026-09-06 test replayed a captured blob byte-for-byte
      rather than fully understanding it). Needs either a second capture with deliberately-varied
      single-field test values to isolate header from content, or careful diffing against more
      `0x9413` instances if any other capture has them
- [ ] Fully decode the `0x90EE` FTP-profile blob (mixed ASCII/UTF-16LE fields, some 4-byte-length-
      prefixed with no null terminator unlike `0x9413`'s convention; SSID/host/port/username/
      password locations found by content search, but the ~100-byte prefix before them and the
      IPv6-placeholder tail aren't understood yet) — same "vary one field, diff the blobs" approach
      as `0x9413` would help
- [x] This Z6III's FTP profile was mutated to username `FLEET`/password `87654321` on 2026-09-06,
      visually verified via the camera's Setup Menu, then reverted back to `fleet`/`12345678` via a
      second `0x90EE` write in the same session — see above for the "needs a preceding `0x90E8` on
      the second write" wrinkle this surfaced
- [ ] Confirm whether the `0x90E8`-before-`0x90EE` requirement is strictly "every write after the
      first" or something narrower (time-based cooldown, write-count-based, etc.) — only tested
      first-skips-it/second-needs-it once each
- [ ] Given `0x9413`/`0x90E8`/`0x90EE`'s failures were all wire-format bugs, not real gating: audit whether
      any *other* vendor op previously assumed "WiFi-only" in this doc was also just mis-transcribed,
      before relying on such claims elsewhere
