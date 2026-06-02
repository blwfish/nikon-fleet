# Spec: Firmware Archive + Rollback
# nikon-fleet — Rev 2 (2026-06-02)

---

## 1. Goals and Non-Goals

### Goals (this release)

- Read firmware version and serial number from the USB device descriptor rather than the Nikon SDK, which returns empty strings for both on all tested models.
- Archive Nikon firmware binary files (.bin) so any previously installed version can be retrieved and re-flashed manually.
- Associate a settings snapshot with each archived firmware version ("canonical snapshot"), so rollback restores both firmware and settings together.
- Provide a guided rollback procedure: a bundle directory containing the firmware binary, the canonical snapshot, and step-by-step instructions including the manual flash step.
- Detect per-camera firmware version changes across the fleet (`fleet firmware check`).
- Annotate diffs that cross a firmware version boundary to distinguish schema-driven changes (capabilities added/removed by firmware) from user-driven setting changes.

### Non-Goals (explicitly deferred)

- **Settings push / write-back.** The SDK exposes no `SetCapability` FFI entry point. The rollback bundle includes a human-readable settings table for manual restoration. Tagged as future work (§9).
- **Automatic firmware download.** The user supplies the .bin file; the tool manages it once provided.
- **Cross-model rollback.** Rollback is per camera body (model + serial).
- **Firmware binary validation.** The tool treats .bin files as opaque blobs and does not parse Nikon firmware internals.

### Pre-requisite (separate bug)

The Nikon SDK's `EnumDevices` fails on all calls after the first invocation — all four fleet bodies are invisible to the SDK despite being visible to macOS. Root cause is almost certainly a PTP exclusive-session not released between process invocations. **This must be fixed before `fleet snapshot` and `fleet check` are reliable**, and therefore before the archive feature is useful in practice. Implementation of the archive should proceed in parallel, but integration testing requires the session bug to be resolved first.

---

## 2. Fleet Model Registry

Probed 2026-06-02. Source of truth for USB product names, firmware encoding, and schema keys.

| Label  | USB Product Name    | idProduct | bcdDevice  | Firmware | Serial        |
|--------|---------------------|-----------|------------|----------|---------------|
| Z 5    | `NIKON DSC Z 5`     | `0x0448`  | `0x0143`   | `1.43`   | `0000003021501` |
| Z 6III | `NIKON DSC Z6_3`    | `0x0454`  | `0x0200`   | `2.00`   | `0000003014527` |
| Z 9 A  | `NIKON DSC Z 9`     | `0x0450`  | `0x0531`   | `5.31`   | `0000003023668` |
| Z 9 B  | `NIKON DSC Z 9`     | `0x0450`  | `0x0531`   | `5.31`   | `0000003029610` |

---

## 3. Camera Identity Sourcing

This section replaces the current SDK-based identity reads with USB descriptor reads.

### 3.1 Model identifier

The canonical model identifier is the USB product string with the `"NIKON DSC "` prefix stripped:

```
"NIKON DSC Z 9"  →  "Z 9"
"NIKON DSC Z6_3" →  "Z6_3"
"NIKON DSC Z 5"  →  "Z 5"
```

This stripped name matches MaidLayerConfig keys exactly. The SDK `DeviceInfo.name` appears to return the same string and may be used as a fallback, but stripping the USB product string is the primary source.

Naming is inconsistent across Nikon generations: Z 5 and Z 9 use `"Z N"` (letter-space-digit); Z 6III uses `"Z6_3"` (no space, underscore for roman numeral suffix). Do not attempt programmatic normalization — use a lookup table if a human-readable display name is needed.

### 3.2 Serial number

Read from the USB `iSerialNumber` string descriptor. In macOS this appears as `"USB Serial Number"` in `ioreg` output. Example: `"0000003021501"`.

Replace the current `format!("id-{}", target.id)` fabrication everywhere in `main.rs`. The fabricated serial is non-deterministic when multiple bodies are connected simultaneously.

Implementation: add `fn usb_serial_for_device(vendor_id: u16, product_id: u16) -> Option<String>` using IOKit or the `rusb` crate. If unavailable, fall back to `id-{n}` with a warning.

### 3.3 Firmware version

Read from USB `bcdDevice` field, BCD-decoded to a version string.

**BCD decoding:**

```
bcdDevice (u16) → hex → interpret each nibble as a decimal digit

Example: 1329 decimal = 0x0531
  high byte = 0x05 → "5"
  low byte  = 0x31 → tens=3, ones=1 → "31"
  result    = "5.31"

Example: 323 decimal = 0x0143
  high byte = 0x01 → "1"
  low byte  = 0x43 → tens=4, ones=3 → "43"
  result    = "1.43"
```

```rust
fn bcd_decode_version(bcd: u16) -> String {
    let major_tens = (bcd >> 12) & 0xF;
    let major_ones = (bcd >> 8) & 0xF;
    let minor_tens = (bcd >> 4) & 0xF;
    let minor_ones = bcd & 0xF;
    let major = major_tens * 10 + major_ones;
    let minor = minor_tens * 10 + minor_ones;
    format!("{}.{:02}", major, minor)
}
```

SDK `DeviceInfo.version` returns `""` on all tested models. Do not use it. If `bcdDevice` is zero or unavailable, store firmware as `""` and warn.

### 3.4 Reading USB fields in Rust

Options in priority order:

1. **`rusb` crate** — cross-platform, access `bcdDevice` and string descriptors from `DeviceDescriptor`. Adds one dependency.
2. **IOKit via `core-foundation` / `io-kit-sys` crates** — macOS-only but avoids `rusb`. More complex.
3. **Shell fallback** — `system_profiler SPUSBDataType` parsed for vendor `0x04b0`. Fragile but zero new dependencies. Acceptable only as a temporary measure.

Filter USB devices to Nikon vendor ID `0x04b0` before reading any fields.

---

## 4. Archive Directory Layout

All firmware-related storage lives under `<data-dir>/firmware/`, parallel to `snapshots/` and `references/`.

```
<data-dir>/
├── snapshots/
├── references/
└── firmware/
    └── {model_slug}/          # model identifier with spaces → underscores, e.g. Z_9
        └── {version}/         # bcdDevice-decoded string, e.g. 5.31
            ├── metadata.json
            └── firmware.bin   # original .bin, renamed on archive
```

`{model_slug}` is the canonical model identifier (§3.1) with spaces replaced by underscores. Examples: `Z_9`, `Z6_3`, `Z_5`.

`{version}` is the bcdDevice-decoded firmware version string, e.g. `5.31`, `2.00`, `1.43`.

### metadata.json

```json
{
  "format_version": 1,
  "model": "Z 9",
  "firmware_version": "5.31",
  "archived_at": "2026-06-02T14:30:00Z",
  "archived_by": "fleet firmware add",
  "bin_sha256": "e3b0c44...",
  "bin_size_bytes": 104857600,
  "source_path": "/Volumes/SD/NIKON/FIRMWARE/Z9FW0531.BIN",
  "canonical_snapshot_path": "snapshots/Z_9_0000003023668_baseline_20260602T143000Z.json",
  "notes": null
}
```

| Field | Type | Notes |
|---|---|---|
| `format_version` | `u32` | Must be `1`; checked on load |
| `model` | `String` | Canonical identifier (§3.1), not slugified |
| `firmware_version` | `String` | bcdDevice-decoded, e.g. `"5.31"` |
| `archived_at` | `String` | RFC 3339 UTC |
| `archived_by` | `String` | Fixed literal `"fleet firmware add"` |
| `bin_sha256` | `String` | Lowercase hex SHA-256 of `firmware.bin` |
| `bin_size_bytes` | `u64` | |
| `source_path` | `String` | Original path supplied by user; informational |
| `canonical_snapshot_path` | `Option<String>` | Relative to `<data-dir>`; `null` until `fleet firmware pin` |
| `notes` | `Option<String>` | Free-form |

`canonical_snapshot_path` is stored relative to `<data-dir>` (not absolute) so the archive survives being moved. This matches how the rest of the codebase resolves snapshot paths.

---

## 5. Data Model Changes

### New file: `src/firmware.rs`

```rust
pub const FIRMWARE_META_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirmwareMeta {
    pub format_version: u32,
    pub model: String,
    pub firmware_version: String,
    pub archived_at: String,
    pub archived_by: String,
    pub bin_sha256: String,
    pub bin_size_bytes: u64,
    pub source_path: String,
    pub canonical_snapshot_path: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Error)]
pub enum FirmwareError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported format_version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("firmware archive already exists for {model} {version} at {path}")]
    AlreadyArchived { model: String, version: String, path: String },
    #[error("no firmware archive found for {model} {version}")]
    NotFound { model: String, version: String },
    #[error("SHA-256 mismatch: stored={stored}, computed={computed}")]
    ChecksumMismatch { stored: String, computed: String },
}

pub fn bcd_decode_version(bcd: u16) -> String { ... }   // §3.3
pub fn model_slug(model: &str) -> String { ... }         // spaces → underscores
pub fn model_from_usb_product(usb_product: &str) -> &str { ... }  // strip "NIKON DSC "
```

### Changes to `diff.rs` (additive only)

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Diff {
    pub changed: Vec<Change>,
    pub only_in_a: Vec<String>,
    pub only_in_b: Vec<String>,
    pub firmware_annotation: Option<FirmwareAnnotation>,  // NEW
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FirmwareAnnotation {
    pub firmware_a: String,
    pub firmware_b: String,
    pub schema_only_in_a: Vec<String>,  // subset of only_in_a — schema-driven removal
    pub schema_only_in_b: Vec<String>,  // subset of only_in_b — schema-driven addition
    pub schema_changed: Vec<String>,    // subset of changed[].name — allowed_ops differs
}
```

New function (existing `diff()` unchanged):

```rust
pub fn diff_with_schema(
    a: &Snapshot,
    b: &Snapshot,
    opts: &DiffOptions,
    schema: &MaidLayerConfig,
) -> Result<Diff, DiffError>
```

### No changes to existing structs

`Snapshot`, `Camera`, `Change`, `DiffOptions` are unchanged. `Camera.firmware` now receives the bcdDevice-decoded string at capture time (previously `""`).

---

## 6. CLI Commands

New subcommand group added to the `Cmd` enum: `Firmware(FirmwareArgs)`.

### `fleet firmware add`

```
fleet firmware add <bin_path> --model <MODEL> --version <VERSION> [--notes <TEXT>] [--force]
```

1. SHA-256 the file.
2. Create `<data-dir>/firmware/{model_slug}/{version}/`.
3. Copy `<bin_path>` → `firmware.bin`.
4. Write `metadata.json` with `canonical_snapshot_path: null`.
5. Error if archive already exists unless `--force`.

```
Archived Z 9 firmware 5.31
  bin:    firmware/Z_9/5.31/firmware.bin  (98.4 MiB)
  sha256: e3b0c44...
  Hint: run `fleet firmware pin --model "Z 9" --version 5.31 <snapshot>` to associate settings.
```

---

### `fleet firmware ls`

```
fleet firmware ls [--model <MODEL>]
```

Walk `<data-dir>/firmware/`, print table sorted by model then version. Flag missing canonical snapshot.

```
Z 9
  5.31   2026-06-02   98.4 MiB   canonical: Z_9_0000003023668_baseline_20260602T143000Z.json
  4.00   2026-01-10   97.1 MiB   canonical: (none)  ← pin needed
Z6_3
  2.00   2026-05-20   84.2 MiB   canonical: Z6_3_0000003014527_fw200_20260520T090000Z.json
Z_5
  1.43   2026-03-01   71.0 MiB   canonical: (none)  ← pin needed
```

---

### `fleet firmware pin`

```
fleet firmware pin <snapshot_path> --model <MODEL> --version <VERSION>
```

Marks a snapshot as the canonical settings for a given firmware version.

1. Load snapshot. Verify `snapshot.camera.model == model`. Warn (not error) if `snapshot.camera.firmware != version` — the snapshot's firmware field may be empty on older captures.
2. Load `metadata.json` for `(model, version)`. Error if no archive entry.
3. Write `canonical_snapshot_path` (relative to `<data-dir>`) into `metadata.json`.

The firmware mismatch is a warning rather than an error because existing snapshots were captured before §3.3 was implemented and have `firmware: ""`. A `--force` flag is not needed; the warning is sufficient.

---

### `fleet firmware rollback`

```
fleet firmware rollback --model <MODEL> --version <VERSION> [--serial <SERIAL>] [--output-dir <DIR>]
```

1. Load and integrity-check `metadata.json` (SHA-256 verify `firmware.bin`).
2. Warn if `canonical_snapshot_path` is null; continue.
3. Create output dir (default: `<data-dir>/rollback-bundles/{model_slug}_{version}_{timestamp}/`).
4. Copy `firmware.bin` and canonical snapshot into output dir.
5. Generate settings table from canonical snapshot (human-readable, volatile properties excluded, `Capability.display` names from MaidLayerConfig where available).
6. Write `rollback-instructions.txt` (§7).
7. Print bundle path.

```
Rollback bundle for Z 9 → 5.31
  rollback-bundles/Z_9_5.31_20260602T150000Z/
    firmware.bin              (98.4 MiB, sha256 verified)
    canonical_settings.json
    rollback-instructions.txt

Follow rollback-instructions.txt to proceed.
```

---

### `fleet firmware check`

```
fleet firmware check [--serial <SERIAL>]
```

For each connected camera: compare live firmware (from USB `bcdDevice`) to `reference.camera.firmware`. Flag bodies where firmware changed since the reference was set, and bodies whose current firmware has no archive entry.

```
Z 9   0000003023668   fw=5.31   ref-fw=5.31   [OK]       archived
Z 9   0000003029610   fw=5.31   ref-fw=4.00   [CHANGED]  archived
Z6_3  0000003014527   fw=2.00   ref-fw=2.00   [OK]       archived
Z_5   0000003021501   fw=1.43   ref-fw=       [NO REF]   archived
```

---

## 7. Rollback Workflow (rollback-instructions.txt)

```
Rollback Procedure: Z 9 → firmware 5.31
Generated: 2026-06-02T15:00:00Z
Bundle: rollback-bundles/Z_9_5.31_20260602T150000Z/

STEP 1 — VERIFY FIRMWARE FILE
  File:   firmware.bin
  Size:   103,013,376 bytes
  SHA-256: e3b0c44...
  Confirm the file is not corrupt before proceeding.

STEP 2 — COPY TO SD CARD
  Copy firmware.bin to an SD card formatted in the camera.
  Rename to the filename required by the Z 9 firmware update procedure.
  (See camera manual — Nikon requires a specific filename per model;
  the exact name is not encoded in this tool.)

STEP 3 — FLASH
  Insert SD card into camera slot 1.
  MENU → SETUP MENU → Firmware version → Update
  Follow on-screen prompts. Do not power off during flashing.

STEP 4 — VERIFY
  After restart: MENU → SETUP MENU → Firmware version
  Confirm the displayed version is 5.31.
  Or reconnect and run:
    fleet discover
    fleet firmware check

STEP 5 — RESTORE SETTINGS
  (Automatic settings push is not yet implemented — see future work.)
  Restore settings manually using the table below as reference.
  canonical_settings.json contains the full snapshot for archival.

  Key settings from canonical snapshot (non-volatile):
  ─────────────────────────────────────────────────────
  [table generated at bundle time: Capability.display → value,
   sorted alphabetically, volatile properties excluded,
   unknown capability names shown as raw kNkMAIDCapability_* symbol]
```

---

## 8. Firmware-Aware Diff Annotation

### Trigger

Annotation is added to `Diff` whenever `a.camera.firmware != b.camera.firmware` and both are non-empty. If either is empty (legacy snapshot), annotation is skipped with no warning.

### Schema section lookup

MaidLayerConfig version strings are **major-version only** (e.g., `"5.0"`). Live firmware strings are full (e.g., `"5.31"`). Match by extracting the integer part before the first dot:

```rust
fn schema_major(firmware_version: &str) -> Option<u32> {
    firmware_version.split('.').next()?.parse().ok()
}
// "5.31" → Some(5) → look for section "5.0"
// "2.00" → Some(2) → look for section "2.0"
```

### Cumulative section model

Schema sections represent capabilities **added at** that major firmware version. The full capability set for firmware N.x is the union of all sections with major version ≤ N.

```
Z 9 firmware 4.x capability set = sections 2.0 ∪ 3.0 ∪ 4.0
Z 9 firmware 5.x capability set = sections 2.0 ∪ 3.0 ∪ 4.0 ∪ 5.0
```

To annotate a diff between firmware 4.x and 5.x:
- `schema_only_in_b` ← capabilities in section `5.0` that are not in sections ≤4
- `schema_only_in_a` ← capabilities present in sections ≤4 that were removed in 5.0 (rare; Nikon rarely removes capabilities)
- `schema_changed` ← capabilities present in both, where `allowed_ops` differs between the two cumulative sets

### Z 5 special case

`Z 5` has only a `[common]` section in MaidLayerConfig — no per-firmware-version sections. If either snapshot is from a Z 5, `firmware_annotation` is set to `None` and no annotation message is shown. (A diff between two Z 5 snapshots on different firmware versions would be unusual anyway given the single `[common]` schema.)

### Unknown schema versions

If `schema_major()` returns a version for which no section exists in MaidLayerConfig (e.g., firmware `"6.00"` for a Z 9 with schema up to `5.0`), emit a warning:

```
Warning: no schema section found for Z 9 firmware 6.00; cross-firmware annotation skipped.
```

Do not silently produce empty annotation.

### Human-readable output

```
! Firmware version boundary: 4.00 → 5.31

Changed (3):
  ExposureDelay                    [schema: allowed_ops changed]
    A: 0
    B: 2

Only in A (1):
  kNkMAIDCapability_OldVideoMode   [schema: removed in 5.x]

Only in B (4):
  AutoCaptureInterval              [schema: added in 5.x]
  ...
```

JSON output: `firmware_annotation` serializes automatically via the existing `--json` flag.

---

## 9. External System Assumptions

| Assumption | Status | Impact if wrong |
|---|---|---|
| `bcdDevice` encodes firmware in BCD on all Nikon models | Verified on Z 5, Z 6III, Z 9 (3/3) | Firmware field wrong; fall back to `""` |
| `iSerialNumber` is stable across reconnects | Verified: same serial across multiple plug cycles | Snapshot identity broken; fall back to `id-{n}` |
| Stripping `"NIKON DSC "` prefix gives MaidLayerConfig key | Verified on all 3 model types | Model lookup fails; needs explicit table |
| MaidLayerConfig section strings match `"{major}.0"` pattern | Verified: `"2.0"`, `"3.0"`, `"4.0"`, `"5.0"` | Version matching fails silently; detected by unknown-version warning |
| Schema sections are cumulative (union model) | Assumed from SDK structure; not confirmed by Nikon docs | Annotation attributes wrong firmware version to capability changes |
| SDK `DeviceInfo.version` is always `""` | Verified on Z 5, Z 6III, Z 9 | — |
| Nikon SD firmware filename is model-specific and not machine-readable | Assumed; not tested | Rollback instructions incomplete |
| SDK has no `SetCapability` entry point | Verified by codebase read | — |

---

## 10. Future Work

### Settings push (`fleet restore`)

```
fleet restore <snapshot_path> [--serial <SERIAL>] [--dry-run]
```

Push non-volatile, writable settings from a snapshot to a live camera via SDK `SetCapability`. The `canonical_snapshot_path` field and `fleet firmware pin` command are designed now so this can be wired in without schema changes. Gate on: (a) SDK session bug fixed, (b) `SetCapability` FFI exposed.

### `fleet firmware rollback --wait`

After the manual flash, poll `fleet discover` until the camera reappears and auto-run `fleet firmware check` to verify the version. Requires no write capability; deferred because the full rollback workflow needs validation on a real camera first.

### Per-model SD firmware filename table

Encode the exact filename Nikon requires on the SD card per model (e.g. `Z9FW0500.BIN`). Allows `rollback-instructions.txt` to give a precise rename instruction instead of directing the user to the manual.

### Automatic firmware acquisition

Nikon distributes firmware through the Download Center web interface with no machine-readable API. Out of scope.

### Survey remaining fleet models

Only Z 5, Z 6III, and Z 9 were probed. If other models (Z 8, Z 6II, Z 7II, etc.) join the fleet, run the USB descriptor probe and add rows to §2.

---

## 11. Design Decisions (resolved)

| Decision | Resolution |
|---|---|
| Canonical snapshot: symlink vs path in metadata | Path in metadata (portable if `--data-dir` moves) |
| `fleet firmware pin` firmware mismatch | Warning, not error (existing snapshots have empty firmware field) |
| Firmware version source | USB `bcdDevice`, BCD-decoded |
| Serial number source | USB `iSerialNumber` |
| Schema version matching | Extract integer major; match `"{major}.0"` section |
| Z 5 annotation | Skip gracefully (no per-version schema sections) |
| Rollback bundle naming collision | Timestamp in default dir name; no `--force` needed |
