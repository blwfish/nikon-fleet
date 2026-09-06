# Handoff: test whether NX Field's WiFi-only vendor PTP ops are also reachable over raw USB PTP

**For:** a fresh Claude Code session on the machine with a Z6III plugged in via USB.
**Origin:** written 2026-09-06 from a session running 1000 miles from the camera — this task
needs to run where the hardware is.

## Context

Read [nx-field-session-2026-07-09.md](nx-field-session-2026-07-09.md) (esp. "MAID / SDK / PTP-IP
relationship") and [todo.md](todo.md) lines 58-105 before starting.

Short version: MAID (`src/sdk.rs`) is confirmed USB-only — it does `rusb` enumeration and never
touches WiFi. NX Field exposes settings MAID doesn't have (FTP transfer profiles, IPTC metadata
profiles, file naming) over PTP/IP-over-WiFi using vendor opcodes:

- `0x9413` — IPTC profile write
- `0x90E8` / `0x90EE` — FTP profile status / write
- `0x943B` — generic vendor-property read wrapper, param = `0x10000 | propcode`

Standard `0xD0xx` *properties* are already proven reachable over raw USB PTP independent of
MAID — the 2026-07-09 `libgphoto2` baseline pass did exactly this and pulled hundreds of them.
But nobody has ever tried sending these specific vendor *operations* over USB. All of the NX
Field capture work was WiFi-only. That's the open question this session should answer.

**Camera available:** a Z6III on USB.

## Do this, in order — stop and report back after each step

1. Implement a minimal raw PTP-over-USB test harness (Bulk-Only Transport / USB Still Image
   class container framing — 4-byte length, 2-byte type, 2-byte code, 4-byte transaction ID, up
   to 5 param words). Python + `pyusb` is fine. Find the Nikon interface (VID `0x04b0`) and its
   bulk IN/OUT endpoints from descriptors. Expect to need `killall PTPCamera` (or disable Image
   Capture) first — macOS's own PTP claim on the device will block libusb otherwise, same issue
   `gphoto2` users hit.

2. `OpenSession` (`0x1002`), then send **`0x943B` with param `0x10000 | 0xD0B4`** (or substitute
   any `0xD0xx` code already confirmed present on the Z6III per the per-body table in
   [todo.md](todo.md)) — this one is read-only by construction regardless of support, so it's the
   safest first probe. Report the response code (`OK` 0x2001 vs `OperationNotSupported`
   0x2005/0x2006/etc.) and any returned data.

3. Only if that succeeds, try `0x90E8` alone (per the session notes it was "seen during
   read-only status checks" on WiFi, so it may also be read-safe) — again just report the
   response code.

4. **Do not** attempt `0x9413` (IPTC write) or `0x90EE` (FTP profile write) over USB in this
   pass — those mutate camera state, and there's no established rollback/verification path for
   this transport yet. Stop and report after step 3 regardless of outcome.

## Goal

Answer: is the camera's PTP command dispatcher transport-agnostic for these vendor ops, or are
they genuinely gated to the WiFi/PTP-IP session context? Either answer changes how much WiFi
infrastructure (`docs/nx-field-session-2026-07-09.md`'s "PTP/IP adjunct client" open item) is
actually required before the reverse-engineered settings become usable from `fleet`.

Once this is answered, fold the result back into `nx-field-session-2026-07-09.md` and/or
`todo.md`, and delete this handoff file.
