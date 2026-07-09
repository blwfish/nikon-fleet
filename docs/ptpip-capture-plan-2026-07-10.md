# PTP/IP Capture Session Plan — 2026-07-10 (with Vic)

Builds on `docs/ptpip-capture-notes.md` (2026-06-12 session: MITM setup, mDNS
discovery, NX Field TXT record decoding). This doc only covers what's new:
sequencing for tomorrow, not a repeat of the capture infrastructure.

## Phase 0 — before Vic arrives, no capture needed

Raw-PTP enumeration pass directly against the Z9, no NX Field/MITM/Vic required:

1. `GetDeviceInfo` (0x1001) → `DevicePropertiesSupported` array.
2. Nikon's `GetVendorPropCodes` (0x90CA — **verify this byte against a real
   `ptp.h`/capture before trusting it**, it came from a summarized fetch, not
   a direct grep).
3. Loop `GetDevicePropDesc` (0x1014) over every returned code; dump current
   value + allowed range/enum for each to a baseline file
   (`{code: {value, allowed_range}}`).

Cheap targeted check within that same pass: query `0xD072` (Artist Name),
`0xD073` (Copyright Information), `0xD053` (Enable Copyright) specifically.
If the Z9 answers, copyright is solved before the session starts — no
capture needed for it at all.

## Phase 1 — capture setup

Per `docs/ptpip-capture-notes.md`: bettercap ARP-MITM + tcpdump. Disconnect
NX Field before starting MITM, reconnect after.

## Phase 2 — NX Field UI sweep

Walk NX Field's own settings screens one control at a time, noting
timestamp per change (existing correlation method: find the
`SetDevicePropValue` that fired, first uint32 in payload = property code).

This is bounded, not comprehensive — NX Field's own feature surface is AF,
exposure, WB, sync release, FTP transfer. It will not touch Custom Settings
menu items that aren't in its UI at all.

## Phase 3 — physical-body test

With the MITM session still live and NX Field connected, change items
directly on the Z9 body itself — Custom Settings bank items, starting with
**c3 Standby Timer** (the metering timeout), plus anything else not present
in NX Field's own UI. Watch for `DevicePropChanged` (0x4006) events carrying
codes absent from Phase 2's list — this catches settings that can only be
changed at the body, not over the wire.

## Phase 4 — Z6III pass

Repeat Phases 0–3 against the Z6III: get its `pid` (Z9's was `0x0450`) and
confirm whether vendor codes carry the same numbers across bodies. Z6III
didn't advertise mDNS mid-session last time — reconnect fresh before
capturing.

## Phase 5 — post-session diff

- `baseline (Phase 0) − touched (Phases 2+3)` → codes never exercised this
  session (candidates for a future targeted pass, or genuinely dead/unused).
- `touched − already-documented (MAID overlap + known vendor codes)` → the
  actual deliverable: new property-code → name entries for the
  property-code → name table goal in `docs/todo.md`.

## Deferred (not blocking tomorrow)

NX Field binary strings pass (see `docs/ptpip-capture-notes.md` §"Strings
pass on NX Field app").
