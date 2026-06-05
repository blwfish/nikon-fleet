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
