# nikon-fleet — backlog

## PTP/IP capture & NX Field reverse engineering

**Goal:** Capture Wireshark traffic from an NX Field session to discover
vendor-private PTP property codes (0xDxxx) and operations (0x9xxx) that
are absent from the MAID SDK — e.g. file naming, folder structure, FTP
transfer settings, AF fine-tune, copyright fields.

Secondary goal: use the same capture setup to debug NX Field weirdness
(connection drops, unexpected behaviour, whatever surfaces).

### Setup options

**Ethernet (cleanest, Z8/Z9 only):**
- Camera ethernet → Mac ethernet, static IPs (camera 192.168.1.2, Mac 192.168.1.1)
- Capture on the ethernet interface

**USB LAN (no extra hardware):**
- Camera USB → Mac, camera in USB LAN mode
- Find the virtual interface: `ifconfig | grep -A4 POINTTOPOINT`
- Wireshark captures on it directly

### Wireshark display filters

```
ptpip                                               # all PTP/IP traffic
ptpip.code == 0x1016                                # SetDevicePropValue (writes)
ptpip.code == 0x1014                                # GetDevicePropDesc  (enum defs)
ptpip.code >= 0x9000 && ptpip.code <= 0x9fff        # vendor-only operations
```

### Correlation method

1. Connect NX Field, let it enumerate (baseline)
2. Change one setting at a time in NX Field; note timestamp
3. Find the `SetDevicePropValue` that fired — first uint32 in payload is the
   property code
4. `GetDevicePropDesc` for same code gives allowed values, current, default
5. Build a property-code → name table

### Binary strings pass

```bash
strings "/Applications/NX Field.app/Contents/MacOS/NX Field" \
  | grep -E 'kNk|MAID|0x[Dd][0-9a-fA-F]{3}|Prop|Capability' \
  | sort -u
```

Property code constants and string labels are often adjacent in the binary.

### Integration with nikon-fleet

Once we have the code → name mapping, the Rust snapshot walk can call
`GetDevicePropValue` for the additional codes directly — no SDK changes
needed, it's a standard PTP operation already used for MAID-enumerated caps.

### 2026-07-09 raw-USB baseline pass (pre-capture-session prep)

Ran Phase 0/4 of `docs/ptpip-capture-plan-2026-07-10.md` directly over USB
via `gphoto2`/`libgphoto2` — no NX Field/MITM/capture needed for this part.
`libgphoto2`'s Nikon driver already issues `GetDeviceInfo` (0x1001) +
`GetVendorPropCodes` (0x90CA) + loops `GetDevicePropDesc` (0x1014) and
ships names for hundreds of `0xD0xx` codes already. Opcodes/property codes
verified against real `camlibs/ptp2/ptp.h` source (not a summarized
guess): `0x1001`, `0x1014`, `0x90CA`, `0xD053`, `0xD072`, `0xD073`,
`0x501E`, `0x501F` all confirmed.

Baselines saved to `references/` (gitignored): `z9-ptp-baseline`,
`z6iii-ptp-baseline`, `z5-ptp-baseline`, `z8-ptp-baseline`
(`-2026-07-09.txt`). The Z8 belongs to Vic (collaborator), not the fleet —
its Artist/Copyright fields read "VICTOR NEWMAN PHOTOGRAPHY", included in
the comparison below at the user's request.

**Copyright fields resolved without any capture** (answers the Phase 0
"cheap targeted check"): `0xD053`/`0xD072`/`0xD073` (and standard mirrors
`0x501E`/`0x501F`) work identically across all four bodies.

**Per-body unique codes** (present on that body, absent from at least one
other):
- Z9-only vs Z6III: `d013 d040 d043 d05e d07f d0b0 d0b1 d0b2 d0b3 d0f6 d177 d186`
- Z6III-only vs Z9: `d037 d045 d04d d04e d050 d051 d094 d095 d096 d097 d0fc d156 d16a d20d ffec`
- Z5-only vs Z9/Z6III: `501c d045 d09d d0a7 d0fc d131 d149 d15d d16a d17a d197 d1ad d1b7 d1b9 d1f0 d1ff d20d d20e d235`
  (Z5 is the simplest body — 280 codes total vs. ~330 for the others)
- Codes present on all four bodies: 261; union across the fleet: 359

**Actual remaining gap — `[Unknown Property]` codes libgphoto2 has no name
for** (this is the real target for tomorrow's NX Field capture, not the
codes above which libgphoto2 already names):
```
d000 d001 d002 d005 d006 d007 d008 d009 d00a d00b d00c d00e d00f d060
d094 d095 d096 d097 d098 d099 d09a d09b d0b0 d0b1 d0bd d0cd d0f6 d119
d12f d19c d1c5 d1c6 d1ff d259 d25a d25b d406 d407
```

**Open anomaly to explain, not yet understood:** every body's summary
lists a handful of `0xD0xx` codes (e.g. `d053`, `d073`) *twice* — once from
the `GetDeviceInfo` properties-supported list, again later (presumably
from `GetVendorPropCodes`). Most duplicates return identical values both
times, but on the Z9 some codes that resolved fine on the first pass
returned PTP error `200a` (device-prop-not-supported) on the second pass,
same live camera state. Unclear if this is a `libgphoto2` driver artifact
(querying the same code via two different capability-list contexts) or a
genuine state-dependent property-availability quirk worth knowing about
for the capture session.

---

## MaidLayer resource strings (elem_type 2 labels)

Parse the `<resourcestringList>` sections in MaidLayer.config to get
camera-menu string labels for the 68 `elem_type 2` integer-code enum
capabilities (e.g. `ExposureMode`: 2 → "A (Aperture priority)").

Required for:
- Showing full labels in the snapshot detail view (currently shows raw ints)
- The editing/bundle-creation UI (option pickers)

---

## Menu-order sort in snapshot detail view

Sort capabilities by their position in the camera's physical menu structure
rather than alphabetically. Order data lives in MaidLayer.config.
Natural companion to the resource-string pass above.

---

## Editing increment (change values + create settings bundles)

After resource strings and menu order are in place: build the ability to
modify capability values and save a new settings bundle using
camera-native terms (same labels the camera shows).

---

## Z8 USB LAN detection

`fleet discover` says "no cameras detected" when a camera is in USB LAN
mode. Should detect Nikon devices in non-PTP USB modes and print a helpful
message ("Camera detected in USB LAN mode — switch to MTP/PTP to use
nikon-fleet").
