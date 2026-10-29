//! `fleet` CLI — settings management for a fleet of Nikon Z cameras.
//!
//! All real logic lives in the `nikon_fleet` library; this file is just
//! the command-line surface.

mod cli_report;
use cli_report::unrestored_error;
// The MCP server's child process; it takes an inherited file descriptor, which Windows has no
// equivalent of here (fleet-mcp is unix-only too).
#[cfg(unix)]
mod worker_cmd;

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use nikon_fleet::camera_lock::CameraLock;
use nikon_fleet::diff::{Diff, DiffOptions, diff_with_schema, is_volatile};
use nikon_fleet::firmware::{
    ArchiveListing, FirmwareError, FirmwareMeta, FIRMWARE_META_FORMAT_VERSION,
    archive_dir, archive_exists, format_size, ingest_bundle, load_meta,
    model_slug, save_meta, scan_archives, sha256_file, IngestOutcome, Provenance, RetrievalMethod,
};
use nikon_fleet::firmware_browse::{
    BrowseError, BrowserWindow, VersionEvidence, browse_firmware, nikon_is_release_page,
};
use nikon_fleet::firmware_source::{
    CameraRelation, DownloadAccess, FetchError, FirmwareSource, FwVersion, ModelStatus,
    NikonDownloadCenter, Release, UreqHttp, camera_relation, fetch_direct,
};
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::ops::cameras::{describe_device, duplicate_models};
use nikon_fleet::ops::capture::{self as ops_capture, CaptureStats};
use nikon_fleet::ops::firmware_latest::{LatestReport, ModelSurvey, survey_json, survey_models, watch_models};
use nikon_fleet::ops::paths::{checked_reference_filename, is_safe_path_component, write_atomic};
use nikon_fleet::ops::list::{EntryState, Listing, list_dir};
use nikon_fleet::ops::labels::{self, Labels};
use nikon_fleet::ops::render;
use nikon_fleet::ops::restore;
use nikon_fleet::ops::target::{
    TargetError, body_firmware, raw_usb_serial_arg, select_by_cli_serial, storable_serial,
};
use nikon_fleet::sdk::{Device, DeviceInfo, Sdk, UsbCameraInfo, pair_devices, usb_camera_list};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use nikon_fleet::transplant;

#[cfg(target_os = "macos")]
const DEFAULT_SDK_BUNDLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/sdk-runtime/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module"
);

// On Windows, ControlServiceLayer.dll is the entry point. The other DLLs
// (NkdPTP.dll, NkRoyalmile.dll, dnssd.dll) must sit alongside it so Windows
// can find them when ControlServiceLayer loads them.
#[cfg(target_os = "windows")]
const DEFAULT_SDK_BUNDLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/sdk-runtime/ControlServiceLayer.dll"
);

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEFAULT_SDK_BUNDLE: &str = "";

const DEFAULT_SCHEMA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/sdk-runtime/MaidLayer.config");

/// Top-level CLI parser. `clap` generates --help, completion, validation,
/// and arg parsing from these derives.
#[derive(Parser, Debug)]
#[command(name = "fleet", about = "Manage settings across a fleet of Nikon Z cameras", version)]
struct Cli {
    /// Base directory for the snapshots/, references/ and firmware/ subdirs.
    /// Defaults to the CURRENT DIRECTORY (`.`), not to the directory the GUIs use: they
    /// read `data_dir` from settings.json, and the CLI does not, so a plain `nikon-fleet
    /// snapshot` writes ./snapshots, which the GUIs never see. To work in the GUIs'
    /// directory pass `--data-dir "$(nikon-fleet settings data-dir)"` (see `nikon-fleet
    /// settings show`).
    #[arg(long, default_value = ".", global = true)]
    data_dir: PathBuf,

    /// Path to the Nikon SDK bundle's executable. Default points at the
    /// project-local sdk-runtime/ (set up by scripts/setup-sdk-runtime.sh).
    #[arg(long, default_value = DEFAULT_SDK_BUNDLE, global = true)]
    sdk_bundle: PathBuf,

    /// Path to MaidLayer.config used to translate numeric capability codes
    /// into kNkMAIDCapability_* symbol names.
    #[arg(long, default_value = DEFAULT_SCHEMA, global = true)]
    schema: PathBuf,

    /// Skip the USB reset before InitializeSDK. Use this when running from a
    /// GUI context (AppKit run loop active) to avoid invalidating Metal surfaces.
    #[arg(long, global = true)]
    no_usb_reset: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List USB-attached cameras.
    Discover(DiscoverArgs),

    /// Capture current settings from a live camera.
    Snapshot(SnapshotArgs),

    /// Manage per-camera reference snapshots.
    #[command(subcommand)]
    Ref(RefCmd),

    /// Internal: run one operation and report it on an inherited file
    /// descriptor (the MCP server's child process). Not for direct use.
    #[cfg(unix)]
    #[command(hide = true)]
    Worker(worker_cmd::WorkerArgs),

    /// Manage the per-model capture baselines that decide whether a capture
    /// is complete (docs/mcp-plan.md §5.5).
    #[command(subcommand)]
    Baseline(BaselineCmd),

    /// Show or change settings.json, which the GUIs read for the data directory (the CLI
    /// does not use it unless you pass it as --data-dir).
    #[command(subcommand)]
    Settings(SettingsCmd),

    /// Diff two snapshot files.
    Diff(DiffArgs),

    /// Diff a live camera against its reference.
    Check(CheckArgs),

    /// List stored snapshots.
    Ls(LsArgs),

    /// Show what a stored snapshot holds. Reads one file; no camera, no SDK.
    #[command(after_help = SHOW_HELP)]
    Show(ShowArgs),

    /// Remove a stored snapshot.
    Rm(RmArgs),

    /// Restore settings from a snapshot to a live camera.
    #[command(after_help = RESTORE_HELP)]
    Restore(RestoreArgs),

    /// Copy settings from a snapshot onto a live camera of a possibly
    /// *different* model, translating enum/range values into the target's
    /// own equivalents instead of assuming the same code/index means the
    /// same thing on both bodies. See src/transplant.rs for the strategy.
    #[command(after_help = TRANSPLANT_HELP)]
    Transplant(TransplantArgs),

    /// Manage the firmware archive.
    #[command(subcommand)]
    Firmware(FirmwareCmd),

    /// Read a vendor-only property (0xD0xx/0x5xxx) over raw USB PTP, bypassing
    /// the MAID SDK. See docs/nx-field-session-2026-07-09.md for background —
    /// these were reverse-engineered from an NX Field WiFi capture and later
    /// confirmed to also work over plain USB.
    VendorRead(VendorReadArgs),

    /// Read the standard PTP DeviceInfo dataset (operation 0x1001) over raw USB
    /// PTP and compare its SerialNumber with the USB iSerialNumber of the same
    /// body. Read-only: it changes nothing on the camera. Uses the raw PTP path
    /// like vendor-read (no --sdk-bundle/MAID SDK needed).
    #[command(after_help = DEVICE_INFO_HELP)]
    DeviceInfo(DeviceInfoArgs),

    /// Write an FTP profile (0x90EE) over raw USB PTP, bypassing the MAID
    /// SDK. Reverse-engineered the same way as VendorRead — see
    /// docs/nx-field-session-2026-07-09.md for the field map and the parts
    /// of the wire format that are still opaque-but-fixed.
    #[command(after_help = VENDOR_WRITE_HELP)]
    VendorWriteFtp(VendorWriteFtpArgs),
}

#[derive(Args, Debug)]
struct DiscoverArgs {
    /// Output camera list as JSON (for GUI / scripting): `{"cameras": [...]}`, each with
    /// `model`, `serial` (the identity snapshots are stored under: the USB serial, or `id-N`
    /// when none could be read), `firmware`, `sdk_id`, `usb_serial` (null when none),
    /// `known_model` and `baseline` (a capture baseline ships for the model).
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct SnapshotArgs {
    /// Camera serial to target. (Used to disambiguate when multiple bodies are attached.)
    #[arg(long)]
    serial: Option<String>,
    /// Optional label, e.g. "wedding-baseline". May start with `-` (`--label -guest`).
    #[arg(long, allow_hyphen_values = true)]
    label: Option<String>,
    /// Save the capture even if it is missing capabilities the model's
    /// baseline reads (a body in an unusual state, e.g. no card). Without
    /// this an incomplete capture is refused and nothing is written.
    #[arg(long)]
    allow_partial: bool,
    /// Print the result as JSON on stdout (the same value the MCP worker's `capture` returns);
    /// progress and warnings go to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum BaselineCmd {
    /// Seed the baseline for a snapshot's model from that (known-good)
    /// snapshot: its properties become the set of capabilities every later
    /// capture of the model must be able to read. Rebuild afterwards: baselines
    /// are embedded into the binary.
    Seed {
        snapshot_path: PathBuf,
        /// Directory to write `{model}.json` into.
        #[arg(long, default_value = "baselines")]
        out_dir: PathBuf,
        /// Replace an existing baseline for this model (needed after a
        /// firmware update). Refuses a replacement that stops requiring a capability the
        /// existing baseline reads (see --allow-shrink).
        #[arg(long)]
        force: bool,
        /// With --force: replace the existing baseline even though the new one lists fewer
        /// capabilities, or the existing file cannot be read to compare. A smaller baseline
        /// passes captures the old one would fail, so say so deliberately.
        #[arg(long, requires = "force")]
        allow_shrink: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SettingsCmd {
    /// Print where settings.json is, what it says and the directory in use.
    Show {
        /// One JSON object: settings_path, settings_exist, data_dir (null when unset),
        /// effective_data_dir, keys (every key in the file).
        #[arg(long)]
        json: bool,
    },
    /// Print the directory the GUIs use (settings.json's data_dir, else the default), and
    /// nothing else: `nikon-fleet --data-dir "$(nikon-fleet settings data-dir)" ls`.
    DataDir,
    /// Set the data directory the GUIs use (an absolute path), keeping every other key in
    /// settings.json. A settings.json that cannot be parsed is left alone and reported.
    SetDataDir {
        /// The directory (absolute). Omit with --unset.
        dir: Option<PathBuf>,
        /// Clear the setting so the default is used.
        #[arg(long, conflicts_with = "dir")]
        unset: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RefCmd {
    /// Promote a stored snapshot to be the reference for its camera. Replaces the camera's
    /// current reference (and says so); refuses a snapshot whose model or serial is not a
    /// plain file-name component.
    Set { snapshot_path: PathBuf },
    /// Show the reference snapshot for a (model, serial).
    Show { model: String, serial: String },
    /// List all references.
    List {
        /// Output as JSON (one object: dir, dir_exists, entries with each
        /// reference's model/serial/firmware/label, skipped_dir_entries).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
struct DiffArgs {
    snapshot_a: PathBuf,
    snapshot_b: PathBuf,
    /// Include volatile properties (battery level, temperature, etc.).
    #[arg(long)]
    include_volatile: bool,
    /// Output as JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct CheckArgs {
    #[arg(long)]
    serial: Option<String>,
    /// Compare even if the live capture is missing capabilities the model's
    /// baseline reads. The diff will then show them as removed.
    #[arg(long)]
    allow_partial: bool,
    /// Print the result as JSON on stdout (the same value the MCP worker's `check` returns);
    /// progress and warnings go to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct LsArgs {
    /// Limit to snapshots for a specific camera, in "{model}_{serial}" form.
    #[arg(long)]
    camera: Option<String>,
    /// Output as JSON (one object: dir, dir_exists, entries, skipped_dir_entries).
    /// An unreadable file is listed with status "unreadable" and an error_kind.
    #[arg(long)]
    json: bool,
}

const SHOW_HELP: &str = "\
Reads one snapshot or reference file and prints each property as `name = value`, sorted by name,
with the kNkMAIDCapability_ prefix left off. Nothing touches a camera or the SDK.

Names come from files in sdk-runtime/ (next to --schema): the SDK's display name for a property
(\"Mode A\" for ModeA), the name of an enumeration's current value (\"Label (3)\" rather than 3) and
a range's scale (\"612.0 (0..1224, 1225 steps)\"). MaidLayer.config supplies the display names and
some value names; scripts/extract-maid-labels.py writes maid-labels.json with the base
capabilities' names (what a cap_0x.. key is called) and the value names the SDK's PDF documents. A value
that its property's list of names does not have is marked [value_not_in_labels]. Without these
files the numbers are shown as they are and a note on stderr says what is missing.
--codes adds the [0xCODE] column (--filter finds codes either way); --no-names drops the display-name
column; --raw drops every name and scale and prints the numbers with the code column, as before.

SNAPSHOT_FILE is a path, or a bare file name (as `ls` prints it) looked up in snapshots/ and then
references/ of --data-dir, and failing that in the current directory.

Values that are missing or unusable are shown as such, never as a default: `(missing)`,
`(malformed: why)`. --filter keeps the properties whose name, shown value, hex code (0xd053 or
d053) or decimal code contains TEXT, ignoring case. --json prints the whole result (header,
rows with a kind and notes per value, filtered_out, truncated) as one JSON document.

Exit status: 0 shown, 1 the file is missing or not a valid snapshot, 2 bad arguments.";

#[derive(Args, Debug)]
struct ShowArgs {
    /// A snapshot or reference file: a path, or a bare file name from snapshots/ or references/.
    snapshot_file: PathBuf,
    /// Only the properties whose name, value or code contains TEXT (ignoring case).
    #[arg(long, value_name = "TEXT")]
    filter: Option<String>,
    /// Print the result as one JSON document instead of text.
    #[arg(long)]
    json: bool,
    /// Add the [0xCODE] column.
    #[arg(long)]
    codes: bool,
    /// Show the SDK's display name next to the internal name (the default when the names are available).
    #[arg(long, conflicts_with_all = ["no_names", "raw"])]
    names: bool,
    /// Do not show display names.
    #[arg(long)]
    no_names: bool,
    /// No names, labels or scales: the numbers as stored, with the code column.
    #[arg(long)]
    raw: bool,
}

#[derive(Args, Debug)]
struct RmArgs {
    snapshot_path: PathBuf,
}

#[derive(Subcommand, Debug)]
enum FirmwareCmd {
    /// Archive a firmware binary file.
    #[command(after_help = ALREADY_ARCHIVED_HELP)]
    Add(FirmwareAddArgs),
    /// List archived firmware versions.
    Ls(FirmwareLsArgs),
    /// Associate a snapshot as the canonical settings for a firmware version.
    Pin(FirmwarePinArgs),
    /// Generate a rollback bundle for a given firmware version.
    Rollback(FirmwareRollbackArgs),
    /// Compare live camera firmware against references and check archive coverage.
    Check(FirmwareCheckArgs),
    /// Ask the firmware source what is newest for each model, and compare with
    /// the archive and the connected cameras.
    Latest(FirmwareLatestArgs),
    /// Archive newly published firmware (or ingest a bundle you downloaded
    /// yourself with --from-file) with provenance.
    #[command(after_help = ALREADY_ARCHIVED_HELP)]
    Fetch(FirmwareFetchArgs),
    /// Open the release's download page in an embedded browser so YOU accept
    /// Nikon's licence and click Download; the file is captured, checked
    /// against the page's version and archived with provenance. Needs a build
    /// with `--features webview`; otherwise prints the manual steps.
    #[command(after_help = ALREADY_ARCHIVED_HELP)]
    Browse(FirmwareBrowseArgs),
}

/// Process exit status for "this model and version is already in the archive and nothing was
/// changed": `firmware add` without `--force`, or a fetch/browse whose bytes differ from the
/// archived ones. A script or GUI tests for it instead of reading the message.
const EXIT_ALREADY_ARCHIVED: i32 = 3;

const ALREADY_ARCHIVED_HELP: &str = "\
Exit status:
  0  done (identical bytes that are already archived count as done)
  1  any other error
  2  command-line usage error
  3  refused: that model and version is already in the archive, nothing was changed
     (`add` without --force; `fetch`/`browse` when the bytes differ from the archived ones;
     with several models, 3 only when every failure was this)";

/// Process exit status for a `vendor-write-ftp` that failed after the profile data had been sent
/// to the camera: it may or may not have changed (`PtpUsbError::FtpWriteIndeterminate`). Distinct
/// from the plain failure (1) a script is allowed to retry blindly: this one needs a look at the
/// camera first.
const EXIT_WRITE_INDETERMINATE: i32 = 4;

const TRANSPLANT_HELP: &str = "\
Exit status:
  0  done (a dry run, or an apply that wrote and read back what it planned)
  1  any other error, or an apply that wrote but did not fully take (the JSON report says which).
     An apply that ends 1 with no JSON report may have written
  2  command-line usage error
  5  --expect-digest was given and the plan no longer matches: nothing was written
  6  an apply (not a dry run) was refused before it could write anything: nothing was written
With --json, a refusal's message leaves out the advice to pass --allow-partial: a program reading it
cannot act on that.";

const RESTORE_HELP: &str = "\
Exit status:
  0  done (a dry run, or a restore that wrote and read back what it planned)
  1  any other error, or a restore that wrote but did not fully take (the JSON report says which).
     A restore that ends 1 with no JSON report may have written
  2  command-line usage error
  5  --expect-digest was given and the plan no longer matches: nothing was written
  6  a restore (not a dry run) was refused before it could write anything: nothing was written
With --json, a refusal's message leaves out the advice to pass --allow-partial: a program reading it
cannot act on that.";

const DEVICE_INFO_HELP: &str = "\
Read-only: sends only the standard GetDeviceInfo (0x1001) inside a PTP session and changes nothing
on the camera. The last line compares the PTP SerialNumber with the USB iSerialNumber:
  serial: PTP \"<x>\" | USB \"<y>\" | match|differ|ptp_empty|usb_empty|both_empty
  usb_empty means the descriptor has no serial but DeviceInfo does (the body could be identified by it).
With --json the same comparison is the serial_comparison object.
Exit status: 0 read and printed; 1 any error (camera busy, no such camera, malformed dataset); 2 usage error.";

const VENDOR_WRITE_HELP: &str = "\
Exit status:
  0  the camera accepted the profile (see the note printed after it, if any)
  1  any other error; nothing was sent to the camera unless the message says so
  2  command-line usage error
  4  the profile data was sent but the write then failed: the camera MAY have changed.
     The profile cannot be read back over USB; check it in the camera's menu before retrying";

#[derive(Args, Debug)]
struct FirmwareBrowseArgs {
    /// Model whose newest firmware to download, e.g. "Z 9" or "Z6_3".
    model: String,
}

/// Models checked when no --model is given: the fleet bodies that have
/// newer firmware to watch (Z 5 is supported by the source but not default).

#[derive(Args, Debug)]
struct FirmwareLatestArgs {
    /// Model to look up, e.g. "Z 9" or "Z6_3". Repeatable. Default: Z 9 and Z6_3.
    #[arg(long = "model")]
    models: Vec<String>,
    /// Output the survey as JSON (for GUI / scripting). Exit status is the
    /// same as without it: non-zero if any model could not be determined.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct FirmwareFetchArgs {
    /// Model to fetch, e.g. "Z 9" or "Z6_3". Repeatable. Default: Z 9 and Z6_3.
    /// Exactly one is required with --from-file.
    #[arg(long = "model")]
    models: Vec<String>,
    /// Report what would be downloaded/archived; write nothing.
    #[arg(long)]
    dry_run: bool,
    /// The firmware file you downloaded yourself from the release's download
    /// page (after accepting Nikon's licence there). Archived under the latest
    /// published version for --model, with provenance and the page saved.
    #[arg(long)]
    from_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct FirmwareAddArgs {
    /// Path to the firmware .bin file to archive.
    bin_path: PathBuf,
    /// Camera model, e.g. "Z 9".
    #[arg(long)]
    model: String,
    /// Firmware version string, e.g. "5.31".
    #[arg(long)]
    version: String,
    /// Optional free-form notes. May start with `-`.
    #[arg(long, allow_hyphen_values = true)]
    notes: Option<String>,
    /// Overwrite an existing archive entry.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct FirmwareLsArgs {
    /// Limit to a specific model, e.g. "Z 9".
    #[arg(long)]
    model: Option<String>,
    /// Output as JSON (one object: dir, dir_exists, entries, skipped_dir_entries). Each valid
    /// entry has status "ok", model, version, directory, bin_present, bin_sha256,
    /// bin_size_bytes, archived_at, archived_by, canonical_snapshot_path, notes and provenance;
    /// an entry that could not be listed has status "unreadable", directory, error_kind and error.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct FirmwarePinArgs {
    /// Snapshot to pin as the canonical settings for this firmware version.
    snapshot_path: PathBuf,
    /// Camera model, e.g. "Z 9".
    #[arg(long)]
    model: String,
    /// Firmware version string, e.g. "5.31".
    #[arg(long)]
    version: String,
}

#[derive(Args, Debug)]
struct FirmwareRollbackArgs {
    /// Camera model, e.g. "Z 9".
    #[arg(long)]
    model: String,
    /// Firmware version string, e.g. "5.31".
    #[arg(long)]
    version: String,
    /// Restrict bundle to a specific camera serial.
    #[arg(long)]
    serial: Option<String>,
    /// Directory to write the rollback bundle into. Default: <data-dir>/rollback-bundles/{model}_{version}_{timestamp}/
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct FirmwareCheckArgs {
    /// Check only the camera with this serial number.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorReadArgs {
    /// Property code, e.g. "0xD053" or "53331". Does not need the 0x10000
    /// vendor-read flag bit — that's added internally.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    propcode: u16,
    /// Camera serial to target. Required when more than one Nikon camera is
    /// on USB; this command does not use --sdk-bundle/the MAID SDK at all.
    /// The "id-N" serial `fleet discover` prints for a camera with no readable
    /// USB serial is accepted only when that is the one Nikon camera attached
    /// (this command cannot tell SDK ids apart); with several cameras use the
    /// real serial or "usb-<bus>:<addr>".
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct DeviceInfoArgs {
    /// Camera serial to target; same rules as vendor-read's --serial (required when more than
    /// one Nikon camera is on USB; "id-N" only when that is the sole camera attached and it has
    /// no readable USB serial).
    #[arg(long)]
    serial: Option<String>,
    /// Print everything as JSON on stdout, including the full operation, event, property and
    /// format lists as hex codes; warnings go to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct VendorWriteFtpArgs {
    /// Profile name shown in the camera's own menus.
    #[arg(long)]
    profile_name: String,
    #[arg(long)]
    ssid_24ghz: String,
    #[arg(long)]
    ssid_5ghz: String,
    /// FTP server hostname or IP address.
    #[arg(long)]
    host: String,
    #[arg(long, default_value_t = 21)]
    port: u16,
    #[arg(long)]
    username: String,
    /// The FTP password as a plain CLI argument — visible to any other
    /// local user via `ps`/`/proc/<pid>/cmdline` while this command runs.
    /// Prefer --password-stdin.
    #[arg(long, required_unless_present = "password_stdin")]
    password: Option<String>,
    /// Read the FTP password from stdin instead of an argument — not
    /// visible in the process list. `echo -n "$PASSWORD" | fleet
    /// vendor-write-ftp ... --password-stdin` or an interactive prompt both
    /// work; reads until EOF and strips a single trailing newline only
    /// (not other whitespace, which could be a real password character).
    #[arg(long, conflicts_with = "password")]
    password_stdin: bool,
    /// Camera serial to target. Required when more than one Nikon camera is
    /// on USB; this command does not use --sdk-bundle/the MAID SDK at all.
    /// The "id-N" serial `fleet discover` prints for a camera with no readable
    /// USB serial is accepted only when that is the one Nikon camera attached
    /// (this command cannot tell SDK ids apart); with several cameras use the
    /// real serial or "usb-<bus>:<addr>".
    #[arg(long)]
    serial: Option<String>,
    /// Print the encoded blob without sending it to the camera.
    #[arg(long)]
    dry_run: bool,
    /// Send this reverse-engineered write even if the connected camera's
    /// model isn't in ptp_usb's confirmed-safe list. Only pass this once
    /// you've independently verified the wire format on that model — see
    /// nikon_fleet::ptp_usb's module doc.
    #[arg(long)]
    force_unconfirmed_model: bool,
}

#[derive(Args, Debug)]
struct RestoreArgs {
    /// Snapshot to restore from. Resolved relative to snapshots/ if not absolute.
    snapshot_path: PathBuf,
    /// Target camera serial. Defaults to the serial recorded in the snapshot.
    #[arg(long)]
    serial: Option<String>,
    /// Print what would be written without sending anything to the camera.
    #[arg(long)]
    dry_run: bool,
    /// Restore only this property and leave everything else on the camera as
    /// it is (a targeted undo). Repeatable; accepts the name with or without
    /// the kNkMAIDCapability_ prefix. A name that is not in the snapshot is an
    /// error, and so is a named property that cannot be written (state
    /// property, absent or read-only on this body, or a failed write): the
    /// command exits non-zero rather than report a partial undo as done.
    #[arg(long = "only", value_name = "PROPERTY")]
    only: Vec<String>,
    /// Go ahead even if the restore point (a capture of the camera as it is now,
    /// taken before any write) is missing capabilities the model's baseline reads,
    /// or the model has no baseline. The restore point is still taken and the
    /// read-back still compared.
    #[arg(long)]
    allow_partial: bool,
    /// Apply only the plan that has this digest (from a `--dry-run` of the same snapshot, target
    /// and `--only`, e.g. one a person has just reviewed). The plan is recomputed on the camera as
    /// it is now; if it no longer matches, nothing is written and the command exits 5. Without
    /// this the command applies the plan it has just computed itself. Needs --expect-summary.
    #[arg(long, value_name = "DIGEST", requires = "expect_summary", conflicts_with = "dry_run")]
    expect_digest: Option<String>,
    /// The plan's summary line, copied from the same dry run as --expect-digest.
    #[arg(long, value_name = "SUMMARY", requires = "expect_digest", conflicts_with = "dry_run")]
    expect_summary: Option<String>,
    /// Print the plan (with --dry-run) or the result as JSON on stdout (the same value the MCP
    /// worker's `restore-plan` / `restore-apply` return); progress and warnings go to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct TransplantArgs {
    /// Source snapshot to copy settings FROM. May be a different camera
    /// model than the target — that's the point of this command, unlike
    /// `restore`. Resolved relative to snapshots/ if not absolute.
    snapshot_path: PathBuf,
    /// Target camera serial. Required when more than one Nikon camera is
    /// on USB; unlike `restore`, this does NOT default to the serial
    /// recorded in the snapshot (that camera is, by definition, a
    /// different — often absent — body for a cross-model transplant).
    #[arg(long)]
    serial: Option<String>,
    /// Report what would be written and by which strategy, without sending
    /// anything to the camera. Still performs live reads of the target's
    /// current enum/range values (needed to know whether a match exists at
    /// all), so this is slower than `restore --dry-run`.
    #[arg(long)]
    dry_run: bool,
    /// Also write this property, which is matched only by raw numeric code
    /// (an enum without decoded labels, e.g. PictureControl, MeteringMode) or
    /// is a numeric scalar copied as-is (e.g. WBAutoType, MovieRecMicrophone).
    /// Nothing guarantees both bodies give the same number the same meaning, so
    /// by default such a property is skipped whenever it would change the
    /// target; naming it here accepts that risk for that one property.
    /// Repeatable; accepts the name with or without the kNkMAIDCapability_
    /// prefix. Run with --dry-run first to see what each would set.
    #[arg(long = "allow-raw-code", value_name = "PROPERTY")]
    allow_raw_code: Vec<String>,
    /// Go ahead even if the restore point (a capture of the camera as it is now,
    /// taken before any write) is missing capabilities the model's baseline reads,
    /// or the model has no baseline. The restore point is still taken and the
    /// read-back still compared.
    #[arg(long)]
    allow_partial: bool,
    /// Apply only the plan that has this digest (from a `--dry-run` of the same snapshot, target
    /// and opt-ins, e.g. one a person has just reviewed). The plan is recomputed on the camera as
    /// it is now; if it no longer matches, nothing is written and the command exits 5. Without
    /// this the command applies the plan it has just computed itself. Needs --expect-summary.
    #[arg(long, value_name = "DIGEST", requires = "expect_summary", conflicts_with = "dry_run")]
    expect_digest: Option<String>,
    /// The plan's summary line, copied from the same dry run as --expect-digest.
    #[arg(long, value_name = "SUMMARY", requires = "expect_digest", conflicts_with = "dry_run")]
    expect_summary: Option<String>,
    /// Print the plan (with --dry-run) or the result as JSON on stdout (the same value the MCP
    /// worker's `transplant-plan` / `transplant-apply` return); progress and warnings go to stderr.
    #[arg(long)]
    json: bool,
}

// ─────────────────────────────────────────────────────────────────────────
// Layout helpers
// ─────────────────────────────────────────────────────────────────────────

fn snapshots_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("snapshots")
}

fn references_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("references")
}

fn ensure_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
}

// ─────────────────────────────────────────────────────────────────────────
// USB enrichment
// ─────────────────────────────────────────────────────────────────────────

fn enrich_devices(devices: Vec<DeviceInfo>) -> Vec<(DeviceInfo, Option<UsbCameraInfo>)> {
    let enriched = pair_devices(devices, &usb_camera_list());
    warn_if_ambiguous_pairing(&enriched);
    enriched
}

/// `pair_devices` disambiguates same-model bodies purely by enumeration
/// order between two independently-collected lists (SDK EnumDevices vs. the
/// OS USB stack) — there is no per-unit identifier in the SDK's device info
/// to cross-check against. That's silent and unfixable with current SDK
/// data, so at minimum make the ambiguity loud instead of invisible: warn
/// whenever more than one body of the same model is present, since that's
/// exactly the condition under which order-based pairing can attach the
/// wrong serial/firmware to a camera.
fn warn_if_ambiguous_pairing(enriched: &[(DeviceInfo, Option<UsbCameraInfo>)]) {
    // The grouping rule is ops::cameras's, shared with the MCP's refusal.
    for (model, count) in duplicate_models(enriched.iter().map(|(dev, _)| dev.name.as_str())) {
        eprintln!(
            "warning: {count} \"{model}\" bodies connected — serial/firmware assignment \
             is based on SDK/USB enumeration order, not a per-unit identifier, and could \
             be swapped between them. Verify with `fleet discover` before trusting which \
             serial is which."
        );
    }
}

/// Who the body being written to is, for a restore point and for deciding whether a snapshot is
/// of this very body. Built from the body itself, never from the snapshot being applied: taking
/// the snapshot's serial when no USB descriptor was read made every other body look like the
/// snapshot's own, and an unreadable (empty) descriptor made a body's own snapshot look foreign.
fn target_identity(target: &DeviceInfo, usb_info: &Option<UsbCameraInfo>) -> Camera {
    Camera {
        model: target.name.clone(),
        serial: storable_serial(usb_info, target.id),
        firmware: body_firmware(target, usb_info),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Commands
// ─────────────────────────────────────────────────────────────────────────

fn cmd_diff(data_dir: &Path, schema_path: &Path, args: &DiffArgs) -> Result<()> {
    // Allow paths to be either absolute or relative to snapshots/.
    let a_path = resolve_snapshot_path(data_dir, &args.snapshot_a);
    let b_path = resolve_snapshot_path(data_dir, &args.snapshot_b);
    let a = Snapshot::load_from_file(&a_path)
        .with_context(|| format!("loading {}", a_path.display()))?;
    let b = Snapshot::load_from_file(&b_path)
        .with_context(|| format!("loading {}", b_path.display()))?;
    let opts = DiffOptions { include_volatile: args.include_volatile };
    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let d = diff_with_schema(&a, &b, &opts, &schema)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&d)?);
    } else {
        print_diff_human(&a, &b, &d);
    }
    Ok(())
}

fn print_diff_human(a: &Snapshot, b: &Snapshot, d: &Diff) {
    println!("A: {} {} {} captured {}", a.camera.model, a.camera.serial,
        a.label.as_deref().unwrap_or(""), a.captured_at);
    println!("B: {} {} {} captured {}", b.camera.model, b.camera.serial,
        b.label.as_deref().unwrap_or(""), b.captured_at);
    println!();

    if d.is_empty() {
        println!("(no differences)");
        return;
    }
    if !d.changed.is_empty() {
        println!("Changed ({}):", d.changed.len());
        for c in &d.changed {
            println!("  {} [{}]", c.name, c.code);
            println!("    A: {}", c.value_a);
            println!("    B: {}", c.value_b);
        }
    }
    if !d.only_in_a.is_empty() {
        println!("\nOnly in A ({}):", d.only_in_a.len());
        for name in &d.only_in_a {
            println!("  {name}");
        }
    }
    if !d.only_in_b.is_empty() {
        println!("\nOnly in B ({}):", d.only_in_b.len());
        for name in &d.only_in_b {
            println!("  {name}");
        }
    }
    if let Some(fa) = &d.firmware_annotation {
        println!("\n! Firmware version boundary: {} → {}", fa.firmware_a, fa.firmware_b);
        if !fa.schema_only_in_a.is_empty() {
            println!("  removed in newer firmware: {}", fa.schema_only_in_a.join(", "));
        }
        if !fa.schema_only_in_b.is_empty() {
            println!("  added in newer firmware: {}", fa.schema_only_in_b.join(", "));
        }
        if !fa.schema_changed.is_empty() {
            println!("  allowed-ops changed across firmware: {}", fa.schema_changed.join(", "));
        }
    }
}

/// Snapshots are addressable by absolute path OR by basename inside snapshots/.
fn resolve_snapshot_path(data_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() || p.exists() {
        p.to_path_buf()
    } else {
        snapshots_dir(data_dir).join(p)
    }
}

fn cmd_ls(data_dir: &Path, args: &LsArgs) -> Result<()> {
    let listing = list_dir(
        &snapshots_dir(data_dir),
        args.camera.as_deref(),
        Some(&references_dir(data_dir)),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&listing)?);
        return Ok(());
    }
    if !listing.dir_exists {
        println!("(no snapshots — directory {} does not exist)", listing.dir);
        return Ok(());
    }
    warn_skipped_dir_entries(&listing);
    for entry in &listing.entries {
        match &entry.state {
            EntryState::Ok { model, serial, firmware, label, captured_at, .. } => println!(
                "{name}\n  {model} {serial}  label={label}  fw={firmware}  {captured_at}",
                name = entry.file,
                label = label.as_deref().unwrap_or("-"),
            ),
            EntryState::Unreadable { error, .. } => println!("{}  (failed to load: {error})", entry.file),
        }
    }
    if listing.entries.is_empty() {
        println!("(no snapshots match)");
    }
    Ok(())
}

fn cmd_show(data_dir: &Path, schema_path: &Path, args: &ShowArgs) -> Result<()> {
    let path = render::locate(data_dir, &args.snapshot_file)?;
    let snapshot = render::load(&path)?;
    // The names live beside MaidLayer.config, wherever --schema puts it.
    let labels = if args.raw {
        Labels::none()
    } else {
        let firmware = Some(snapshot.camera.firmware.as_str()).filter(|f| !f.is_empty());
        Labels::load_from(schema_path, &schema_path.with_file_name(labels::LABELS_FILE), &snapshot.camera.model, firmware)
    };
    let opts = if args.raw { render::RenderOptions::raw() } else { render::RenderOptions::viewer(&labels) };
    let rendered = render::render_snapshot_opts(&snapshot, args.filter.as_deref(), &opts);
    if let Some(note) = &rendered.labels_note {
        eprintln!("note: {note}");
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rendered)?);
    } else {
        let text = render::TextOptions { codes: args.codes || args.raw, names: !args.raw && !args.no_names };
        print!("{}", rendered.to_text_with(args.filter.as_deref(), text));
    }
    Ok(())
}

/// Directory entries the OS refused to yield are counted by `list_dir`; say so
/// (stderr, so `--json` and piped stdout stay clean) instead of dropping them.
fn warn_skipped_dir_entries(listing: &Listing) {
    if listing.skipped_dir_entries > 0 {
        eprintln!(
            "warning: {} entr{} in {} could not be read and {} not listed",
            listing.skipped_dir_entries,
            if listing.skipped_dir_entries == 1 { "y" } else { "ies" },
            listing.dir,
            if listing.skipped_dir_entries == 1 { "was" } else { "were" },
        );
    }
}

fn cmd_rm(_data_dir: &Path, args: &RmArgs) -> Result<()> {
    fs::remove_file(&args.snapshot_path)
        .with_context(|| format!("removing {}", args.snapshot_path.display()))?;
    println!("removed {}", args.snapshot_path.display());
    Ok(())
}

/// What `ref set` did, for its message.
#[derive(Debug)]
struct ReferenceSet {
    camera: Camera,
    target: PathBuf,
    /// The reference that was there before, when there was one: its capture time and label.
    replaced: Option<(String, Option<String>)>,
    /// The snapshot was already byte-for-byte the reference; nothing changed.
    unchanged: bool,
}

/// Promote `snapshot_path` to be the reference for its camera: a byte copy of the file under
/// `refs/{model_slug}_{serial}.json`. The name is built from the snapshot's own model and serial
/// through `checked_reference_filename`, so a `/` or `..` in a snapshot (data, not code) cannot
/// write outside `refs`; the bytes parsed are the bytes written (one read), and the write is
/// atomic, so a failure keeps the old reference. An existing reference is replaced: that is what
/// promoting means, and the result says what was replaced.
fn set_reference(refs: &Path, snapshot_path: &Path) -> Result<ReferenceSet> {
    let bytes = fs::read(snapshot_path).with_context(|| format!("loading {}", snapshot_path.display()))?;
    let snap = Snapshot::from_json(
        std::str::from_utf8(&bytes).with_context(|| format!("loading {}", snapshot_path.display()))?,
    )
    .with_context(|| format!("loading {}", snapshot_path.display()))?;
    let name = checked_reference_filename(&snap.camera.model, &snap.camera.serial).map_err(|e| {
        anyhow::anyhow!(
            "{} cannot be a reference: {e}; the reference file name is built from its camera model and serial",
            snapshot_path.display()
        )
    })?;
    ensure_dir(refs)?;
    let target = refs.join(name);
    let previous = fs::read(&target).ok();
    let replaced = previous
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|t| Snapshot::from_json(t).ok())
        .map(|s| (s.captured_at, s.label));
    let unchanged = previous.as_deref() == Some(bytes.as_slice());
    write_atomic(&target, &bytes).with_context(|| format!("writing {}", target.display()))?;
    Ok(ReferenceSet { camera: snap.camera, target, replaced, unchanged })
}

fn cmd_ref(data_dir: &Path, sub: &RefCmd) -> Result<()> {
    let refs = references_dir(data_dir);
    match sub {
        RefCmd::Set { snapshot_path } => {
            let done = set_reference(&refs, snapshot_path)?;
            println!(
                "Set reference for {} {} → {}",
                done.camera.model, done.camera.serial, done.target.display()
            );
            match (&done.replaced, done.unchanged) {
                (_, true) => println!("  (this snapshot was already the reference; nothing changed)"),
                (Some((captured_at, label)), false) => println!(
                    "  replaced the previous reference (captured {captured_at}, label {})",
                    label.as_deref().unwrap_or("-")
                ),
                (None, false) => {}
            }
        }
        RefCmd::Show { model, serial } => {
            let target = refs.join(checked_reference_filename(model, serial)?);
            if !target.exists() {
                bail!("no reference for {} {} (looked for {})", model, serial, target.display());
            }
            let snap = Snapshot::load_from_file(&target)?;
            println!(
                "{} {}  fw={}  label={}  captured={}\n  {} properties",
                snap.camera.model, snap.camera.serial,
                snap.camera.firmware,
                snap.label.as_deref().unwrap_or("-"),
                snap.captured_at,
                snap.properties.len(),
            );
        }
        RefCmd::List { json } => {
            let listing = list_dir(&refs, None, None)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&listing)?);
                return Ok(());
            }
            if !listing.dir_exists {
                println!("(no references — directory {} does not exist)", listing.dir);
                return Ok(());
            }
            warn_skipped_dir_entries(&listing);
            for entry in &listing.entries {
                println!("{}", entry.file);
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Live-camera commands (SDK)
// ─────────────────────────────────────────────────────────────────────────

// OP_GET and OP_SET are re-exported from nikon_fleet::sdk.

/// An initialised SDK plus the cross-process camera lock that serialises SDK
/// sessions (`nikon_fleet::camera_lock`). The lock is released only after the
/// SDK is dropped: fields drop in declaration order, so `sdk` must stay first.
struct SdkSession {
    sdk: Sdk,
    _camera_lock: CameraLock,
}

impl std::ops::Deref for SdkSession {
    type Target = Sdk;
    fn deref(&self) -> &Sdk {
        &self.sdk
    }
}

impl std::ops::DerefMut for SdkSession {
    fn deref_mut(&mut self) -> &mut Sdk {
        &mut self.sdk
    }
}

/// Take the camera lock, then load and initialise the SDK. The lock comes
/// first because `InitializeSDK` (and the USB reset before it) is exactly the
/// step that must not overlap another session (F2, F3 in
/// docs/mcp-hardware-findings.md). Contention fails at once with
/// "camera busy"; there is no wait or retry.
fn open_sdk(bundle_path: &Path, no_usb_reset: bool) -> Result<SdkSession> {
    if !bundle_path.exists() {
        bail!(
            "SDK bundle not found at {}.\n\
             Run scripts/setup-sdk-runtime.sh first, or pass --sdk-bundle <path>.",
            bundle_path.display()
        );
    }
    let camera_lock = CameraLock::acquire()?;
    let mut sdk = Sdk::open(bundle_path)
        .with_context(|| format!("loading SDK from {}", bundle_path.display()))?;
    if no_usb_reset {
        sdk.initialize_no_usb_reset().context("InitializeSDK")?;
    } else {
        sdk.initialize().context("InitializeSDK")?;
    }
    Ok(SdkSession { sdk, _camera_lock: camera_lock })
}

fn cmd_discover(bundle_path: &Path, args: &DiscoverArgs, no_usb_reset: bool) -> Result<()> {
    // The SDK prints its own line to stdout when it initialises; keep that off --json's stdout.
    let mut json_out = nikon_fleet::json_out::JsonOut::redirect(args.json)?;
    let sdk = open_sdk(bundle_path, no_usb_reset)?;
    let enriched = enrich_devices(sdk.devices()?);
    let report = discover_report(&enriched, args.json)?;
    if args.json {
        json_out.emit(&report)?;
    } else {
        println!("{report}");
    }
    // Read-only and finished: the output is out and flushed, so leave without closing the session
    // (F24: the SDK's DisconnectDevice can hang, and nothing here needs it).
    nikon_fleet::process_exit::exit_now(0)
}

/// What `discover` prints: the `{"cameras": [...]}` document under `--json`, else the text listing.
/// One value built through serde in every case, so the empty and populated JSON cannot drift apart.
/// The rows are the ones the egui GUI can build (`ops::cameras::describe_device`): `serial` is the
/// identity snapshots are stored under, so a snapshot just taken shows in its own list, and the
/// text row prints the same fields so a serial shown there is one `--serial` accepts.
fn discover_report(enriched: &[(DeviceInfo, Option<UsbCameraInfo>)], json: bool) -> Result<String> {
    let cameras: Vec<_> = enriched.iter().map(|(dev, usb)| describe_device(dev, usb)).collect();
    if json {
        return Ok(serde_json::json!({ "cameras": cameras }).to_string());
    }
    if cameras.is_empty() {
        return Ok("(no Nikon cameras detected)".to_string());
    }
    let mut lines = vec![format!("{} device(s):", cameras.len())];
    for cam in &cameras {
        lines.push(format!(
            "  {}  serial={}  fw={}  id={}  baseline={}",
            cam.model, cam.serial, cam.firmware, cam.sdk_id, if cam.baseline { "yes" } else { "no" }
        ));
    }
    Ok(lines.join("\n"))
}

/// Select the target device: by --serial if given (the one rule for every caller,
/// `ops::target::select_by_cli_serial`: a USB serial, or the `id-N` / SDK id `discover` printed
/// for a body with no readable one; ambiguity is refused, never first-match), else the first
/// available one. Shared by cmd_snapshot, cmd_check and cmd_transplant.
fn select_device(
    enriched: Vec<(DeviceInfo, Option<UsbCameraInfo>)>,
    serial: Option<&str>,
) -> Result<(DeviceInfo, Option<UsbCameraInfo>)> {
    match serial {
        Some(s) => select_by_cli_serial(enriched, s).map_err(|e| target_error(s, e)),
        None => {
            let idx = enriched.iter().position(|(d, _)| d.available).unwrap_or(0);
            Ok(enriched.into_iter().nth(idx).unwrap())
        }
    }
}

/// A `TargetError` with the CLI's own hint for the one case that is a mistake of naming.
fn target_error(requested: &str, e: TargetError) -> anyhow::Error {
    match e {
        TargetError::NotFound { .. } => {
            anyhow::anyhow!("no device with serial {requested} (see `nikon-fleet discover` for the serials in use)")
        }
        other => anyhow::anyhow!("{other}"),
    }
}

/// Whether `cmd_transplant` should refuse to guess a target rather than
/// call `select_device` — no `--serial` given, and more than one candidate
/// is actually available to write to. Unlike `select_device`'s other
/// callers (read-only commands, where "pick the first available one" is a
/// reasonable default), transplant performs a destructive cross-model write
/// to physical hardware, so silently picking one of several real candidates
/// is exactly the wrong default.
fn transplant_target_ambiguous(enriched: &[(DeviceInfo, Option<UsbCameraInfo>)], serial: Option<&str>) -> bool {
    serial.is_none() && enriched.iter().filter(|(d, _)| d.available).count() > 1
}

/// Capture through `ops::capture` and show each failed read on stderr, in the
/// form the CLI has always used. Shared by cmd_snapshot and cmd_check, so a
/// fix to one cannot silently miss the other.
fn capture_capabilities(
    device: &Device,
    name_lookup: &HashMap<u32, String>,
    snap: &mut Snapshot,
) -> CaptureStats {
    let stats = ops_capture::capture(device, name_lookup, snap);
    for f in &stats.failed {
        eprintln!("  warn: {} [{:#x}]: {}", f.name, f.code, f.error);
    }
    stats
}

/// `unknown_schema` warning shared by cmd_snapshot and cmd_check.
fn warn_if_unknown_schema(unknown_schema: usize, schema_path: &Path) {
    use nikon_fleet::ops::baseline::capabilities_phrase;
    if unknown_schema > 0 {
        eprintln!(
            "warning: {} not found in the loaded schema \
             ({schema_path}) — recorded by numeric code (cap_0x...) instead of a symbolic \
             name. The schema may be stale relative to this camera's firmware.",
            capabilities_phrase(unknown_schema),
            schema_path = schema_path.display(),
        );
    }
}

/// Judge a freshly captured snapshot against its model's baseline, in memory,
/// before anything is written. An incomplete capture is an error unless
/// `allow_partial`; a capture the gate cannot judge is noted on stderr and
/// allowed (a new model has to be capturable to be baselined), with one exception: a capture
/// of a *different firmware* that lacks capabilities the baseline reads. A firmware update can
/// drop capabilities, but an interrupted capture or a USB reset looks exactly the same, so that
/// is refused too unless `allow_partial` says the shortfall is expected.
fn apply_capture_gate(snap: &Snapshot, allow_partial: bool) -> Result<nikon_fleet::ops::baseline::GateOutcome> {
    use nikon_fleet::ops::baseline::{GateOutcome, NoBaselineReason, describe_failure, gate};
    use nikon_fleet::ops::capture_snapshot::firmware_shortfall;
    if snap.camera.firmware.is_empty() {
        eprintln!(
            "warning: this body's firmware could not be read (no USB record paired with it), so the \
             capture records it as empty; the baseline gate treats unknown firmware as unchanged."
        );
    }
    let verdict = gate(snap);
    match &verdict {
        GateOutcome::Passed => {}
        GateOutcome::Failed { missing } => {
            let text = describe_failure(snap, missing);
            if allow_partial {
                eprintln!("warning: {text} Continuing because --allow-partial was given.");
            } else {
                bail!("{text}\nNothing was written. Re-run the capture, or pass --allow-partial to keep it anyway.")
            }
        }
        GateOutcome::NoBaseline(NoBaselineReason::UnknownModel) => {
            eprintln!(
                "note: no baseline for {:?}; this capture was not checked for completeness \
                 (`nikon-fleet baseline seed <snapshot>` makes one).",
                snap.camera.model
            );
        }
        GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline, capture }) => {
            match firmware_shortfall(snap, &verdict) {
                None => eprintln!(
                    "note: firmware {capture} differs from the {:?} baseline's {baseline} and the readable set differs \
                     too; this capture was not checked for completeness (re-seed the baseline for the new firmware).",
                    snap.camera.model
                ),
                Some(shortfall) if allow_partial => {
                    eprintln!("warning: {shortfall} Continuing because --allow-partial was given; re-seed the baseline for the new firmware.");
                }
                Some(shortfall) => bail!(
                    "{shortfall}\nNothing was written. Re-run the capture; if the firmware really dropped them, pass \
                     --allow-partial and re-seed the baseline for the new firmware."
                ),
            }
        }
    }
    Ok(verdict)
}

/// Human output: stdout normally, stderr under `--json` so that stdout holds only the JSON.
fn say(json: bool, text: &str) {
    if json {
        eprint!("{text}");
    } else {
        print!("{text}");
    }
}

/// What `seed_baseline` wrote.
#[derive(Debug)]
struct Seeded {
    path: PathBuf,
    /// The exact text written, for `registration_of`.
    json: String,
}

/// Write the baseline seeded from `snapshot_path` to `out_dir/{model_slug}.json`.
/// Refuses to replace an existing file unless `force`; with `force` it still refuses a
/// replacement that stops requiring a capability the existing baseline reads (or one it cannot
/// read to compare), unless `allow_shrink`: a snapshot of a partial capture or a restore point
/// carries no marker saying so, and a shrunken baseline passes captures the old one failed.
fn seed_baseline(snapshot_path: &Path, out_dir: &Path, force: bool, allow_shrink: bool) -> Result<Seeded> {
    use nikon_fleet::ops::baseline::{Baseline, capabilities_phrase, describe_missing_list, lost_capabilities};
    let snap = Snapshot::load_from_file(snapshot_path)
        .with_context(|| format!("loading {}", snapshot_path.display()))?;
    let from = snapshot_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let baseline = Baseline::from_snapshot(&snap, &from)?;
    let slug = model_slug(&baseline.model);
    if !is_safe_path_component(&slug) {
        bail!("model {:?} does not make a usable file name", baseline.model);
    }
    ensure_dir(out_dir)?;
    let path = out_dir.join(format!("{slug}.json"));
    let json = baseline.to_pretty_json()?;
    if force {
        match fs::read_to_string(&path) {
            Ok(existing_text) => match Baseline::from_json(&existing_text) {
                Ok(existing) => {
                    if existing.model != baseline.model && !allow_shrink {
                        bail!(
                            "{} holds the baseline for {:?}, not {:?} (their names make the same file); \
                             pass --allow-shrink to replace it anyway",
                            path.display(), existing.model, baseline.model
                        );
                    }
                    let lost = lost_capabilities(&existing, &baseline);
                    if !lost.is_empty() && !allow_shrink {
                        bail!(
                            "refusing to replace {}: the new baseline (from {from}, firmware {}) no longer lists {} the \
                             existing one (firmware {}) reads: {}. A smaller baseline passes captures the old one would fail. \
                             If those capabilities really went away (a firmware update, a body state), pass --allow-shrink.",
                            path.display(), baseline.firmware, capabilities_phrase(lost.len()),
                            existing.firmware, describe_missing_list(&lost)
                        );
                    }
                }
                Err(e) if !allow_shrink => bail!(
                    "the existing {} cannot be read as a baseline ({e}), so the new one cannot be compared with it; \
                     pass --allow-shrink to replace it anyway",
                    path.display()
                ),
                Err(_) => {}
            },
            // Nothing there yet: --force on a fresh model is just a write.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading the existing {}", path.display())),
        }
        // Atomic: the shrink guard above protects this file, so a full disk or a kill mid-write
        // must leave the old baseline, not a truncated one.
        nikon_fleet::ops::paths::write_atomic(&path, json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    } else {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("{} (already exists? pass --force to replace it)", path.display()))?;
        f.write_all(json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(Seeded { path, json })
}

/// What the user must do next for a written baseline to be used by this program: nothing, a
/// rebuild, or a rebuild after registering the file (the registry is a hand-kept list).
fn seed_next_step(path: &Path, json: &str) -> String {
    use nikon_fleet::ops::baseline::{Registration, registration_of};
    let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match registration_of(&file, json) {
        Registration::Embedded => "This build already embeds exactly this baseline.".to_string(),
        Registration::RebuildNeeded => {
            "Baselines are embedded at build time: rebuild to use it (this build still has the old one).".to_string()
        }
        Registration::NotRegistered => format!(
            "NOT ACTIVE: no baseline file is read from disk, and {file} is not in the built-in registry. \
             Add {file:?} to BUILTIN_SOURCES in src/ops/baseline.rs (the test `every_file_in_baselines_is_registered` \
             fails until you do, if the file is in the repo's baselines/), then rebuild. Until then captures of this \
             model are reported as having no baseline."
        ),
    }
}

fn cmd_baseline(sub: &BaselineCmd) -> Result<()> {
    match sub {
        BaselineCmd::Seed { snapshot_path, out_dir, force, allow_shrink } => {
            let seeded = seed_baseline(snapshot_path, out_dir, *force, *allow_shrink)?;
            println!("Wrote {}\n{}", seeded.path.display(), seed_next_step(&seeded.path, &seeded.json));
            Ok(())
        }
    }
}

fn cmd_snapshot(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &SnapshotArgs, no_usb_reset: bool) -> Result<()> {
    let mut json_out = nikon_fleet::json_out::JsonOut::redirect(args.json)?;
    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    let (target, usb_info) = select_device(enriched, args.serial.as_deref())?;
    say(args.json, &format!("Connecting to {:?} (id={})…\n", target.name, target.id));

    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let name_lookup = schema.name_map_for_model(&target.name);
    if name_lookup.is_empty() {
        eprintln!(
            "warning: no schema entries found for model {:?}; properties will be keyed by numeric id",
            target.name
        );
    }

    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;
    say(args.json, &format!("Reading {} capabilities…\n", device.capabilities.len()));

    let captured_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let serial = storable_serial(&usb_info, target.id);
    let firmware = body_firmware(&target, &usb_info);
    let mut snap = Snapshot::new(
        Camera {
            model: target.name.clone(),
            serial: serial.clone(),
            firmware,
        },
        Transport::Usb,
        captured_at,
    );
    snap.label = args.label.clone();

    let stats = capture_capabilities(&device, &name_lookup, &mut snap);
    say(
        args.json,
        &format!(
            "  ok={}  err={}  skipped(no-get-bit)={}  unknown-schema={}\n",
            stats.read_ok, stats.read_err, stats.skipped_no_get, stats.unknown_schema
        ),
    );
    warn_if_unknown_schema(stats.unknown_schema, schema_path);

    // Judge completeness in memory, before anything is written. A capture that read nothing is
    // refused first, whatever `--allow-partial` says: there is nothing worth keeping.
    let verdict = judge_capture(&snap, &stats, args.allow_partial)
        .map_err(|e| anyhow::anyhow!("{e}; nothing was written"))?;

    // Save under snapshots/.
    let dir = snapshots_dir(data_dir);
    ensure_dir(&dir)?;
    let filename = snap.suggested_filename();
    let path = dir.join(&filename);
    // create-new: a capture must never replace an existing snapshot.
    snap.save_new(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    say(args.json, &format!("Wrote {}\n", path.display()));
    if args.json {
        // The same value the MCP worker's `capture` op returns.
        let outcome = nikon_fleet::ops::capture_snapshot::CaptureOutcome::new(
            filename.clone(),
            snap.label.clone(),
            snap.camera.clone(),
            snap.properties.len(),
            stats.clone(),
            verdict,
        );
        json_out.emit(&serde_json::to_string_pretty(&outcome)?)?;
    }
    // Read-only and finished: leave without closing the session (F24).
    nikon_fleet::process_exit::exit_now(0)
}

/// Read one vendor-only property via raw USB PTP (`0x943B`), bypassing the
/// MAID SDK entirely — no --sdk-bundle needed. See `nikon_fleet::ptp_usb`.
fn cmd_vendor_read(args: &VendorReadArgs) -> Result<()> {
    let read = vendor_locked_target(CameraLock::acquire, args.serial.as_deref(), usb_camera_list)
        .and_then(|(lock, serial)| {
            nikon_fleet::ptp_usb::read_vendor_property_report_locked(lock, serial.as_deref(), args.propcode)
                .map_err(anyhow::Error::new)
        })
        .with_context(|| format!("reading vendor property 0x{:04x} over USB PTP", args.propcode))?;
    let (stdout, warnings) = vendor_read_output(args.propcode, &read);
    for w in warnings {
        eprintln!("{w}");
    }
    println!("{stdout}");
    Ok(())
}

/// Read the PTP DeviceInfo dataset via raw USB PTP (`0x1001`) and print it with the serial
/// comparison. Read-only. See `nikon_fleet::ptp_usb`.
fn cmd_device_info(args: &DeviceInfoArgs) -> Result<()> {
    let report = vendor_locked_target(CameraLock::acquire, args.serial.as_deref(), usb_camera_list)
        .and_then(|(lock, serial)| {
            nikon_fleet::ptp_usb::read_device_info_locked(lock, serial.as_deref()).map_err(anyhow::Error::new)
        })
        .context("reading the PTP DeviceInfo dataset over USB")?;
    for w in &report.usb_warnings {
        eprintln!("warning: USB descriptor: {w}");
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&device_info_json(&report))?);
    } else {
        println!("{}", device_info_text(&report));
    }
    Ok(())
}

/// `0x1001`-style code list as hex strings, for `device-info --json`.
fn hex_codes(codes: &[u16]) -> Vec<String> {
    codes.iter().map(|c| format!("0x{c:04x}")).collect()
}

/// A decoded PTP string for the text output: quoted, with its anomalies (if any) after it.
fn ptp_string_text(s: &nikon_fleet::ptp_usb::PtpString) -> String {
    if s.anomalies.is_empty() {
        format!("{:?}", s.text)
    } else {
        let tags: Vec<&str> = s.anomalies.iter().map(|a| a.tag()).collect();
        format!("{:?}  [anomaly: {}]", s.text, tags.join(", "))
    }
}

/// The serial line of `device-info`, the question the command exists to answer.
fn device_info_serial_line(report: &nikon_fleet::ptp_usb::DeviceInfoReport) -> String {
    format!(
        "serial: PTP {:?} | USB {:?} | {}",
        report.dataset.serial_number.text,
        report.usb_serial,
        report.serial_verdict().tag()
    )
}

/// What `device-info` prints without `--json`: every string and number of the dataset, the
/// size of each list (the lists themselves are in `--json`), the count of trailing bytes, then the
/// serial comparison.
fn device_info_text(report: &nikon_fleet::ptp_usb::DeviceInfoReport) -> String {
    let d = &report.dataset;
    [
        format!("USB model:                 {:?}", report.usb_model),
        format!("standard version:          {} (0x{:04x})", d.standard_version, d.standard_version),
        format!("vendor extension id:       0x{:08x}", d.vendor_extension_id),
        format!("vendor extension version:  {} (0x{:04x})", d.vendor_extension_version, d.vendor_extension_version),
        format!("vendor extension desc:     {}", ptp_string_text(&d.vendor_extension_desc)),
        format!("functional mode:           0x{:04x}", d.functional_mode),
        format!("operations supported:      {}", d.operations_supported.len()),
        format!("events supported:          {}", d.events_supported.len()),
        format!("device properties:         {}", d.device_properties_supported.len()),
        format!("capture formats:           {}", d.capture_formats.len()),
        format!("playback formats:          {}", d.playback_formats.len()),
        format!("manufacturer:              {}", ptp_string_text(&d.manufacturer)),
        format!("model:                     {}", ptp_string_text(&d.model)),
        format!("device version:            {}", ptp_string_text(&d.device_version)),
        format!("serial number:             {}", ptp_string_text(&d.serial_number)),
        format!("trailing bytes:            {}", d.trailing.len()),
        device_info_serial_line(report),
    ]
    .join("\n")
}

/// What `device-info --json` prints: the whole dataset (lists as hex codes), the USB identity of
/// the same body, and the serial comparison.
fn device_info_json(report: &nikon_fleet::ptp_usb::DeviceInfoReport) -> serde_json::Value {
    let d = &report.dataset;
    let string = |s: &nikon_fleet::ptp_usb::PtpString| {
        serde_json::json!({ "text": s.text, "anomalies": s.anomalies.iter().map(|a| a.tag()).collect::<Vec<_>>() })
    };
    serde_json::json!({
        "usb": { "model": report.usb_model, "serial": report.usb_serial, "warnings": report.usb_warnings },
        "device_info": {
            "standard_version": d.standard_version,
            "vendor_extension_id": d.vendor_extension_id,
            "vendor_extension_version": d.vendor_extension_version,
            "vendor_extension_desc": string(&d.vendor_extension_desc),
            "functional_mode": d.functional_mode,
            "operations_supported": hex_codes(&d.operations_supported),
            "events_supported": hex_codes(&d.events_supported),
            "device_properties_supported": hex_codes(&d.device_properties_supported),
            "capture_formats": hex_codes(&d.capture_formats),
            "playback_formats": hex_codes(&d.playback_formats),
            "manufacturer": string(&d.manufacturer),
            "model": string(&d.model),
            "device_version": string(&d.device_version),
            "serial_number": string(&d.serial_number),
            "trailing_bytes": d.trailing.len(),
            "trailing_hex": nikon_fleet::ptp_usb::hex_bytes(&d.trailing),
        },
        "serial_comparison": {
            "ptp": d.serial_number.text,
            "usb": report.usb_serial,
            "verdict": report.serial_verdict().tag(),
        },
    })
}

/// What `vendor-read` prints: the data line for stdout, and one stderr line per warning the read
/// returned (an unsurveyed model, say). The command has no `--json` mode, so warnings are text.
fn vendor_read_output(propcode: u16, read: &nikon_fleet::ptp_usb::VendorRead) -> (String, Vec<String>) {
    (
        format!("0x{propcode:04x}: {} byte(s): {}", read.data.len(), nikon_fleet::ptp_usb::hex_bytes(&read.data)),
        read.warnings.iter().map(|w| format!("warning: {w}")).collect(),
    )
}

/// The serial to hand the raw-USB vendor commands. They see only the USB bus, so the `id-N`
/// that `discover` and the GUIs give for a body with no readable USB serial means nothing to
/// them: it resolves only when that body is the sole Nikon camera attached (`ops::target`), and
/// otherwise stops here, before any camera is opened. The bus is scanned (`attached`) only for an
/// `id-N`; a real serial or `usb-<bus>:<addr>` is passed through untouched.
fn vendor_target_serial(
    serial: Option<&str>,
    attached: impl FnOnce() -> Vec<UsbCameraInfo>,
) -> Result<Option<String>> {
    let on_bus = match serial {
        Some(s) if nikon_fleet::ops::target::is_id_fallback(s) => attached(),
        _ => Vec::new(),
    };
    raw_usb_serial_arg(serial, &on_bus).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The camera lock and the serial for a raw-USB vendor command, in that order: the lock is taken
/// first and handed on to the session (`PtpUsbSession::open_locked`), so resolving an `id-N`
/// (which reads the descriptors of the attached bodies) never touches the bus underneath another
/// nikon-fleet session. A busy lock is `PtpUsbError::CameraLock`, as it is from `PtpUsbSession::open`.
fn vendor_locked_target(
    acquire_lock: impl FnOnce() -> Result<CameraLock, nikon_fleet::camera_lock::LockError>,
    serial: Option<&str>,
    attached: impl FnOnce() -> Vec<UsbCameraInfo>,
) -> Result<(CameraLock, Option<String>)> {
    let lock = acquire_lock().map_err(nikon_fleet::ptp_usb::PtpUsbError::CameraLock)?;
    let serial = vendor_target_serial(serial, attached)?;
    Ok((lock, serial))
}

/// Strip exactly one trailing `\n` (as `echo`/a shell heredoc would add),
/// not general trailing whitespace — a password could legitimately end in
/// a space or tab, and this must not silently eat that.
fn strip_single_trailing_newline(s: String) -> String {
    s.strip_suffix('\n').map(str::to_string).unwrap_or(s)
}

/// Write an FTP profile via raw USB PTP (`0x90EE`), bypassing the MAID SDK
/// entirely. See `nikon_fleet::ptp_usb`.
fn cmd_vendor_write_ftp(args: &VendorWriteFtpArgs) -> Result<()> {
    let password = if args.password_stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("reading password from stdin")?;
        strip_single_trailing_newline(buf)
    } else {
        // clap's required_unless_present guarantees this is Some when
        // password_stdin is false.
        args.password.clone().expect("clap guarantees --password is set when --password-stdin is not")
    };
    // Before anything is encoded, previewed or sent: the same required-field rule the GUIs apply
    // (one list, `ptp_usb::ftp_profile_missing_fields`). An all-empty profile used to encode to
    // a blank-field blob and, without --dry-run, be written to the camera.
    let missing = nikon_fleet::ptp_usb::ftp_profile_missing_fields(
        &args.profile_name, &args.ssid_24ghz, &args.ssid_5ghz, &args.host, &args.username, &password,
    );
    if !missing.is_empty() {
        bail!("refusing to write an FTP profile with missing field(s): {}", missing.join(", "));
    }
    let profile = nikon_fleet::ptp_usb::FtpProfile {
        profile_name: args.profile_name.clone(),
        ssid_24ghz: args.ssid_24ghz.clone(),
        ssid_5ghz: args.ssid_5ghz.clone(),
        host: args.host.clone(),
        port: args.port,
        username: args.username.clone(),
        password,
    };

    if args.dry_run {
        // Redact the password before encoding for preview: the hex dump
        // below would otherwise make the plaintext password trivially
        // readable byte-for-byte, defeating the CLI's own --password
        // handling and any masked entry field a caller GUI used to collect
        // it.
        let blob = nikon_fleet::ptp_usb::encode_ftp_profile(&profile.with_password_redacted())
            .context("encoding FTP profile")?;
        println!(
            "Would write {} byte(s) (password redacted in this preview): {}",
            blob.len(),
            nikon_fleet::ptp_usb::hex_bytes(&blob)
        );
        return Ok(());
    }

    let receipt = vendor_locked_target(CameraLock::acquire, args.serial.as_deref(), usb_camera_list)
        .and_then(|(lock, serial)| {
            nikon_fleet::ptp_usb::write_ftp_profile_locked(lock, serial.as_deref(), &profile, args.force_unconfirmed_model)
                .map_err(anyhow::Error::new)
        })
        .map_err(|e| match e.downcast::<nikon_fleet::ptp_usb::PtpUsbError>() {
            Ok(e) => vendor_write_error(e),
            Err(other) => other,
        })?;
    println!("Wrote FTP profile {:?} ({}@{}:{})", args.profile_name, args.username, args.host, args.port);
    if let Some(note) = ftp_receipt_note(&receipt) {
        eprintln!("{note}");
    }
    Ok(())
}

/// A failed `vendor-write-ftp`, worded for the person at the terminal. Most failures are plain
/// ("camera busy", "no such camera", ...): their own text says what to do. An indeterminate write
/// (the data was sent, then something failed) is the one that must not read like the others:
/// `exit_code_for` gives it [`EXIT_WRITE_INDETERMINATE`].
fn vendor_write_error(e: nikon_fleet::ptp_usb::PtpUsbError) -> anyhow::Error {
    use nikon_fleet::ptp_usb::PtpUsbError;
    match e {
        e @ PtpUsbError::FtpWriteIndeterminate { .. } => anyhow::Error::new(e).context(
            "the camera may have changed: the profile data was sent before this failed; check the FTP profile \
             in the camera's menu before deciding whether to repeat the write",
        ),
        other => anyhow::Error::new(other).context("writing FTP profile over USB PTP"),
    }
}

/// What a successful FTP-profile write cannot promise: the camera answered OK, which is all there is
/// to go on (nothing reads the profile back). A reply that did not match the command, or that carried
/// parameters (none seen in any capture), is worth saying so the OK is not over-trusted.
fn ftp_receipt_note(r: &nikon_fleet::ptp_usb::FtpWriteReceipt) -> Option<String> {
    let mut notes = Vec::new();
    if !r.reply_matched_command {
        notes.push(
            "a reply to the setup or the write carried a different transaction id or operation code than the command"
                .to_string(),
        );
    }
    if !r.response_params.is_empty() {
        notes.push(format!("the write's response carried parameters {:?} (none were seen in any capture)", r.response_params));
    }
    (!notes.is_empty()).then(|| {
        format!(
            "note: the camera answered OK, but {}. The profile cannot be read back, so check it in the camera's menu.",
            notes.join(", and ")
        )
    })
}

/// Whether a capture should be treated as a hard failure: zero capabilities
/// read successfully (an empty, useless snapshot), not a routine handful of
/// per-capability read errors mixed with real successes.
///
/// The read-error count is deliberately not part of the test. An earlier version
/// required `read_err > 0`, which let a 0-read/0-error capture through with
/// exit status 0: observed on a Z6III when another process's USB reset landed
/// during `connect`, leaving the device with an empty capability list and
/// `snapshot` writing a 232-byte empty file as if it had succeeded. A real
/// body always has hundreds of readable capabilities, so "nothing read" is a
/// failure whatever the error count.
fn snapshot_capture_failed(read_ok: usize) -> bool {
    read_ok == 0
}

/// The whole in-memory verdict on a fresh capture, in the order that matters: a capture that read
/// nothing is an error before the baseline gate looks at it (`--allow-partial` would otherwise let
/// an empty capture through the gate as merely "incomplete"), then [`apply_capture_gate`].
/// The same order as `ops::capture_snapshot`'s `NothingReadable`, then `Incomplete`.
fn judge_capture(
    snap: &Snapshot,
    stats: &nikon_fleet::ops::capture::CaptureStats,
    allow_partial: bool,
) -> Result<nikon_fleet::ops::baseline::GateOutcome> {
    if snapshot_capture_failed(stats.read_ok) {
        bail!("no capability could be read ({} read error(s), see above)", stats.read_err);
    }
    apply_capture_gate(snap, allow_partial)
}

/// A connected camera that reports zero capabilities cannot be read from or
/// written to, and every command downstream would misreport that as a normal
/// result (snapshot: an empty file; restore/transplant: every property
/// "absent on target"; check: every property "only in reference"). Seen on
/// hardware when a second fleet process opened the SDK, with a USB reset,
/// while this one was connecting. Call right after `sdk.connect`, before
/// anything is read, written or saved.
fn ensure_capabilities_present(count: usize) -> Result<()> {
    if count == 0 {
        bail!(
            "the camera connected but reported 0 capabilities, so nothing was read or written. \
             This usually means another fleet/GUI process reset or opened the camera at the same \
             time; close it and retry (or unplug and replug the camera)"
        );
    }
    Ok(())
}

fn cmd_check(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &CheckArgs, no_usb_reset: bool) -> Result<()> {
    let mut json_out = nikon_fleet::json_out::JsonOut::redirect(args.json)?;
    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    let (target, usb_info) = select_device(enriched, args.serial.as_deref())?;

    // Live snapshot — shares capture_capabilities with cmd_snapshot; this
    // used to be an independent copy of the same loop.
    let schema = MaidLayerConfig::parse_file(schema_path)?;
    let name_lookup = schema.name_map_for_model(&target.name);
    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;
    let serial = storable_serial(&usb_info, target.id);
    let firmware = body_firmware(&target, &usb_info);

    let mut live = Snapshot::new(
        Camera {
            model: target.name.clone(),
            serial: serial.clone(),
            firmware,
        },
        Transport::Usb,
        OffsetDateTime::now_utc().format(&Rfc3339)?,
    );
    live.label = Some("live".into());
    let stats = capture_capabilities(&device, &name_lookup, &mut live);
    warn_if_unknown_schema(stats.unknown_schema, schema_path);
    let verdict = judge_capture(&live, &stats, args.allow_partial)?;

    // Reference for this body.
    let ref_path = references_dir(data_dir).join(checked_reference_filename(&target.name, &serial)?);
    if !ref_path.exists() {
        bail!(
            "no reference for {} {} at {}.\n\
             Hint: `fleet snapshot` first, then `fleet ref set <snapshot>`.",
            target.name, serial, ref_path.display()
        );
    }
    let reference = Snapshot::load_from_file(&ref_path)?;

    let d = diff_with_schema(&reference, &live, &DiffOptions::default(), &schema)?;
    if args.json {
        // The same value the MCP worker's `check` op returns.
        let report = nikon_fleet::ops::check::CheckReport::new(live.camera.clone(), &reference, verdict, stats, d);
        json_out.emit(&serde_json::to_string_pretty(&report)?)?;
    } else {
        print_diff_human(&reference, &live, &d);
    }
    // Read-only and finished: leave without closing the session (F24).
    nikon_fleet::process_exit::exit_now(0)
}

// ─────────────────────────────────────────────────────────────────────────
// Firmware commands
// ─────────────────────────────────────────────────────────────────────────

/// Whether `version` matches the exact `"{major}.{minor:02}"` shape
/// `bcd_decode_version` always produces (e.g. `"5.30"`) — the format
/// `fleet firmware check`'s archive lookup compares against. Deliberately
/// permissive about `major` (single or multi-digit); the load-bearing part
/// is `minor` being exactly 2 digits, since that's the part a human is
/// likely to type wrong (`"5.3"` instead of `"5.30"`).
fn looks_like_bcd_version(version: &str) -> bool {
    let Some((major, minor)) = version.split_once('.') else { return false };
    !major.is_empty() && major.chars().all(|c| c.is_ascii_digit())
        && minor.len() == 2 && minor.chars().all(|c| c.is_ascii_digit())
}


fn cmd_firmware_add(data_dir: &Path, args: &FirmwareAddArgs) -> Result<()> {
    if !args.bin_path.exists() {
        bail!("firmware file not found: {}", args.bin_path.display());
    }
    if !is_safe_path_component(&model_slug(&args.model)) {
        bail!("--model {:?} is not a valid single path component after slugging", args.model);
    }
    if !is_safe_path_component(&args.version) {
        bail!("--version {:?} is not a valid single path component", args.version);
    }
    if !looks_like_bcd_version(&args.version) {
        // --version is free-typed, but `fleet firmware check`'s archive
        // lookup compares it against bcd_decode_version()'s output, which
        // is always exactly "{major}.{minor:02}" (e.g. "5.30", never
        // "5.3"). A version that doesn't match that shape will silently
        // never match a live camera's firmware — warn now, at archive time,
        // instead of leaving the operator to debug a mismatch later with no
        // diagnostic at all.
        eprintln!(
            "warning: {:?} doesn't look like \"{{major}}.{{minor:02}}\" (e.g. \"5.30\", not \
             \"5.3\") — this is the exact shape `fleet firmware check` compares against a live \
             camera's firmware, so a differently-formatted version will never be recognized as \
             archived for that camera.",
            args.version
        );
    }
    let dir = archive_dir(data_dir, &args.model, &args.version);
    if dir.join("metadata.json").exists() && !args.force {
        return Err(FirmwareError::AlreadyArchived {
            model: args.model.clone(),
            version: args.version.clone(),
            path: dir.display().to_string(),
        }.into());
    }

    // Hash the source before copying so the stored digest reflects the original,
    // not a potentially truncated copy written under disk-full conditions.
    let size = fs::metadata(&args.bin_path)
        .with_context(|| format!("stat {}", args.bin_path.display()))?.len();
    let sha256 = sha256_file(&args.bin_path)
        .with_context(|| format!("hashing {}", args.bin_path.display()))?;

    fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    let dest = dir.join("firmware.bin");
    fs::copy(&args.bin_path, &dest)
        .with_context(|| format!("copying firmware to {}", dest.display()))?;
    let archived_at = OffsetDateTime::now_utc().format(&Rfc3339)?;

    let meta = FirmwareMeta {
        format_version: FIRMWARE_META_FORMAT_VERSION,
        model: args.model.clone(),
        firmware_version: args.version.clone(),
        archived_at,
        archived_by: "fleet firmware add".into(),
        bin_sha256: sha256.clone(),
        bin_size_bytes: size,
        source_path: args.bin_path.display().to_string(),
        canonical_snapshot_path: None,
        notes: args.notes.clone(),
        provenance: None,
    };
    save_meta(&meta, &dir)?;

    println!(
        "Archived {} firmware {}\n  bin:    firmware/{}/{}/firmware.bin  ({})\n  sha256: {}",
        args.model, args.version,
        model_slug(&args.model), args.version,
        format_size(size),
        sha256,
    );
    println!(
        "  Hint: run `fleet firmware pin --model {:?} --version {} <snapshot>` to associate settings.",
        args.model, args.version
    );
    Ok(())
}

fn cmd_firmware_ls(data_dir: &Path, args: &FirmwareLsArgs) -> Result<()> {
    // One walk and one validation rule (`firmware::scan_archives`) for the text and the JSON.
    let scan = scan_archives(data_dir, args.model.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&ArchiveListing::from_scan(&scan))?);
        return Ok(());
    }
    scan.warn_skipped();
    let entries = scan.entries;
    if entries.is_empty() {
        if let Some(m) = &args.model {
            println!("(no firmware archived for {m})");
        } else {
            println!("(no firmware archived)");
        }
        return Ok(());
    }
    let mut cur_model = String::new();
    for e in &entries {
        if e.model != cur_model {
            println!("{}", e.model);
            cur_model = e.model.clone();
        }
        let canonical = match &e.meta.canonical_snapshot_path {
            Some(p) => p.clone(),
            None => "(none)  ← pin needed".into(),
        };
        println!(
            "  {}   {}   {}   canonical: {}",
            e.version,
            e.meta.archived_at.get(..10).unwrap_or(&e.meta.archived_at),
            format_size(e.meta.bin_size_bytes),
            canonical,
        );
    }
    Ok(())
}

fn cmd_firmware_pin(data_dir: &Path, args: &FirmwarePinArgs) -> Result<()> {
    let snap = Snapshot::load_from_file(&args.snapshot_path)
        .with_context(|| format!("loading {}", args.snapshot_path.display()))?;

    if snap.camera.model != args.model {
        bail!(
            "snapshot is for model {:?} but --model is {:?}",
            snap.camera.model, args.model
        );
    }
    if !snap.camera.firmware.is_empty() && snap.camera.firmware != args.version {
        eprintln!(
            "warning: snapshot firmware field is {:?}, expected {:?} (proceeding)",
            snap.camera.firmware, args.version
        );
    }

    let mut meta = load_meta(data_dir, &args.model, &args.version)?;
    // canonical_snapshot_path must be relative to data_dir for portability.
    // Canonicalize both sides before stripping: a plain component-wise
    // strip_prefix fails on e.g. snapshot_path="snapshots/foo.json" against
    // the common default data_dir="." (different path *representations* of
    // the same location aren't a shared prefix syntactically), even though
    // the file genuinely is inside data_dir. Canonicalizing resolves both to
    // absolute, symlink-free paths so the comparison is representation-independent.
    let canonical_snapshot = args.snapshot_path.canonicalize()
        .with_context(|| format!("resolving {}", args.snapshot_path.display()))?;
    let canonical_data_dir = data_dir.canonicalize()
        .with_context(|| format!("resolving {}", data_dir.display()))?;
    let rel = canonical_snapshot
        .strip_prefix(&canonical_data_dir)
        .with_context(|| format!(
            "snapshot {} is outside data-dir {}; the canonical path must be \
             relative for portability. Move the snapshot under data-dir first.",
            args.snapshot_path.display(), data_dir.display()
        ))?
        .display().to_string();
    meta.canonical_snapshot_path = Some(rel.clone());

    let dir = archive_dir(data_dir, &args.model, &args.version);
    save_meta(&meta, &dir)?;
    println!("Pinned {} {} → {}", args.model, args.version, rel);
    Ok(())
}

fn generate_settings_table(snap: &Snapshot, schema: &MaidLayerConfig) -> String {
    let mut display_map: HashMap<String, String> = HashMap::new();
    for section in schema.sections_for_model(&snap.camera.model) {
        for cap in &section.capabilities {
            display_map.entry(cap.name.clone())
                .or_insert_with(|| cap.display.clone());
        }
    }
    let mut rows: Vec<(String, String)> = snap.properties.iter()
        .filter(|(name, _)| !is_volatile(name))
        .map(|(name, prop)| {
            let label = display_map.get(name)
                .filter(|d| !d.is_empty())
                .cloned()
                .unwrap_or_else(|| name.clone());
            (label, prop.value.to_string())
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.iter()
        .map(|(label, value)| format!("  {label:<50} {value}\n"))
        .collect()
}

fn cmd_firmware_rollback(data_dir: &Path, schema_path: &Path, args: &FirmwareRollbackArgs) -> Result<()> {
    // --serial doesn't change what's IN the bundle (the firmware binary and
    // canonical settings aren't per-camera), so its only meaningful effect
    // is confirming, before generating anything, that the operator is
    // targeting a camera that's actually present and of the right model —
    // catching a copy-pasted wrong serial before the operator flashes the
    // wrong body. Previously this flag was parsed and silently ignored.
    if let Some(serial) = &args.serial {
        let cameras = usb_camera_list();
        match cameras.iter().find(|c| &c.serial == serial) {
            Some(c) if c.model == args.model => {}
            Some(c) => bail!(
                "camera with serial {serial} is a {:?}, not {:?} — refusing to generate a \
                 rollback bundle for the wrong model",
                c.model, args.model
            ),
            None => bail!(
                "no connected camera with serial {serial} (found: {}); \
                 pass a serial from `fleet discover`, or omit --serial",
                cameras.iter().map(|c| c.serial.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    let meta = load_meta(data_dir, &args.model, &args.version)?;
    let archive = archive_dir(data_dir, &args.model, &args.version);
    let bin_path = archive.join("firmware.bin");

    // Integrity check.
    let computed = sha256_file(&bin_path)
        .with_context(|| format!("hashing {}", bin_path.display()))?;
    if computed != meta.bin_sha256 {
        return Err(FirmwareError::ChecksumMismatch {
            stored: meta.bin_sha256.clone(),
            computed,
        }.into());
    }

    let now = OffsetDateTime::now_utc();
    let ts = now.format(&Rfc3339)?.replace(':', "").replace('-', "");
    let out_dir = match &args.output_dir {
        Some(d) => d.clone(),
        None => data_dir.join("rollback-bundles").join(format!(
            "{}_{}_{}", model_slug(&args.model), args.version, ts
        )),
    };
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating {}", out_dir.display()))?;

    fs::copy(&bin_path, out_dir.join("firmware.bin"))?;

    let canonical_snap: Option<Snapshot> = match &meta.canonical_snapshot_path {
        Some(rel) => {
            let p = data_dir.join(rel);
            match Snapshot::load_from_file(&p) {
                Ok(s) => {
                    fs::copy(&p, out_dir.join("canonical_settings.json"))?;
                    Some(s)
                }
                Err(e) => {
                    eprintln!("warning: could not load canonical snapshot {}: {e}", p.display());
                    None
                }
            }
        }
        None => {
            eprintln!("warning: no canonical snapshot pinned for {} {}; bundle will lack settings reference.", args.model, args.version);
            None
        }
    };

    // Generate rollback instructions.
    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let timestamp_str = now.format(&Rfc3339)?;

    let mut instructions = String::new();
    instructions.push_str(&format!("Rollback Procedure: {} → firmware {}\n", args.model, args.version));
    instructions.push_str(&format!("Generated: {timestamp_str}\n"));
    instructions.push_str(&format!("Bundle: {}\n", out_dir.display()));
    if let Some(serial) = &args.serial {
        instructions.push_str(&format!("Target camera serial: {serial}  (verified connected and {} at generation time)\n", args.model));
    }
    instructions.push_str("\nSTEP 1 — VERIFY FIRMWARE FILE\n");
    instructions.push_str("  File:    firmware.bin\n");
    instructions.push_str(&format!("  Size:    {} bytes\n", meta.bin_size_bytes));
    instructions.push_str(&format!("  SHA-256: {}\n", meta.bin_sha256));
    instructions.push_str("  Confirm the file is not corrupt before proceeding.\n");
    instructions.push_str("\nSTEP 2 — COPY TO SD CARD\n");
    instructions.push_str("  Copy firmware.bin to an SD card formatted in the camera.\n");
    instructions.push_str("  Rename to the filename required by the camera's firmware update procedure.\n");
    instructions.push_str("  (See camera manual — Nikon requires a specific filename per model;\n");
    instructions.push_str("  the exact name is not encoded in this tool.)\n");
    instructions.push_str("\nSTEP 3 — FLASH\n");
    instructions.push_str("  Insert SD card into camera slot 1.\n");
    instructions.push_str("  MENU → SETUP MENU → Firmware version → Update\n");
    instructions.push_str("  Follow on-screen prompts. Do not power off during flashing.\n");
    instructions.push_str("\nSTEP 4 — VERIFY\n");
    instructions.push_str("  After restart: MENU → SETUP MENU → Firmware version\n");
    instructions.push_str(&format!("  Confirm the displayed version is {}.\n", args.version));
    instructions.push_str("  Or reconnect and run:\n");
    instructions.push_str("    fleet discover\n");
    instructions.push_str("    fleet firmware check\n");
    instructions.push_str("\nSTEP 5 — RESTORE SETTINGS\n");
    instructions.push_str("  (Automatic settings push is not yet implemented — see future work.)\n");
    instructions.push_str("  Restore settings manually using the table below as reference.\n");
    instructions.push_str("  canonical_settings.json contains the full snapshot for archival.\n\n");
    if let Some(snap) = &canonical_snap {
        instructions.push_str("  Key settings from canonical snapshot (non-volatile):\n");
        instructions.push_str("  ─────────────────────────────────────────────────────\n");
        instructions.push_str(&generate_settings_table(snap, &schema));
    } else {
        instructions.push_str("  (No canonical snapshot pinned for this firmware version.)\n");
    }
    fs::write(out_dir.join("rollback-instructions.txt"), instructions)?;

    println!(
        "Rollback bundle for {} → {}\n  {}/\n    firmware.bin              ({}, sha256 verified)",
        args.model, args.version, out_dir.display(), format_size(meta.bin_size_bytes)
    );
    if canonical_snap.is_some() {
        println!("    canonical_settings.json");
    }
    println!("    rollback-instructions.txt\n\nFollow rollback-instructions.txt to proceed.");
    Ok(())
}

fn cmd_firmware_check(data_dir: &Path, args: &FirmwareCheckArgs) -> Result<()> {
    let cameras = usb_camera_list();
    let filtered: Vec<&UsbCameraInfo> = match &args.serial {
        Some(s) => cameras.iter().filter(|c| &c.serial == s).collect(),
        None => cameras.iter().collect(),
    };
    if filtered.is_empty() {
        if args.serial.is_some() {
            bail!("no camera found with serial {:?}", args.serial);
        } else {
            println!("(no Nikon cameras detected via USB)");
            return Ok(());
        }
    }
    let refs_dir = references_dir(data_dir);
    for cam in filtered {
        let ref_fw = if refs_dir.exists() {
            // A model or serial that is not a plain name has no reference to find.
            match checked_reference_filename(&cam.model, &cam.serial) {
                Ok(name) if refs_dir.join(&name).exists() => Snapshot::load_from_file(refs_dir.join(name))
                    .ok()
                    .map(|s| s.camera.firmware)
                    .unwrap_or_default(),
                _ => String::new(),
            }
        } else {
            String::new()
        };

        let status = if ref_fw.is_empty() {
            "[NO REF]"
        } else if cam.firmware == ref_fw {
            "[OK]     "
        } else {
            "[CHANGED]"
        };

        let archived = if archive_exists(data_dir, &cam.model, &cam.firmware) {
            "archived"
        } else {
            "no archive"
        };

        println!(
            "{:<6}  {}   fw={}   ref-fw={:<6}  {}  {}",
            cam.model, cam.serial, cam.firmware, ref_fw, status, archived
        );
    }
    Ok(())
}

// ── Firmware source: latest / fetch ──────────────────────────────────────


fn print_survey(s: &ModelSurvey) {
    match (&s.status, &s.release) {
        (ModelStatus::CouldNotDetermine { reason }, _) => {
            println!("{:<5} COULD NOT DETERMINE — {reason}", s.model);
            println!("      (this says nothing about whether new firmware exists)");
        }
        (status, Ok(r)) => {
            let label = match status {
                ModelStatus::NewAvailable { .. } => "NEW — not in archive",
                _ => "already archived",
            };
            println!("{:<5} latest {} ({})  {label}", s.model, r.version,
                r.released.as_deref().map(|d| format!("released {d}")).unwrap_or_else(|| "release date unknown".into()));
            println!("      page: {}", r.page_url);
            match &r.download {
                DownloadAccess::Gated { notice } => println!("      download: GATED — {notice}"),
                DownloadAccess::Direct { url, .. } => println!("      download: direct — {url}"),
            }
            for w in &r.warnings {
                println!("      warning: {w}");
            }
        }
        (_, Err(_)) => unreachable!("assess() maps every Err to CouldNotDetermine"),
    }
    let archived: Vec<String> = s.archived.iter().map(|v| v.to_string()).collect();
    println!("      archived: {}", if archived.is_empty() { "(none)".into() } else { archived.join(", ") });
    for n in &s.archive_notes {
        println!("      warning: {n}");
    }
    if s.cameras.is_empty() {
        println!("      fleet: no {} connected via USB", s.model);
    }
    for (serial, fw) in &s.cameras {
        let rel = match &s.release {
            Ok(r) => match camera_relation(fw, r.version) {
                CameraRelation::Behind => "behind latest".to_string(),
                CameraRelation::Current => "at latest".to_string(),
                CameraRelation::Ahead => "AHEAD of the source's latest".to_string(),
                CameraRelation::Unreadable(why) => format!("firmware unreadable ({why})"),
            },
            Err(_) => "latest unknown".to_string(),
        };
        let in_archive = FwVersion::parse_canonical(fw).ok().is_some_and(|v| s.archived.contains(&v));
        println!("      fleet: {serial}  fw={fw}  {rel}; running version {}",
            if in_archive { "archived" } else { "NOT archived" });
    }
}

fn cmd_firmware_latest(data_dir: &Path, args: &FirmwareLatestArgs) -> Result<()> {
    let models = watch_models(&args.models);
    let surveys = survey_models(data_dir, &models, &NikonDownloadCenter::new(), &UreqHttp::new());
    let unknown = surveys.iter().filter(|s| matches!(s.status, ModelStatus::CouldNotDetermine { .. })).count();
    if args.json {
        let report = LatestReport { models: surveys.iter().map(survey_json).collect() };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for s in &surveys {
            print_survey(s);
        }
    }
    if unknown > 0 {
        bail!("{unknown} of {} model(s) could not be determined (see above)", surveys.len());
    }
    Ok(())
}

fn cmd_firmware_fetch(data_dir: &Path, args: &FirmwareFetchArgs) -> Result<()> {
    if args.from_file.is_some() && args.models.len() != 1 {
        bail!("--from-file needs exactly one --model (got {}); the file belongs to one model", args.models.len());
    }
    let source = NikonDownloadCenter::new();
    let http = UreqHttp::new();
    let models = watch_models(&args.models);
    let surveys = survey_models(data_dir, &models, &source, &http);
    let mut failures = 0usize;
    let mut already_archived = 0usize;
    let mut manual_pending = 0usize;

    for s in &surveys {
        print_survey(s);
        let release = match &s.release {
            Ok(r) => r,
            Err(_) => { failures += 1; continue; }
        };
        let result: Result<()> = (|| {
            if let Some(file) = &args.from_file {
                return ingest_user_file(data_dir, source.id(), release, file, args.dry_run);
            }
            match (&s.status, &release.download) {
                (ModelStatus::AlreadyArchived { .. }, _) => Ok(()),
                (_, DownloadAccess::Gated { .. }) => {
                    manual_pending += 1;
                    println!("      ACTION REQUIRED: open {}", release.page_url);
                    println!("        accept the licence there and download the file, then run:");
                    println!("        fleet firmware fetch --model {:?} --from-file <downloaded file>", s.model);
                    Ok(())
                }
                (_, DownloadAccess::Direct { url, .. }) if args.dry_run => {
                    println!("      dry-run: would download {url} and archive as {} {}", s.model, release.version);
                    Ok(())
                }
                (_, DownloadAccess::Direct { .. }) => {
                    let scratch = tempfile::tempdir().context("creating scratch dir")?;
                    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
                    match fetch_direct(&http, source.id(), release, data_dir, scratch.path(), &now)? {
                        IngestOutcome::Archived(m) => print_archived(&m),
                        IngestOutcome::AlreadyPresent(_) => println!("      already archived (identical bytes)"),
                    }
                    Ok(())
                }
            }
        })();
        if let Err(e) = result {
            failures += 1;
            already_archived += usize::from(is_already_archived(&e));
            eprintln!("error: {}: {e:#}", s.model);
        }
    }
    if manual_pending > 0 {
        println!("\n{manual_pending} model(s) need a manual, licence-gated download (see ACTION REQUIRED above).");
    }
    if failures > 0 {
        return Err(fetch_failure(failures, already_archived, surveys.len()));
    }
    Ok(())
}

/// The end of a `fetch` run in which `failures` of `total` models failed, `already_archived` of
/// them because that version is already archived with different bytes. When that is every
/// failure the run exits `EXIT_ALREADY_ARCHIVED`; any other mix is the ordinary exit 1.
fn fetch_failure(failures: usize, already_archived: usize, total: usize) -> anyhow::Error {
    let message = format!("{failures} of {total} model(s) failed (see above)");
    if failures > 0 && already_archived == failures {
        ExitCodeError { code: EXIT_ALREADY_ARCHIVED, message }.into()
    } else {
        anyhow::anyhow!(message)
    }
}

/// Manual route, shown when the embedded browser is unavailable.
fn print_manual_download(model: &str, page_url: &str) {
    println!("Download it by hand instead:");
    println!("  1. open {page_url}");
    println!("  2. read and accept the licence there, choose your OS, and download the file");
    println!("  3. fleet firmware fetch --model {model:?} --from-file <downloaded file>");
}

#[cfg(feature = "webview")]
fn embedded_browser() -> impl BrowserWindow {
    nikon_fleet::firmware_webview::EmbeddedBrowser
}

/// Stand-in so the command still exists (and explains itself) in builds
/// without the `webview` feature.
#[cfg(not(feature = "webview"))]
fn embedded_browser() -> impl BrowserWindow {
    use nikon_fleet::firmware_browse::{BrowseError, DownloadTracker};
    struct NoBrowser;
    impl BrowserWindow for NoBrowser {
        fn run(&self, _: &str, _: &Path, _: std::rc::Rc<std::cell::RefCell<DownloadTracker>>) -> Result<(), BrowseError> {
            Err(BrowseError::NotCompiledIn)
        }
    }
    NoBrowser
}

fn cmd_firmware_browse(data_dir: &Path, args: &FirmwareBrowseArgs) -> Result<()> {
    let source = NikonDownloadCenter::new();
    let http = UreqHttp::new();
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    println!("Looking up the newest {} firmware…", args.model);
    // The window opens only after the lookup succeeds; show where we're going first.
    let release = source.latest(&args.model, &http)
        .with_context(|| format!("could not determine the newest firmware for {:?}", args.model))?;
    println!("  {} {} (released {}) — {}", release.model, release.version,
        release.released.as_deref().unwrap_or("?"), release.page_url);
    println!("A browser window will open on that page. Read and accept Nikon's licence and click Download");
    println!("yourself — this tool never ticks \"I agree\" or presses Download for you.");

    match browse_firmware(data_dir, &source, &http, &args.model, &embedded_browser(), nikon_is_release_page, &now) {
        Ok(rep) => {
            match &rep.outcome {
                IngestOutcome::Archived(m) => print_archived(m),
                IngestOutcome::AlreadyPresent(_) => println!("      already archived (identical bytes); nothing written"),
            }
            match &rep.evidence {
                VersionEvidence::Confirms => println!("      file name {:?} agrees with version {}", rep.captured.original_name, rep.release.version),
                VersionEvidence::Unknown => println!(
                    "      NOTE: file name {:?} does not say which version it is, so the version {} comes from the page you were on only",
                    rep.captured.original_name, rep.release.version),
                VersionEvidence::Contradicts(_) => {} // refused earlier
            }
            Ok(())
        }
        Err(e) if e.browser_unavailable() => {
            eprintln!("{e}");
            print_manual_download(&args.model, &release.page_url);
            bail!("embedded browser unavailable")
        }
        Err(e) => Err(e.into()),
    }
}

fn print_archived(m: &FirmwareMeta) {
    println!(
        "      archived {} {}  firmware/{}/{}/firmware.bin  ({}, sha256 {})",
        m.model, m.firmware_version, model_slug(&m.model), m.firmware_version,
        format_size(m.bin_size_bytes), m.bin_sha256
    );
}

/// Archive a file the user downloaded themselves as `release`'s version.
fn ingest_user_file(data_dir: &Path, source_id: &str, release: &Release, file: &Path, dry_run: bool) -> Result<()> {
    let version = release.version.to_string();
    if dry_run {
        let sha = sha256_file(file).with_context(|| format!("hashing {}", file.display()))?;
        let size = fs::metadata(file)?.len();
        if archive_exists(data_dir, &release.model, &version) {
            let meta = load_meta(data_dir, &release.model, &version)?;
            if meta.bin_sha256 == sha {
                println!("      dry-run: {} {version} already archived with identical bytes; no-op", release.model);
            } else {
                bail!("dry-run: {} {version} is already archived with DIFFERENT bytes (archived {}, file {sha}); a real run would refuse",
                    release.model, meta.bin_sha256);
            }
        } else {
            println!("      dry-run: would archive {} as {} {version} ({}, sha256 {sha})",
                file.display(), release.model, format_size(size));
        }
        return Ok(());
    }
    let prov = Provenance {
        source_id: source_id.to_owned(),
        source_url: release.page_url.clone(),
        page_url: release.page_url.clone(),
        released: release.released.clone(),
        retrieved_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        method: RetrievalMethod::ManualAfterGate,
        published_size: None,
        published_sha256: None,
        original_filename: file.file_name().map(|n| n.to_string_lossy().into_owned()),
    };
    match ingest_bundle(data_dir, &release.model, &version, file, &prov, Some(&release.raw_page))? {
        IngestOutcome::Archived(m) => print_archived(&m),
        IngestOutcome::AlreadyPresent(_) => println!("      already archived (identical bytes); nothing written"),
    }
    Ok(())
}

/// The digest and summary a restore apply is held to: the plan a person approved and presented
/// (`--expect-digest`, `--expect-summary`), else the one just computed.
fn restore_confirmation<'a>(args: &'a RestoreArgs, digest: &'a str, summary: &'a str) -> (&'a str, &'a str) {
    (args.expect_digest.as_deref().unwrap_or(digest), args.expect_summary.as_deref().unwrap_or(summary))
}

/// A restore apply refused before any write, worded as a transplant's refusal is (the shared
/// `ApplyError`), a stale plan with its own status.
fn restore_refusal(e: nikon_fleet::ops::restore_flow::RestoreApplyError, json: bool) -> anyhow::Error {
    use nikon_fleet::ops::restore_flow::RestoreApplyError;
    match e {
        RestoreApplyError::Apply(a) => transplant_refusal(a, json),
        RestoreApplyError::Refused(r) => anyhow::anyhow!("{r}"),
    }
}

/// Restore settings onto the camera. An apply that fails before it has a report in hand (every
/// `RestoreApplyError` is raised before the first write) exits `EXIT_REFUSED_BEFORE_WRITE`, as a
/// transplant does (see `transplant_exit`).
fn cmd_restore(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &RestoreArgs, no_usb_reset: bool) -> Result<()> {
    let mut report_in_hand = false;
    restore_run(data_dir, bundle, schema_path, args, no_usb_reset, &mut report_in_hand)
        .map_err(|e| transplant_exit(e, args.dry_run, !report_in_hand))
}

fn restore_run(
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    args: &RestoreArgs,
    no_usb_reset: bool,
    report_in_hand: &mut bool,
) -> Result<()> {
    use nikon_fleet::ops::baseline::GatePolicy;
    use nikon_fleet::ops::restore_flow::{BodyPolicy, RestoreApplyRequest, RestorePlanInput, apply_restore, build_restore_plan};

    let json = args.json;
    let mut json_out = nikon_fleet::json_out::JsonOut::redirect(json)?;
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let source_bytes = fs::read(&snap_path).with_context(|| format!("loading {}", snap_path.display()))?;
    let snap = Snapshot::from_json(&String::from_utf8(source_bytes.clone()).with_context(|| format!("loading {}", snap_path.display()))?)
        .with_context(|| format!("loading {}", snap_path.display()))?;
    let source_name = snap_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    // Validated before the camera is touched: a mistyped --only must fail here,
    // not run as a restore that quietly had nothing to do.
    let only = if args.only.is_empty() {
        None
    } else {
        Some(restore::resolve_only(&args.only, &snap.properties)?)
    };

    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);

    // Match by --serial override, or by the serial embedded in the snapshot.
    let target_serial = args.serial.as_deref().unwrap_or(&snap.camera.serial);
    let (target, usb_info) = select_by_cli_serial(enriched, target_serial).map_err(|e| match e {
        TargetError::NotFound { .. } => anyhow::anyhow!(
            "no connected camera with serial {}  (snapshot is for {} {})",
            target_serial, snap.camera.model, snap.camera.serial
        ),
        other => anyhow::anyhow!("{other}"),
    })?;

    say(json, &format!("Connecting to {:?} (id={})…\n", target.name, target.id));
    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;

    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let name_lookup = schema.name_map_for_model(&target.name);
    let identity = target_identity(&target, &usb_info);
    // The CLI picked the body by serial above (including the id-N and SDK-id forms the MCP refuses),
    // so only the model is checked here. A different model is a transplant, not a restore.
    let input = RestorePlanInput {
        source: &snap,
        source_name: &source_name,
        source_bytes: &source_bytes,
        target: &identity,
        only: only.as_ref(),
        body_policy: BodyPolicy::SameModel,
    };
    let plan = build_restore_plan(&device, &input).map_err(|r| {
        anyhow::anyhow!("{r}\nUse `nikon-fleet transplant` to copy settings between models.")
    })?;
    let named = only.as_ref().map(|o| o.len());

    if args.dry_run {
        say(json, &cli_report::render_restore_dry_run(&plan.steps, &plan.counts, only.is_some()));
        say(json, &format!("Plan: {}\nDigest: {}\n", plan.summary, plan.digest));
        if json {
            json_out.emit(&serde_json::to_string_pretty(&plan)?)?;
        }
        if let Some(msg) = unrestored_error(named, &plan.unrestorable) {
            bail!("{msg}");
        }
        return Ok(());
    }

    let snapshots = snapshots_dir(data_dir);
    let captured_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    // Unless the caller presents a plan it approved (--expect-digest, the way the GUI does), the
    // plan above is this command's own and running it is the consent.
    let confirmation = restore_confirmation(args, &plan.digest, &plan.summary);
    let req = RestoreApplyRequest {
        plan: input,
        snapshots_dir: &snapshots,
        name_lookup: &name_lookup,
        expected_digest: confirmation.0,
        expected_summary: confirmation.1,
        captured_at,
        gate_policy: if args.allow_partial { GatePolicy::AllowPartial } else { GatePolicy::Strict },
    };
    let report = apply_restore(&device, &req, &mut |_| {}).map_err(|e| restore_refusal(e, json))?;
    // From here the camera may have been written.
    *report_in_hand = true;

    for w in cli_report::restore_write_warnings(&report.steps) {
        eprintln!("{w}");
    }
    say(json, &cli_report::restore_tally("Restore complete:", &report.counts, only.is_some()));
    say(json, &cli_report::render_restore_apply(&report));
    if let Some(v) = &report.verification {
        let (line, _) = restore_verification_text(v);
        say(json, &format!("{line}\n"));
    }
    if json {
        json_out.emit(&serde_json::to_string_pretty(&report)?)?;
    }

    let readback_failed = report.verification.as_ref().map(|v| restore_verification_text(v).1).unwrap_or(false);
    if let Some(msg) = cli_report::restore_failure(&report, named, readback_failed) {
        bail!("{msg}");
    }
    Ok(())
}

/// The one-line read-back report for the CLI, and whether it is a failure.
/// `residual_found` fails; an incomplete read-back is a warning: nothing is
/// claimed either way, and the command's other checks have already passed.
fn restore_verification_text(v: &restore::Verification) -> (String, bool) {
    match v.verdict {
        restore::Verdict::Verified => (
            "Read-back: verified — every written property reads back as restored".to_string(),
            false,
        ),
        restore::Verdict::NothingWritten => ("Read-back: nothing was written, nothing to verify".to_string(), false),
        restore::Verdict::Unjudged => (
            "Read-back: UNVERIFIED — none of the written properties could be compared (volatile-listed, \
             never-copy or absent from the snapshot); nothing is claimed"
                .to_string(),
            false,
        ),
        restore::Verdict::ReadbackIncomplete => (
            "Read-back: INCOMPLETE — the camera could not be read back well enough to verify the writes; \
             nothing is claimed"
                .to_string(),
            false,
        ),
        restore::Verdict::ResidualFound => {
            let r = v.residual.clone().unwrap_or_default();
            let mut names: Vec<String> = r.changed.iter().map(|n| format!("{n} (differs)")).collect();
            names.extend(r.missing.iter().map(|n| format!("{n} (not readable)")));
            (
                format!(
                    "Read-back: UNVERIFIED — {} not read back as restored: {}. \
                     The camera accepted the write and did not keep it.",
                    if names.len() == 1 {
                        "1 written property does".to_string()
                    } else {
                        format!("{} written properties do", names.len())
                    },
                    names.join(", ")
                ),
                true,
            )
        }
    }
}

/// The digest and summary an apply is held to: the plan a person approved and presented
/// (`--expect-digest`, `--expect-summary`), else the one just computed, whose approval is the
/// running of the command itself.
fn transplant_confirmation<'a>(args: &'a TransplantArgs, digest: &'a str, summary: &'a str) -> (&'a str, &'a str) {
    (args.expect_digest.as_deref().unwrap_or(digest), args.expect_summary.as_deref().unwrap_or(summary))
}

/// A transplant apply refused before any write: its message with the hint for its tag (unless
/// `json`: the caller is a program, which cannot pass `--allow-partial` for the person), and for a
/// stale plan the status a caller tests (`EXIT_STALE_PLAN`) instead of reading the message.
fn transplant_refusal(e: nikon_fleet::ops::apply::ApplyError, json: bool) -> anyhow::Error {
    use nikon_fleet::ops::apply::{ApplyError, EXIT_STALE_PLAN};
    let hint = if json { None } else { cli_report::refusal_hint(e.tag()) };
    let message = format!("{e}{}", hint.map(|h| format!("\n{h}")).unwrap_or_default());
    match e {
        ApplyError::StalePlan { .. } => ExitCodeError { code: EXIT_STALE_PLAN, message }.into(),
        _ => anyhow::anyhow!(message),
    }
}

/// The result of `apply_transplant`: an `ApplyError` is a refusal before any write; a report in hand
/// means the camera may have been written, which `report_in_hand` records for `transplant_exit`.
fn after_apply<R>(result: Result<R, nikon_fleet::ops::apply::ApplyError>, json: bool, report_in_hand: &mut bool) -> Result<R> {
    let report = result.map_err(|e| transplant_refusal(e, json))?;
    *report_in_hand = true;
    Ok(report)
}

/// How a failed `transplant` ends the process. An apply that failed before it had a report in hand
/// (`wrote_nothing`: every `ApplyError` is raised before the first write) exits
/// `EXIT_REFUSED_BEFORE_WRITE`, so a caller can tell "nothing was written" from "this may have
/// written"; a stale plan keeps its own status. A dry run and an apply that had written keep the
/// plain failure, 1.
fn transplant_exit(e: anyhow::Error, dry_run: bool, wrote_nothing: bool) -> anyhow::Error {
    if dry_run || !wrote_nothing || e.downcast_ref::<ExitCodeError>().is_some() {
        return e;
    }
    ExitCodeError { code: nikon_fleet::ops::apply::EXIT_REFUSED_BEFORE_WRITE, message: format!("{e:?}") }.into()
}

/// Copy a snapshot's settings onto a live camera that may be a different
/// model, translating enum/range values via `nikon_fleet::transplant`
/// instead of `restore`'s literal same-model copy. See that module's docs
/// for the translation strategy and why capability *codes* (not menu
/// position, not per-model renumbering) are the right join key here.
fn cmd_transplant(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &TransplantArgs, no_usb_reset: bool) -> Result<()> {
    // Set once `apply_transplant` has returned a report: from then on the camera may have been written.
    let mut report_in_hand = false;
    transplant_run(data_dir, bundle, schema_path, args, no_usb_reset, &mut report_in_hand)
        .map_err(|e| transplant_exit(e, args.dry_run, !report_in_hand))
}

fn transplant_run(
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    args: &TransplantArgs,
    no_usb_reset: bool,
    report_in_hand: &mut bool,
) -> Result<()> {
    use nikon_fleet::ops::apply::{ApplyRequest, apply_transplant};
    use nikon_fleet::ops::baseline::GatePolicy;
    use nikon_fleet::ops::plan::{PlanInput, build_plan};

    let json = args.json;
    let mut json_out = nikon_fleet::json_out::JsonOut::redirect(json)?;
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let source_bytes = fs::read(&snap_path).with_context(|| format!("loading {}", snap_path.display()))?;
    let snap = Snapshot::from_json(&String::from_utf8(source_bytes.clone()).with_context(|| format!("loading {}", snap_path.display()))?)
        .with_context(|| format!("loading {}", snap_path.display()))?;

    // Validated before the camera is touched: a mistyped opt-in must fail
    // here, not run as a silently safe transplant.
    let options = transplant::TransplantOptions {
        allow_raw_code: transplant::resolve_allow_raw_code(&args.allow_raw_code, &snap.properties)
            .map_err(|e| anyhow::anyhow!(e))?,
    };

    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    // Unlike cmd_snapshot/cmd_check (select_device's other callers, where
    // "just pick the first available one" is a reasonable default for a
    // read-only command), transplant is a destructive cross-model write —
    // select_device's own --serial-omitted behavior of silently picking the
    // first available device is exactly wrong here when there's more than
    // one real candidate. Reject the ambiguity explicitly instead of
    // silently guessing which physical camera gets written to.
    if transplant_target_ambiguous(&enriched, args.serial.as_deref()) {
        let available = enriched.iter().filter(|(d, _)| d.available).count();
        bail!(
            "{available} Nikon cameras are available; pass --serial to choose the transplant \
             target (transplant writes to physical camera hardware — refusing to guess which one)"
        );
    }
    let (target, usb_info) = select_device(enriched, args.serial.as_deref())?;

    // Before connecting: refuse non-Z-series models and build the per-code
    // name check. Needs the schema (MaidLayer.config) — unlike restore,
    // transplant can't run without it.
    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let guard = transplant::CodeNameGuard::new(&schema, &snap.camera.model, &target.name)?;
    let name_lookup = schema.name_map_for_model(&target.name);

    if target.name == snap.camera.model {
        say(
            json,
            &format!(
                "note: source and target are both {:?} — `fleet restore` is the simpler/faster tool for a \
                 same-model copy. Like transplant, it leaves out settings that read as a different \
                 position per body (ShootingLimit) when the snapshot is of another body.\n",
                target.name
            ),
        );
    }

    say(json, &format!("Connecting to {:?} (id={})…\n", target.name, target.id));
    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;

    let identity = target_identity(&target, &usb_info);
    let input = PlanInput { source: &snap, source_bytes: &source_bytes, target: &identity, options: &options };
    let plan = build_plan(&device, &guard, &input);

    if args.dry_run {
        let text = cli_report::render_transplant(&plan.outcomes, true);
        for w in &text.warnings {
            eprintln!("{w}");
        }
        say(json, &text.stdout);
        say(json, &format!("Plan: {}\nDigest: {}\n", plan.summary, plan.digest));
        if json {
            json_out.emit(&serde_json::to_string_pretty(&plan)?)?;
        }
        return Ok(());
    }

    // The same sequence the MCP's transplant_apply runs: recompute the plan, take a restore point,
    // write, read back. Unless the caller presents a plan it approved (--expect-digest, the way
    // the GUI does), the plan above is this command's own and its digest and summary are what
    // apply is given: running the command is the consent, the digest only guards the gap between
    // the two reads.
    let snapshots = snapshots_dir(data_dir);
    let confirmation = transplant_confirmation(args, &plan.digest, &plan.summary);
    let req = ApplyRequest {
        plan: input,
        guard: &guard,
        snapshots_dir: &snapshots,
        name_lookup: &name_lookup,
        expected_digest: confirmation.0,
        expected_summary: confirmation.1,
        captured_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        gate_policy: if args.allow_partial { GatePolicy::AllowPartial } else { GatePolicy::Strict },
    };
    let report = after_apply(apply_transplant(&device, &req, &mut |_| {}), json, report_in_hand)?;

    let text = cli_report::render_transplant(&report.outcomes, false);
    for w in &text.warnings {
        eprintln!("{w}");
    }
    say(json, &text.stdout);
    let closing = cli_report::render_transplant_apply(&report);
    say(json, &closing.stdout);
    if json {
        json_out.emit(&serde_json::to_string_pretty(&report)?)?;
    }
    if let Some(msg) = closing.failing {
        bail!("{msg}");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────

/// An error that carries the process exit status it should end the run with.
#[derive(Debug)]
struct ExitCodeError {
    code: i32,
    message: String,
}

impl std::fmt::Display for ExitCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitCodeError {}

/// The archive error inside `e`, however it was wrapped: raised directly, inside a fetch
/// (`FetchError::Archive`), or inside a browse (`BrowseError::Ingest`).
fn firmware_error_in(e: &anyhow::Error) -> Option<&FirmwareError> {
    if let Some(f) = e.downcast_ref::<FirmwareError>() {
        return Some(f);
    }
    match (e.downcast_ref::<FetchError>(), e.downcast_ref::<BrowseError>()) {
        (Some(FetchError::Archive(f)), _) => Some(f),
        (_, Some(BrowseError::Ingest { error, .. })) => Some(error),
        _ => None,
    }
}

/// This model and version is already archived and the command refused to change it: the typed
/// `FirmwareError`s for "already there" (`AlreadyArchived`: `add` without `--force`;
/// `ContentMismatch`: same version, different bytes).
fn is_already_archived(e: &anyhow::Error) -> bool {
    matches!(
        firmware_error_in(e),
        Some(FirmwareError::AlreadyArchived { .. } | FirmwareError::ContentMismatch { .. })
    )
}

/// The process exit status for a failed run: 3 for "already archived" (`EXIT_ALREADY_ARCHIVED`),
/// 4 for an indeterminate FTP-profile write (`EXIT_WRITE_INDETERMINATE`), a status an error
/// carries itself (5 for a stale transplant plan, `EXIT_STALE_PLAN`), else 1.
fn exit_code_for(e: &anyhow::Error) -> i32 {
    if let Some(x) = e.downcast_ref::<ExitCodeError>() {
        x.code
    } else if is_already_archived(e) {
        EXIT_ALREADY_ARCHIVED
    } else if matches!(
        e.downcast_ref::<nikon_fleet::ptp_usb::PtpUsbError>(),
        Some(nikon_fleet::ptp_usb::PtpUsbError::FtpWriteIndeterminate { .. })
    ) {
        EXIT_WRITE_INDETERMINATE
    } else {
        1
    }
}

fn cmd_settings(sub: &SettingsCmd) -> Result<()> {
    use nikon_fleet::settings::{resolve, settings_path, update_in};
    let path = settings_path();
    match sub {
        SettingsCmd::Show { json } => {
            let r = resolve(&path)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("settings file:  {}{}", r.settings_path, if r.settings_exist { "" } else { "  (does not exist)" });
                println!("data_dir:       {}", r.data_dir.as_deref().unwrap_or("(unset)"));
                println!("in use by GUIs: {}", r.effective_data_dir);
                println!("the CLI uses --data-dir, default `.`; pass --data-dir with the line above to share it");
            }
        }
        SettingsCmd::DataDir => println!("{}", resolve(&path)?.effective_data_dir),
        SettingsCmd::SetDataDir { dir, unset } => {
            let new = match (dir, unset) {
                (Some(d), false) => {
                    if !d.is_absolute() {
                        bail!("{} is not an absolute path; a relative data_dir would mean a different place to each program", d.display());
                    }
                    Some(d.to_str().with_context(|| format!("{} is not valid UTF-8", d.display()))?.to_string())
                }
                (None, true) => None,
                _ => bail!("give a directory, or --unset to clear the setting"),
            };
            let s = update_in(&path, |s| s.set_data_dir(new.as_deref()))?;
            println!("data_dir: {}  ({})", s.data_dir().unwrap_or("(unset)"), path.display());
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let no_reset = cli.no_usb_reset;
    match &cli.cmd {
        Cmd::Discover(a) => cmd_discover(&cli.sdk_bundle, a, no_reset),
        Cmd::Snapshot(args) => cmd_snapshot(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Check(args) => cmd_check(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Diff(args) => cmd_diff(&cli.data_dir, &cli.schema, args),
        Cmd::Ls(args) => cmd_ls(&cli.data_dir, args),
        Cmd::Show(args) => cmd_show(&cli.data_dir, &cli.schema, args),
        Cmd::Rm(args) => cmd_rm(&cli.data_dir, args),
        Cmd::Ref(sub) => cmd_ref(&cli.data_dir, sub),
        Cmd::Baseline(sub) => cmd_baseline(sub),
        Cmd::Settings(sub) => cmd_settings(sub),
        #[cfg(unix)]
        Cmd::Worker(args) => {
            let code = worker_cmd::run(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args);
            std::process::exit(code)
        }
        Cmd::Restore(args) => cmd_restore(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Transplant(args) => cmd_transplant(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::VendorRead(args) => cmd_vendor_read(args),
        Cmd::DeviceInfo(args) => cmd_device_info(args),
        Cmd::VendorWriteFtp(args) => cmd_vendor_write_ftp(args),
        Cmd::Firmware(sub) => match sub {
            FirmwareCmd::Add(args) => cmd_firmware_add(&cli.data_dir, args),
            FirmwareCmd::Ls(args) => cmd_firmware_ls(&cli.data_dir, args),
            FirmwareCmd::Pin(args) => cmd_firmware_pin(&cli.data_dir, args),
            FirmwareCmd::Rollback(args) => cmd_firmware_rollback(&cli.data_dir, &cli.schema, args),
            FirmwareCmd::Check(args) => cmd_firmware_check(&cli.data_dir, args),
            FirmwareCmd::Latest(args) => cmd_firmware_latest(&cli.data_dir, args),
            FirmwareCmd::Fetch(args) => cmd_firmware_fetch(&cli.data_dir, args),
            FirmwareCmd::Browse(args) => cmd_firmware_browse(&cli.data_dir, args),
        },
    }
}

fn main() {
    let cli = Cli::parse();
    // Test hook, not an option: with this variable set the command line is parsed and nothing runs.
    // tests/gui_cli_argv_parity.rs feeds it every argv the Python GUI builds. An environment
    // variable rather than a flag, so no wrapper script can turn a real command into a silent
    // exit 0 by adding an argument.
    if std::env::var_os("NIKON_FLEET_PARSE_ONLY").is_some_and(|v| v == "1") {
        return;
    }
    if let Err(e) = run(cli) {
        // Exactly what returning the error from `main` printed, with the exit status chosen
        // by `exit_code_for` instead of always 1.
        eprintln!("Error: {e:?}");
        std::process::exit(exit_code_for(&e));
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nikon_fleet::ops::list::reference_filename;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use serde_json::json;

    // ── the identity of the body being written to ────────────────────────

    fn device(id: u32) -> DeviceInfo {
        DeviceInfo { id, name: "Z6_3".into(), available: true, connected_pid: 0, version: "2.00".into() }
    }

    #[test]
    fn the_target_identity_is_the_bodys_own_never_the_snapshots() {
        // restore --serial reaches a body that has a USB record: its serial and firmware are its own.
        let own = target_identity(&device(3), &Some(usb("0000003023668")));
        assert_eq!((own.serial.as_str(), own.firmware.as_str(), own.model.as_str()), ("0000003023668", "5.31", "Z6_3"));
        // No USB record (the `id-N` and SDK-id forms): the id fallback, which no snapshot's real
        // serial can equal, so the restore reads as another body's and ShootingLimit is not written.
        let none = target_identity(&device(3), &None);
        assert_eq!((none.serial.as_str(), none.firmware.as_str()), ("id-3", "2.00"));
        // An unreadable descriptor (Some("")) is not a serial either.
        let empty = target_identity(&device(3), &Some(usb("")));
        assert_eq!(empty.serial, "id-3");
    }

    #[test]
    fn a_restore_onto_a_body_with_no_usb_record_is_never_the_snapshots_own_body() {
        use nikon_fleet::ops::restore::Source;
        let snapshot_serial = "0000003023668";
        let identity = target_identity(&device(3), &None);
        assert_eq!(Source::of(snapshot_serial, &identity.serial), Source::OtherBody);
        // With the descriptor read and equal, it is the same body.
        let identity = target_identity(&device(3), &Some(usb(snapshot_serial)));
        assert_eq!(Source::of(snapshot_serial, &identity.serial), Source::SameBody);
        // With the descriptor unreadable, a body's own snapshot is not provably its own: fail closed.
        let identity = target_identity(&device(3), &Some(usb("")));
        assert_eq!(Source::of(snapshot_serial, &identity.serial), Source::OtherBody);
    }

    // ── firmware latest --json shape ─────────────────────────────────────

    // ── reference_filename parity ────────────────────────────────────────
    // Python GUI uses: f"{model.replace(' ', '_')}_{serial}.json"
    // This test pins that contract so CLI and GUI produce identical paths.
    // A mismatch breaks `fleet check` because the reference file cannot be found.

    #[test]
    fn reference_filename_spaces_become_underscores() {
        assert_eq!(reference_filename("Z 9",   "30012345"), "Z_9_30012345.json");
        assert_eq!(reference_filename("Z 6III", "30098765"), "Z_6III_30098765.json");
        assert_eq!(reference_filename("Z 30",  "30011111"), "Z_30_30011111.json");
    }

    #[test]
    fn reference_filename_no_spaces_unchanged() {
        assert_eq!(reference_filename("Z6_3", "30056789"), "Z6_3_30056789.json");
    }

    // ── resolve_snapshot_path ─────────────────────────────────────────────
    // "Snapshots are addressable by absolute path OR by basename inside
    // snapshots/" — the is_absolute() branch is safe to test directly, but
    // the p.exists() branch depends on the process's current directory, so
    // those two cases are serialized behind CWD_LOCK to avoid racing other
    // tests in this binary that also touch cwd.

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_snapshot_path_absolute_passthrough_regardless_of_existence() {
        let data_dir = Path::new("/some/data-dir");
        let abs = Path::new("/somewhere/else/snap.json");
        assert_eq!(resolve_snapshot_path(data_dir, abs), abs.to_path_buf());
    }

    #[test]
    fn resolve_snapshot_path_relative_nonexistent_joins_snapshots_dir() {
        let _guard = CWD_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let data_dir = tmp.path().join("data");
        let resolved = resolve_snapshot_path(&data_dir, Path::new("missing.json"));
        std::env::set_current_dir(&prev).unwrap();
        assert_eq!(resolved, snapshots_dir(&data_dir).join("missing.json"));
    }

    #[test]
    fn resolve_snapshot_path_relative_existing_in_cwd_passthrough() {
        // Ambiguous case pinned: a relative path that happens to exist
        // relative to cwd is returned as-is, NOT joined under snapshots/ —
        // even though it wasn't found there. This is the p.exists() branch.
        let _guard = CWD_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        fs::write(tmp.path().join("here.json"), "{}").unwrap();
        let data_dir = tmp.path().join("data");
        let resolved = resolve_snapshot_path(&data_dir, Path::new("here.json"));
        std::env::set_current_dir(&prev).unwrap();
        assert_eq!(resolved, PathBuf::from("here.json"));
    }

    // ── select_device (the serial rule itself is ops::target's) ──────────

    fn dev(id: u32) -> DeviceInfo {
        DeviceInfo { id, name: "Z 9".into(), available: true, connected_pid: 0, version: "5.31".into() }
    }
    fn dev_unavailable(id: u32) -> DeviceInfo {
        DeviceInfo { available: false, ..dev(id) }
    }
    fn usb(serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: "Z 9".into() }
    }

    // ── select_device ─────────────────────────────────────────────────────
    // Shared by cmd_snapshot and cmd_check (previously two identical copies
    // of this match).

    #[test]
    fn select_device_by_serial_finds_match() {
        let enriched = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("BBB")))];
        let (d, _) = select_device(enriched, Some("BBB")).unwrap();
        assert_eq!(d.id, 2);
    }

    #[test]
    fn select_device_by_serial_no_match_errors() {
        let enriched = vec![(dev(1), Some(usb("AAA")))];
        assert!(select_device(enriched, Some("NOPE")).is_err());
    }

    #[test]
    fn select_device_refuses_a_serial_that_names_two_bodies_instead_of_taking_the_first() {
        // Body A (SDK id 1, serial AAA) and B (SDK id 2, USB serial "1"): `--serial 1` is B's USB
        // serial and A's SDK id. The CLI's inline copy used to take A (first match).
        let enriched = vec![(dev(1), Some(usb("AAA"))), (dev(2), Some(usb("1")))];
        let err = select_device(enriched.clone(), Some("1")).unwrap_err().to_string();
        assert!(err.contains("USB serial of one attached camera") && err.contains("SDK id of another"), "{err}");
        // The same call through the one rule every front end uses, so they cannot differ.
        assert_eq!(
            select_by_cli_serial(enriched.clone(), "1"),
            Err(TargetError::AmbiguousSerial { requested: "1".into() })
        );
        // A name that is unambiguous works in every form.
        assert_eq!(select_device(enriched.clone(), Some("AAA")).unwrap().0.id, 1);
        assert_eq!(select_device(enriched.clone(), Some("id-2")).unwrap().0.id, 2);
        let missing = select_device(enriched, Some("NOPE")).unwrap_err().to_string();
        assert!(missing.contains("no device with serial NOPE") && missing.contains("discover"), "{missing}");
    }

    #[test]
    fn select_device_none_picks_first_available() {
        // The first entry is unavailable — must skip it, not just take index 0.
        let enriched = vec![(dev_unavailable(1), None), (dev(2), None)];
        let (d, _) = select_device(enriched, None).unwrap();
        assert_eq!(d.id, 2);
    }

    #[test]
    fn select_device_none_falls_back_to_first_when_none_available() {
        // Boundary: no available device at all — falls back to index 0
        // rather than erroring (matches the prior inline behavior).
        let enriched = vec![(dev_unavailable(1), None), (dev_unavailable(2), None)];
        let (d, _) = select_device(enriched, None).unwrap();
        assert_eq!(d.id, 1);
    }

    // ── transplant_target_ambiguous ──────────────────────────────────────

    #[test]
    fn transplant_ambiguous_two_available_no_serial() {
        let enriched = vec![(dev(1), None), (dev(2), None)];
        assert!(transplant_target_ambiguous(&enriched, None));
    }

    #[test]
    fn transplant_not_ambiguous_at_exactly_one_available() {
        // Boundary: exactly 1 available candidate (plus an unavailable one)
        // is NOT ambiguous — there's only one real target to pick.
        let enriched = vec![(dev_unavailable(1), None), (dev(2), None)];
        assert!(!transplant_target_ambiguous(&enriched, None));
    }

    #[test]
    fn transplant_not_ambiguous_with_zero_available() {
        // Boundary: zero available — not "ambiguous" in the sense this check
        // guards against; select_device's own no-device-available fallback
        // behavior is unchanged by this check.
        let enriched = vec![(dev_unavailable(1), None), (dev_unavailable(2), None)];
        assert!(!transplant_target_ambiguous(&enriched, None));
    }

    #[test]
    fn transplant_not_ambiguous_when_serial_given_even_with_multiple_available() {
        // An explicit --serial disambiguates regardless of candidate count.
        let enriched = vec![(dev(1), None), (dev(2), None)];
        assert!(!transplant_target_ambiguous(&enriched, Some("AAA")));
    }

    #[test]
    fn transplant_not_ambiguous_single_candidate_total() {
        let enriched = vec![(dev(1), None)];
        assert!(!transplant_target_ambiguous(&enriched, None));
    }

    // ── strip_single_trailing_newline ───────────────────────────────────

    #[test]
    fn strip_single_trailing_newline_removes_one() {
        assert_eq!(strip_single_trailing_newline("hunter2\n".to_string()), "hunter2");
    }

    #[test]
    fn strip_single_trailing_newline_removes_only_one_not_all() {
        // A blank line before EOF is unusual but shouldn't be silently
        // collapsed away beyond the one newline echo/heredoc would add.
        assert_eq!(strip_single_trailing_newline("hunter2\n\n".to_string()), "hunter2\n");
    }

    #[test]
    fn strip_single_trailing_newline_preserves_trailing_space() {
        // A password could legitimately end in a space — must not be
        // treated the same as a trailing newline.
        assert_eq!(strip_single_trailing_newline("hunter2 \n".to_string()), "hunter2 ");
    }

    #[test]
    fn strip_single_trailing_newline_no_newline_present() {
        // e.g. `printf '%s' "$PW"` with no trailing newline at all.
        assert_eq!(strip_single_trailing_newline("hunter2".to_string()), "hunter2");
    }

    #[test]
    fn strip_single_trailing_newline_empty_string() {
        assert_eq!(strip_single_trailing_newline(String::new()), "");
    }

    // ── snapshot_capture_failed ─────────────────

    #[test]
    fn snapshot_capture_failed_zero_ok_zero_err_is_failure() {
        // Pinned from a hardware observation (Z6III, 2026-10-27): a USB reset
        // from another process during connect produced "Reading 0
        // capabilities" -> ok=0 err=0, and the old `read_ok == 0 && read_err
        // > 0` test let that through as a successful capture of an empty
        // snapshot. Nothing read is a failure regardless of the error count.
        assert!(snapshot_capture_failed(0));
    }

    #[test]
    fn ensure_capabilities_present_boundary() {
        // At the threshold (0), just above (1), and a real body's size.
        let zero = ensure_capabilities_present(0).unwrap_err().to_string();
        assert!(zero.contains("0 capabilities"), "message was: {zero}");
        assert!(zero.contains("another fleet/GUI process"), "needs an actionable hint: {zero}");
        assert!(ensure_capabilities_present(1).is_ok());
        assert!(ensure_capabilities_present(327).is_ok());
    }

    #[test]
    fn snapshot_capture_failed_zero_ok_nonzero_err_is_failure() {
        assert!(snapshot_capture_failed(0)); // with read errors alongside: same answer
    }

    #[test]
    fn snapshot_capture_failed_any_ok_is_not_failure() {
        // Boundary: even a single successful read means this isn't a total
        // failure, regardless of how many others failed.
        assert!(!snapshot_capture_failed(1)); // 99 failed reads alongside don't matter
    }

    // name_map_for_model's tests now live once, canonically, in
    // maid_layer.rs (it moved there — see MaidLayerConfig::name_map_for_model).

    // ── generate_settings_table ───────────────────────────────────────────

    const DISPLAY_SCHEMA: &str = r#"<model:Z 9>
    <version>common</version>
    <caplist>
        <capability:kNkMAIDCapability_Aperture-100>
            <description>0,0,"Aperture"</description>
            <allowedoperation:14></allowedoperation>
        </capability>
    </caplist>
</model>
"#;

    #[test]
    fn generate_settings_table_filters_volatile_properties() {
        let schema = MaidLayerConfig::parse(DISPLAY_SCHEMA).unwrap();
        let mut snap = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "ABC123".into(), firmware: "5.00".into() },
            Transport::Usb,
            "2026-01-01T00:00:00Z".into(),
        );
        snap.insert("kNkMAIDCapability_Aperture", 100, json!(56));
        snap.insert("kNkMAIDCapability_BatteryLevel", 999, json!(80));

        let table = generate_settings_table(&snap, &schema);
        assert!(table.contains("Aperture"), "non-volatile property with a display name should appear");
        assert!(!table.contains("BatteryLevel"), "volatile property must be filtered out");
    }

    #[test]
    fn generate_settings_table_falls_back_to_raw_name_without_display() {
        let schema = MaidLayerConfig::parse(DISPLAY_SCHEMA).unwrap();
        let mut snap = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "ABC123".into(), firmware: "5.00".into() },
            Transport::Usb,
            "2026-01-01T00:00:00Z".into(),
        );
        // Not present in DISPLAY_SCHEMA, so no display label is known.
        snap.insert("kNkMAIDCapability_Unknown", 777, json!("x"));

        let table = generate_settings_table(&snap, &schema);
        assert!(table.contains("kNkMAIDCapability_Unknown"));
    }

    #[test]
    fn generate_settings_table_sorts_rows_by_label() {
        let schema = MaidLayerConfig::parse("").unwrap();
        let mut snap = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "ABC123".into(), firmware: "5.00".into() },
            Transport::Usb,
            "2026-01-01T00:00:00Z".into(),
        );
        snap.insert("Zebra", 1, json!(1));
        snap.insert("Alpha", 2, json!(2));

        let table = generate_settings_table(&snap, &schema);
        let alpha_pos = table.find("Alpha").unwrap();
        let zebra_pos = table.find("Zebra").unwrap();
        assert!(alpha_pos < zebra_pos, "rows must be sorted alphabetically by label");
    }

    // ── cmd_ref ───────────────────────────────────────────────────────────

    fn write_snapshot(path: &Path, model: &str, serial: &str) {
        let s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.00".into() },
            Transport::File,
            "2026-01-01T00:00:00Z".into(),
        );
        s.save_to_file(path).unwrap();
    }

    #[test]
    fn cmd_ref_set_then_show_round_trip() {
        let data_dir = TempDir::new().unwrap();
        let snap_path = data_dir.path().join("snap.json");
        write_snapshot(&snap_path, "Z 9", "ABC123");

        cmd_ref(data_dir.path(), &RefCmd::Set { snapshot_path: snap_path.clone() }).unwrap();

        let ref_path = references_dir(data_dir.path()).join("Z_9_ABC123.json");
        assert!(ref_path.exists());

        // Show must succeed now that the reference exists.
        assert!(cmd_ref(data_dir.path(), &RefCmd::Show {
            model: "Z 9".into(), serial: "ABC123".into(),
        }).is_ok());
    }

    #[test]
    fn cmd_ref_show_missing_reference_errors() {
        let data_dir = TempDir::new().unwrap();
        let err = cmd_ref(data_dir.path(), &RefCmd::Show {
            model: "Z 9".into(), serial: "NOPE".into(),
        }).unwrap_err();
        assert!(err.to_string().contains("no reference"));
    }

    #[test]
    fn cmd_ref_list_missing_dir_ok_empty() {
        let data_dir = TempDir::new().unwrap();
        assert!(cmd_ref(data_dir.path(), &RefCmd::List { json: false }).is_ok());
    }

    // ── restore read-back report ─────────────────────────────────────────

    fn verification(verdict: restore::Verdict, changed: &[&str], missing: &[&str]) -> restore::Verification {
        restore::Verification {
            verdict,
            gate: None,
            stats: None,
            readback: None,
            residual: Some(nikon_fleet::ops::readback::Residual {
                changed: changed.iter().map(|s| s.to_string()).collect(),
                missing: missing.iter().map(|s| s.to_string()).collect(),
                unjudged: Vec::new(),
                judged: 1,
            }),
        }
    }

    #[test]
    fn restore_verification_text_fails_only_when_a_written_property_did_not_take() {
        let (ok, fail) = restore_verification_text(&verification(restore::Verdict::Verified, &[], &[]));
        assert!(!fail && ok.contains("verified"), "{ok}");
        let (none, fail) = restore_verification_text(&verification(restore::Verdict::NothingWritten, &[], &[]));
        assert!(!fail && none.contains("nothing to verify"));
        let (inc, fail) = restore_verification_text(&verification(restore::Verdict::ReadbackIncomplete, &[], &[]));
        assert!(!fail, "an incomplete read-back claims nothing and is not a failure");
        assert!(inc.contains("INCOMPLETE") && inc.contains("nothing is claimed"), "{inc}");
        let (bad, fail) = restore_verification_text(&verification(restore::Verdict::ResidualFound, &["WBTuneAuto"], &["Gone"]));
        assert!(fail);
        assert!(bad.contains("UNVERIFIED") && bad.contains("WBTuneAuto (differs)") && bad.contains("Gone (not readable)"), "{bad}");
        assert!(bad.contains("2 written properties do not"), "{bad}");
        let (one, _) = restore_verification_text(&verification(restore::Verdict::ResidualFound, &["A"], &[]));
        assert!(one.contains("1 written property does not read back"), "{one}");
    }

    // ── targeted restore verdict ─────────────────────────────────────────

    #[test]
    fn unrestored_error_is_none_only_when_everything_named_was_written() {
        assert_eq!(unrestored_error(Some(3), &[]), None);
        let one = unrestored_error(Some(1), &[("ISO".into(), "read_only")]).unwrap();
        assert!(one.contains("1 of 1 named property was not written") && one.contains("ISO (read_only)"), "{one}");
        let two = unrestored_error(Some(5), &[("A".into(), "never_copied"), ("B".into(), "write_failed")]).unwrap();
        assert!(two.contains("2 of 5 named properties were not written"), "{two}");
        assert!(two.contains("A (never_copied), B (write_failed)"), "{two}");
    }

    #[test]
    fn a_full_restore_never_fails_for_routine_skips() {
        // Without --only a restore skips absent, read-only and state properties
        // all the time; that must not become an error.
        let skips = [("A".to_string(), "read_only"), ("B".to_string(), "never_copied")];
        assert_eq!(unrestored_error(None, &skips), None);
    }

    // ── capture gate and baseline seeding ────────────────────────────────

    fn capture_from_builtin(model: &str, drop_first: usize) -> Snapshot {
        let b = nikon_fleet::ops::baseline::baseline_for(model).unwrap();
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: "1".into(), firmware: b.firmware.clone() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for c in b.readable.iter().skip(drop_first) {
            s.insert(c.name.clone(), c.code, serde_json::json!(1));
        }
        s
    }

    #[test]
    fn capture_gate_passes_a_complete_capture_and_refuses_an_incomplete_one() {
        for model in ["Z 9", "Z6_3"] {
            assert!(apply_capture_gate(&capture_from_builtin(model, 0), false).is_ok(), "{model} complete");
            let err = apply_capture_gate(&capture_from_builtin(model, 1), false).unwrap_err().to_string();
            assert!(err.contains("incomplete") && err.contains("Nothing was written") && err.contains("--allow-partial"), "{err}");
            assert!(err.contains("1 capability "), "exactly one missing: {err}");
        }
    }

    #[test]
    fn capture_gate_allow_partial_lets_an_incomplete_capture_through() {
        assert!(apply_capture_gate(&capture_from_builtin("Z 9", 1), true).is_ok());
        // Nearly nothing read (the ok=18 shape from F2) is still only allowed on request.
        let tiny = capture_from_builtin("Z 9", 178);
        assert!(apply_capture_gate(&tiny, false).is_err());
        assert!(apply_capture_gate(&tiny, true).is_ok());
    }

    #[test]
    fn capture_gate_does_not_judge_an_unknown_model() {
        let mut unknown = capture_from_builtin("Z 9", 0);
        unknown.camera.model = "Z 5".into();
        unknown.properties.clear();
        assert!(apply_capture_gate(&unknown, false).is_ok(), "no baseline: allowed, with a note");
    }

    #[test]
    fn a_capture_of_new_firmware_that_reads_everything_is_allowed_with_a_note() {
        // The baseline is out of date (extras appeared), but nothing it requires is missing.
        let mut newer = capture_from_builtin("Z 9", 0);
        newer.camera.firmware = "9.99".into();
        newer.insert("kNkMAIDCapability_NewInNineNineNine", 999_999, serde_json::json!(1));
        assert!(apply_capture_gate(&newer, false).is_ok());
    }

    #[test]
    fn a_capture_of_new_firmware_that_lacks_baseline_capabilities_is_not_kept_silently() {
        // Cannot tell a firmware that dropped capabilities from an interrupted capture or a USB
        // reset (the ok=18 err=194 shape): it used to be written with only a stderr note.
        let mut newer = capture_from_builtin("Z 9", 5);
        newer.camera.firmware = "9.99".into();
        let err = apply_capture_gate(&newer, false).unwrap_err().to_string();
        assert!(err.contains("firmware 9.99") && err.contains("5.31") && err.contains("5 capabilities"), "{err}");
        assert!(err.contains("Nothing was written") && err.contains("--allow-partial") && err.contains("re-seed"), "{err}");
        // One missing is enough (threshold: at one, not only at many).
        let mut one = capture_from_builtin("Z 9", 1);
        one.camera.firmware = "9.99".into();
        assert!(apply_capture_gate(&one, false).unwrap_err().to_string().contains("1 capability "));
        // The deliberate way through, which still names the shortfall on stderr.
        assert!(apply_capture_gate(&newer, true).is_ok());
    }

    #[test]
    fn an_incomplete_capture_with_unknown_firmware_fails_like_any_incomplete_capture() {
        // `DeviceInfo.version` is always "", so a body with no USB record is captured with
        // firmware "": that is no evidence the firmware changed, and it used to land in
        // NoBaseline(FirmwareChanged) and be saved.
        let mut unknown_fw = capture_from_builtin("Z 9", 3);
        unknown_fw.camera.firmware = String::new();
        let err = apply_capture_gate(&unknown_fw, false).unwrap_err().to_string();
        assert!(err.contains("incomplete") && err.contains("3 capabilities") && err.contains("firmware unknown"), "{err}");
        assert!(apply_capture_gate(&unknown_fw, true).is_ok());
        // Complete with unknown firmware is fine.
        let mut complete = capture_from_builtin("Z 9", 0);
        complete.camera.firmware = String::new();
        assert!(apply_capture_gate(&complete, false).is_ok());
    }

    fn write_seed_source(dir: &Path, model: &str, firmware: &str, codes: &[u32]) -> PathBuf {
        write_named_seed_source(dir, "src_snapshot.json", model, firmware, codes)
    }

    fn write_named_seed_source(dir: &Path, name: &str, model: &str, firmware: &str, codes: &[u32]) -> PathBuf {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: "1".into(), firmware: firmware.into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for c in codes {
            s.insert(format!("kNkMAIDCapability_C{c}"), *c, serde_json::json!(1));
        }
        let p = dir.join(name);
        s.save_to_file(&p).unwrap();
        p
    }

    #[test]
    fn seed_baseline_writes_model_slug_json_that_loads_back() {
        let d = TempDir::new().unwrap();
        let src = write_seed_source(d.path(), "Z 9", "5.31", &[3, 1, 2]);
        let out = d.path().join("baselines");
        let seeded = seed_baseline(&src, &out, false, false).unwrap();
        assert_eq!(seeded.path, out.join("Z_9.json"));
        assert_eq!(fs::read_to_string(&seeded.path).unwrap(), seeded.json);
        let b = nikon_fleet::ops::baseline::Baseline::from_json(&seeded.json).unwrap();
        assert_eq!((b.model.as_str(), b.firmware.as_str(), b.readable.len()), ("Z 9", "5.31", 3));
        assert_eq!(b.seeded_from, "src_snapshot.json");
    }

    #[test]
    fn seed_baseline_refuses_to_replace_without_force_and_leaves_the_file_alone() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        let first = write_seed_source(d.path(), "Z 9", "5.31", &[1, 2]);
        let path = seed_baseline(&first, &out, false, false).unwrap().path;
        let before = fs::read_to_string(&path).unwrap();
        let second = write_seed_source(d.path(), "Z 9", "5.40", &[1, 2, 3]);
        let err = seed_baseline(&second, &out, false, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "{err:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        seed_baseline(&second, &out, true, false).unwrap();
        let replaced = fs::read_to_string(&path).unwrap();
        assert!(replaced.contains("5.40") && replaced != before);
    }

    #[test]
    fn seed_baseline_refuses_a_useless_or_unsafe_source() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        let empty = write_seed_source(d.path(), "Z 9", "5.31", &[]);
        assert!(seed_baseline(&empty, &out, false, false).is_err(), "no readable capabilities");
        let no_fw = write_seed_source(d.path(), "Z 9", "", &[1]);
        assert!(seed_baseline(&no_fw, &out, false, false).is_err(), "no firmware");
        let hostile = write_seed_source(d.path(), "../evil", "1.00", &[1]);
        assert!(seed_baseline(&hostile, &out, false, false).is_err(), "model that is not a file name");
        assert!(!d.path().join("evil.json").exists() && !out.join("evil.json").exists());
    }

    // ── --force must not shrink a baseline unless told to ────────────────

    fn seeded_file(d: &Path) -> (PathBuf, PathBuf, String) {
        let out = d.join("baselines");
        let first = write_named_seed_source(d, "first.json", "Z 9", "5.31", &[1, 2, 3]);
        let path = seed_baseline(&first, &out, false, false).unwrap().path;
        let before = fs::read_to_string(&path).unwrap();
        (out, path, before)
    }

    #[test]
    fn force_refuses_a_replacement_that_drops_a_capability_and_leaves_the_file_alone() {
        let d = TempDir::new().unwrap();
        let (out, path, before) = seeded_file(d.path());
        // A partial capture (or a restore point) of the same model: 3 is gone.
        let partial = write_named_seed_source(d.path(), "partial.json", "Z 9", "5.31", &[1, 2]);
        let err = format!("{:#}", seed_baseline(&partial, &out, true, false).unwrap_err());
        assert!(err.contains("no longer lists 1 capability") && err.contains("C3 (0x3)") && err.contains("--allow-shrink"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "the existing baseline is untouched");
        // Same size, different set: still loses one.
        let swapped = write_named_seed_source(d.path(), "swapped.json", "Z 9", "5.31", &[1, 2, 9]);
        assert!(seed_baseline(&swapped, &out, true, false).is_err());
        // The second flag is the deliberate way through.
        seed_baseline(&partial, &out, true, true).unwrap();
        assert_ne!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn force_allows_a_replacement_that_keeps_or_grows_the_set() {
        let d = TempDir::new().unwrap();
        let (out, path, before) = seeded_file(d.path());
        let same = write_named_seed_source(d.path(), "same.json", "Z 9", "5.40", &[1, 2, 3]);
        seed_baseline(&same, &out, true, false).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("5.40"));
        let grown = write_named_seed_source(d.path(), "grown.json", "Z 9", "5.50", &[1, 2, 3, 4]);
        seed_baseline(&grown, &out, true, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("5.50") && text != before);
    }

    #[test]
    fn force_on_a_model_with_no_file_yet_is_just_a_write() {
        let d = TempDir::new().unwrap();
        let src = write_seed_source(d.path(), "Z 9", "5.31", &[1]);
        let seeded = seed_baseline(&src, &d.path().join("b"), true, false).unwrap();
        assert!(seeded.path.exists());
    }

    #[test]
    fn force_refuses_to_compare_against_an_existing_file_it_cannot_read() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("Z_9.json"), "{ not a baseline").unwrap();
        let src = write_seed_source(d.path(), "Z 9", "5.31", &[1]);
        let err = format!("{:#}", seed_baseline(&src, &out, true, false).unwrap_err());
        assert!(err.contains("cannot be read as a baseline") && err.contains("--allow-shrink"), "{err}");
        assert_eq!(fs::read_to_string(out.join("Z_9.json")).unwrap(), "{ not a baseline");
        seed_baseline(&src, &out, true, true).unwrap();
        assert!(fs::read_to_string(out.join("Z_9.json")).unwrap().contains("5.31"));
    }

    #[test]
    fn force_refuses_to_replace_another_models_baseline_that_slugs_to_the_same_file() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        let first = write_named_seed_source(d.path(), "a.json", "Z 9", "5.31", &[1]);
        let path = seed_baseline(&first, &out, false, false).unwrap().path;
        let before = fs::read_to_string(&path).unwrap();
        // "Z_9" slugs to the same Z_9.json as "Z 9" (firmware::slug_collision_pinned).
        let other = write_named_seed_source(d.path(), "b.json", "Z_9", "5.31", &[1, 2]);
        let err = format!("{:#}", seed_baseline(&other, &out, true, false).unwrap_err());
        assert!(err.contains("not \"Z_9\"") && err.contains("--allow-shrink"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    // ── what the user is told after seeding ──────────────────────────────

    #[test]
    fn seed_next_step_says_whether_the_file_is_embedded_stale_or_never_read() {
        let (file, text) = ("Z_9.json", "stale text");
        let p = Path::new("somewhere").join(file);
        assert!(seed_next_step(&p, text).contains("rebuild to use it"), "registered, different bytes");
        let unregistered = Path::new("baselines/Z_5.json");
        let t = seed_next_step(unregistered, "{}");
        assert!(t.starts_with("NOT ACTIVE") && t.contains("BUILTIN_SOURCES") && t.contains("Z_5.json"), "{t}");
        // The shipped file's own text is "already embedded".
        let shipped = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines/Z_9.json")).unwrap();
        assert!(seed_next_step(&p, &shipped).contains("already embeds"), "registered, identical bytes");
    }

    // ── looks_like_bcd_version ───────────────────────────────────────────

    #[test]
    fn looks_like_bcd_version_accepts_exact_shape() {
        assert!(looks_like_bcd_version("5.30"));
        assert!(looks_like_bcd_version("10.00"));
        assert!(looks_like_bcd_version("0.01"));
    }

    #[test]
    fn looks_like_bcd_version_rejects_unpadded_minor() {
        // The exact typo the finding this closes was about: "5.3" instead
        // of "5.30" silently never matches a live camera's firmware string.
        assert!(!looks_like_bcd_version("5.3"));
    }

    #[test]
    fn looks_like_bcd_version_rejects_three_digit_minor() {
        assert!(!looks_like_bcd_version("5.300"));
    }

    #[test]
    fn looks_like_bcd_version_rejects_non_numeric() {
        assert!(!looks_like_bcd_version("common"));
        assert!(!looks_like_bcd_version("5.3a"));
        assert!(!looks_like_bcd_version(""));
    }

    #[test]
    fn looks_like_bcd_version_rejects_no_dot() {
        assert!(!looks_like_bcd_version("530"));
    }

    // ── cmd_firmware_add ──────────────────────────────────────────────────

    fn firmware_add_args(bin_path: PathBuf, model: &str, version: &str, force: bool) -> FirmwareAddArgs {
        FirmwareAddArgs { bin_path, model: model.into(), version: version.into(), notes: None, force }
    }

    #[test]
    fn cmd_firmware_add_missing_bin_file_errors() {
        let data_dir = TempDir::new().unwrap();
        let args = firmware_add_args(data_dir.path().join("nope.bin"), "Z 9", "5.31", false);
        let err = cmd_firmware_add(data_dir.path(), &args).unwrap_err();
        assert!(err.to_string().contains("firmware file not found"));
    }

    #[test]
    fn cmd_firmware_add_duplicate_without_force_errors() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();

        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path.clone(), "Z 9", "5.31", false)).unwrap();
        let err = cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<FirmwareError>(),
            Some(FirmwareError::AlreadyArchived { .. })
        ));
    }

    #[test]
    fn cmd_firmware_add_duplicate_with_force_overwrites() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path.clone(), "Z 9", "5.31", false)).unwrap();

        fs::write(&bin_path, b"v2-longer-content").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", true)).unwrap();

        let archived = archive_dir(data_dir.path(), "Z 9", "5.31").join("firmware.bin");
        assert_eq!(fs::read(archived).unwrap(), b"v2-longer-content");
    }

    // ── cmd_firmware_pin ──────────────────────────────────────────────────

    #[test]
    fn cmd_firmware_pin_model_mismatch_errors() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let snap_path = data_dir.path().join("snap.json");
        write_snapshot(&snap_path, "Z 8", "ABC123"); // wrong model

        let err = cmd_firmware_pin(data_dir.path(), &FirmwarePinArgs {
            snapshot_path: snap_path, model: "Z 9".into(), version: "5.31".into(),
        }).unwrap_err();
        assert!(err.to_string().contains("but --model is"));
    }

    #[test]
    fn cmd_firmware_pin_snapshot_outside_data_dir_errors() {
        // Ambiguous/boundary case: canonical_snapshot_path must be relative
        // to data_dir for portability, so a snapshot living outside it
        // can't be pinned as-is (strip_prefix fails).
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let outside = TempDir::new().unwrap();
        let snap_path = outside.path().join("snap.json");
        write_snapshot(&snap_path, "Z 9", "ABC123");

        let err = cmd_firmware_pin(data_dir.path(), &FirmwarePinArgs {
            snapshot_path: snap_path, model: "Z 9".into(), version: "5.31".into(),
        }).unwrap_err();
        assert!(err.to_string().contains("outside data-dir"));
    }

    #[test]
    fn cmd_firmware_pin_success_records_relative_path() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let snap_path = data_dir.path().join("snap.json");
        write_snapshot(&snap_path, "Z 9", "ABC123");

        cmd_firmware_pin(data_dir.path(), &FirmwarePinArgs {
            snapshot_path: snap_path, model: "Z 9".into(), version: "5.31".into(),
        }).unwrap();

        let meta = load_meta(data_dir.path(), "Z 9", "5.31").unwrap();
        assert_eq!(meta.canonical_snapshot_path.as_deref(), Some("snap.json"));
    }

    #[test]
    fn cmd_firmware_pin_relative_data_dir_dot_succeeds() {
        // Regression test: the normal-case invocation `fleet firmware pin
        // snapshots/foo.json ...` with the default `--data-dir "."` used to
        // fail with "outside data-dir" because a plain component-wise
        // strip_prefix("snapshots/foo.json", ".") doesn't match even though
        // the file genuinely is inside data_dir — only caught by canonicalizing
        // both sides first. Uses cwd (like resolve_snapshot_path's tests)
        // since data_dir="." is only meaningful relative to cwd.
        let _guard = CWD_LOCK.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let data_dir = Path::new(".");
        let bin_path = PathBuf::from("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir, &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        fs::create_dir_all("snapshots").unwrap();
        let snap_path = PathBuf::from("snapshots/foo.json");
        write_snapshot(&snap_path, "Z 9", "ABC123");

        let result = cmd_firmware_pin(data_dir, &FirmwarePinArgs {
            snapshot_path: snap_path, model: "Z 9".into(), version: "5.31".into(),
        });
        std::env::set_current_dir(&prev).unwrap();

        result.unwrap();
        let meta = load_meta(Path::new(tmp.path()), "Z 9", "5.31").unwrap();
        // Stored as the path was given, so with the platform's separator.
        assert_eq!(meta.canonical_snapshot_path.as_deref(), Some(Path::new("snapshots").join("foo.json").to_str().unwrap()));
    }

    // ── cmd_firmware_rollback ─────────────────────────────────────────────

    #[test]
    fn cmd_firmware_rollback_unknown_serial_errors() {
        // --serial doesn't change bundle contents (not per-camera), so its
        // only job is catching a wrong/typo'd serial before generating
        // anything. No real camera in the test environment will ever have
        // this made-up serial, so this deterministically hits the
        // not-found path without needing hardware.
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let schema_path = data_dir.path().join("schema.config");
        fs::write(&schema_path, "").unwrap();
        let err = cmd_firmware_rollback(data_dir.path(), &schema_path, &FirmwareRollbackArgs {
            model: "Z 9".into(), version: "5.31".into(),
            serial: Some("NOT-A-REAL-CONNECTED-SERIAL-38fa2".into()), output_dir: None,
        }).unwrap_err();
        assert!(err.to_string().contains("no connected camera with serial"));
    }

    #[test]
    fn cmd_firmware_rollback_checksum_mismatch_errors() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        // Corrupt the archived binary after the fact so its hash no longer
        // matches the recorded metadata.
        fs::write(archive_dir(data_dir.path(), "Z 9", "5.31").join("firmware.bin"), b"corrupted").unwrap();

        let schema_path = data_dir.path().join("schema.config");
        fs::write(&schema_path, "").unwrap();
        let err = cmd_firmware_rollback(data_dir.path(), &schema_path, &FirmwareRollbackArgs {
            model: "Z 9".into(), version: "5.31".into(), serial: None, output_dir: None,
        }).unwrap_err();
        assert!(matches!(
            err.downcast_ref::<FirmwareError>(),
            Some(FirmwareError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn cmd_firmware_rollback_without_canonical_snapshot_still_succeeds() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let schema_path = data_dir.path().join("schema.config");
        fs::write(&schema_path, "").unwrap();
        let out_dir = data_dir.path().join("out");
        cmd_firmware_rollback(data_dir.path(), &schema_path, &FirmwareRollbackArgs {
            model: "Z 9".into(), version: "5.31".into(), serial: None,
            output_dir: Some(out_dir.clone()),
        }).unwrap();

        assert!(out_dir.join("firmware.bin").exists());
        assert!(out_dir.join("rollback-instructions.txt").exists());
        assert!(!out_dir.join("canonical_settings.json").exists());
    }

    #[test]
    fn cmd_firmware_rollback_with_canonical_snapshot_bundles_it() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        cmd_firmware_add(data_dir.path(), &firmware_add_args(bin_path, "Z 9", "5.31", false)).unwrap();

        let snap_path = data_dir.path().join("snap.json");
        write_snapshot(&snap_path, "Z 9", "ABC123");
        cmd_firmware_pin(data_dir.path(), &FirmwarePinArgs {
            snapshot_path: snap_path, model: "Z 9".into(), version: "5.31".into(),
        }).unwrap();

        let schema_path = data_dir.path().join("schema.config");
        fs::write(&schema_path, "").unwrap();
        let out_dir = data_dir.path().join("out");
        cmd_firmware_rollback(data_dir.path(), &schema_path, &FirmwareRollbackArgs {
            model: "Z 9".into(), version: "5.31".into(), serial: None,
            output_dir: Some(out_dir.clone()),
        }).unwrap();

        assert!(out_dir.join("canonical_settings.json").exists());
    }
    // ── exit status for "already archived" ───────────────────────────────

    fn already(model: &str) -> FirmwareError {
        FirmwareError::AlreadyArchived { model: model.into(), version: "5.31".into(), path: "p".into() }
    }

    fn mismatch() -> FirmwareError {
        FirmwareError::ContentMismatch { model: "Z 9".into(), version: "5.31".into(), existing: "a".into(), new: "b".into() }
    }

    #[test]
    fn exit_code_is_three_for_already_archived_however_the_error_is_wrapped() {
        let archived: [anyhow::Error; 6] = [
            already("Z 9").into(),
            mismatch().into(),
            // `fetch` raises it through FetchError::Archive (transparent), `browse` inside BrowseError::Ingest.
            FetchError::Archive(already("Z 9")).into(),
            FetchError::Archive(mismatch()).into(),
            BrowseError::Ingest { kept: PathBuf::from("k"), error: mismatch() }.into(),
            anyhow::Error::from(already("Z 9")).context("while adding"),
        ];
        for e in &archived {
            assert_eq!(exit_code_for(e), EXIT_ALREADY_ARCHIVED, "{e:?}");
        }
        assert_eq!(EXIT_ALREADY_ARCHIVED, 3, "documented in --help; scripts test for it");
    }

    #[test]
    fn every_other_error_is_exit_one_including_other_archive_errors() {
        let others: [anyhow::Error; 7] = [
            anyhow::anyhow!("no cameras"),
            FirmwareError::NotFound { model: "Z 9".into(), version: "5.31".into() }.into(),
            FirmwareError::ChecksumMismatch { stored: "a".into(), computed: "b".into() }.into(),
            FirmwareError::IncompleteArchive { path: "p".into() }.into(),
            FetchError::Gated("Z 9".into()).into(),
            BrowseError::NotCompiledIn.into(),
            BrowseError::Ingest { kept: PathBuf::from("k"), error: FirmwareError::Io(std::io::Error::other("x")) }.into(),
        ];
        for e in &others {
            assert_eq!(exit_code_for(e), 1, "{e:?}");
        }
    }

    #[test]
    fn a_fetch_exits_three_only_when_every_failure_was_already_archived() {
        let code = |failures, already, total| exit_code_for(&fetch_failure(failures, already, total));
        assert_eq!(code(1, 1, 1), EXIT_ALREADY_ARCHIVED);
        assert_eq!(code(2, 2, 2), EXIT_ALREADY_ARCHIVED);
        assert_eq!(code(2, 1, 2), 1, "one refusal and one real failure is a real failure");
        assert_eq!(code(1, 0, 2), 1);
        assert_eq!(code(2, 2, 3), EXIT_ALREADY_ARCHIVED, "models that succeeded do not matter");
        let msg = fetch_failure(2, 2, 3).to_string();
        assert!(msg.contains("2 of 3 model(s) failed"), "{msg}");
    }

    // ── discover's output, and snapshot's order of refusals ──────────────

    fn named_device(id: u32, name: &str) -> DeviceInfo {
        DeviceInfo { id, name: name.into(), available: true, connected_pid: 0, version: String::new() }
    }

    fn usb_record(serial: &str, model: &str, firmware: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 1, serial: serial.into(), firmware: firmware.into(), model: model.into() }
    }

    #[test]
    fn discover_prints_one_json_document_even_for_no_cameras() {
        let empty: serde_json::Value = serde_json::from_str(&discover_report(&[], true).unwrap()).unwrap();
        assert_eq!(empty, serde_json::json!({"cameras": []}));
        assert_eq!(discover_report(&[], false).unwrap(), "(no Nikon cameras detected)");
        let rows = [(named_device(3, "Z 9"), Some(usb_record("S9", "Z 9", "5.31"))), (named_device(4, "Z 5"), None)];
        let v: serde_json::Value = serde_json::from_str(&discover_report(&rows, true).unwrap()).unwrap();
        let cams = v["cameras"].as_array().unwrap();
        assert_eq!(cams.len(), 2);
        assert_eq!((cams[0]["serial"].as_str(), cams[0]["firmware"].as_str(), cams[0]["baseline"].as_bool()), (Some("S9"), Some("5.31"), Some(true)));
        assert_eq!((cams[1]["serial"].as_str(), cams[1]["usb_serial"].is_null(), cams[1]["baseline"].as_bool()), (Some("id-4"), true, Some(false)));
    }

    #[test]
    fn discover_text_lists_the_storable_serial_and_the_baseline() {
        let rows = [(named_device(3, "Z 9"), Some(usb_record("S9", "Z 9", "5.31"))), (named_device(4, "Z 5"), None)];
        let text = discover_report(&rows, false).unwrap();
        assert_eq!(
            text,
            "2 device(s):\n  Z 9  serial=S9  fw=5.31  id=3  baseline=yes\n  Z 5  serial=id-4  fw=  id=4  baseline=no"
        );
    }

    #[test]
    fn a_capture_that_read_nothing_is_refused_before_the_gate_even_with_allow_partial() {
        use nikon_fleet::ops::capture::CaptureStats;
        let empty = Snapshot::new(Camera { model: "Z 9".into(), serial: "S".into(), firmware: "5.31".into() }, nikon_fleet::snapshot::Transport::Usb, "t".into());
        let stats = |read_ok, read_err| CaptureStats { read_ok, read_err, ..Default::default() };
        // at the threshold: 0 reads is refused under both settings, and says how many reads failed
        for allow_partial in [false, true] {
            let err = judge_capture(&empty, &stats(0, 7), allow_partial).unwrap_err().to_string();
            assert!(err.contains("no capability could be read (7 read error(s)"), "{err}");
        }
        assert!(judge_capture(&empty, &stats(0, 0), true).is_err(), "no errors and no reads is still nothing");
        // just above: 1 read gets past this check and is the gate's to judge (a Z 9 shortfall: allowed only by --allow-partial)
        let one_read = Snapshot { ..empty.clone() };
        assert!(judge_capture(&one_read, &stats(1, 0), false).is_err(), "the gate refuses the shortfall");
        assert!(judge_capture(&one_read, &stats(1, 0), true).is_ok(), "and --allow-partial is the way past it");
        // a model with no baseline is not judged, so one read is enough
        let z5 = Snapshot::new(Camera { model: "Z 5".into(), serial: "S".into(), firmware: "1.43".into() }, nikon_fleet::snapshot::Transport::Usb, "t".into());
        assert!(judge_capture(&z5, &stats(1, 0), false).is_ok());
        assert!(judge_capture(&z5, &stats(0, 0), false).is_err());
    }

    // ── exit status for an indeterminate FTP-profile write ───────────────

    fn indeterminate() -> nikon_fleet::ptp_usb::PtpUsbError {
        nikon_fleet::ptp_usb::PtpUsbError::FtpWriteIndeterminate {
            cause: Box::new(nikon_fleet::ptp_usb::PtpUsbError::ShortWrite { written: 40, expected: 82 }),
        }
    }

    #[test]
    fn a_stale_transplant_plan_exits_five_and_every_other_refusal_exits_one() {
        use nikon_fleet::ops::apply::{ApplyError, EXIT_STALE_PLAN};
        let stale = transplant_refusal(ApplyError::StalePlan { what: "digest" }, false);
        assert_eq!(exit_code_for(&stale), EXIT_STALE_PLAN);
        assert_eq!(EXIT_STALE_PLAN, 5, "documented in --help; the GUI tests for it");
        assert!(![1, 2, EXIT_ALREADY_ARCHIVED, EXIT_WRITE_INDETERMINATE].contains(&EXIT_STALE_PLAN));
        let missing = transplant_refusal(ApplyError::MissingConfirmation { which: "digest" }, false);
        assert_eq!(exit_code_for(&missing), 1, "before transplant_exit: the refusal itself is not what picks 6");
        assert!(stale.to_string().contains("stale"), "the message still says why: {stale}");
    }

    #[test]
    fn an_apply_refused_before_it_could_write_exits_six_and_no_other_failure_does() {
        use nikon_fleet::ops::apply::{ApplyError, EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
        assert_eq!(EXIT_REFUSED_BEFORE_WRITE, 6, "documented in --help; the GUI tests for it");
        assert!(![1, 2, EXIT_ALREADY_ARCHIVED, EXIT_WRITE_INDETERMINATE, EXIT_STALE_PLAN].contains(&EXIT_REFUSED_BEFORE_WRITE));
        let boom = || anyhow::anyhow!("no Nikon cameras detected");
        // an apply with no report in hand: the camera was not written
        let refused = transplant_exit(boom(), false, true);
        assert_eq!(exit_code_for(&refused), EXIT_REFUSED_BEFORE_WRITE);
        assert!(refused.to_string().contains("no Nikon cameras detected"), "the message is kept: {refused}");
        // a failure once the report is in hand (the output could not be written, the apply did not fully take) may have written
        assert_eq!(exit_code_for(&transplant_exit(boom(), false, false)), 1);
        // a dry run never writes, and keeps the plain failure it always had
        assert_eq!(exit_code_for(&transplant_exit(boom(), true, true)), 1);
        // a stale plan keeps its own status
        let stale = transplant_refusal(ApplyError::StalePlan { what: "digest" }, true);
        assert_eq!(exit_code_for(&transplant_exit(stale, false, true)), EXIT_STALE_PLAN);
        // a cause chain survives into the message
        let chained = anyhow::Error::new(std::io::Error::other("disk gone")).context("loading snap.json");
        let text = transplant_exit(chained, false, true).to_string();
        assert!(text.contains("loading snap.json") && text.contains("disk gone"), "{text}");
    }

    #[test]
    fn a_report_in_hand_is_remembered_and_a_refusal_is_not() {
        use nikon_fleet::ops::apply::ApplyError;
        let mut in_hand = false;
        assert!(after_apply::<u8>(Err(ApplyError::StalePlan { what: "digest" }), false, &mut in_hand).is_err());
        assert!(!in_hand, "a refusal is raised before any write");
        assert_eq!(after_apply(Ok(7_u8), false, &mut in_hand).unwrap(), 7);
        assert!(in_hand, "from the report on, a failure may have followed a write");
    }

    #[test]
    fn a_json_refusal_leaves_out_the_allow_partial_advice_a_program_cannot_act_on() {
        use nikon_fleet::ops::apply::ApplyError;
        use nikon_fleet::ops::baseline::NoBaselineReason;
        let refusal = |json| transplant_refusal(ApplyError::NoBaseline(NoBaselineReason::UnknownModel), json).to_string();
        assert!(refusal(false).contains("--allow-partial"), "a person at a terminal is told");
        let for_a_program = refusal(true);
        assert!(!for_a_program.contains("--allow-partial"), "{for_a_program}");
        assert!(for_a_program.contains("nothing was written"), "the refusal itself stays: {for_a_program}");
        // a refusal with no advice is the same either way
        let same = |json| transplant_refusal(ApplyError::MissingConfirmation { which: "digest" }, json).to_string();
        assert_eq!(same(false), same(true));
    }

    #[test]
    fn an_apply_is_held_to_the_plan_it_was_given_and_only_otherwise_to_its_own() {
        use clap::Parser;
        let args = |extra: &[&str]| {
            let mut argv = vec!["nikon-fleet", "transplant", "snap.json"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().cmd {
                Cmd::Transplant(a) => a,
                _ => unreachable!(),
            }
        };
        let own = args(&[]);
        assert_eq!(transplant_confirmation(&own, "fresh-digest", "fresh summary"), ("fresh-digest", "fresh summary"));
        let given = args(&["--expect-digest", "approved-digest", "--expect-summary", "approved summary"]);
        assert_eq!(transplant_confirmation(&given, "fresh-digest", "fresh summary"), ("approved-digest", "approved summary"));
    }

    #[test]
    fn a_restore_is_held_to_the_plan_it_was_given_and_only_otherwise_to_its_own() {
        use clap::Parser;
        let args = |extra: &[&str]| {
            let mut argv = vec!["nikon-fleet", "restore", "snap.json"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().cmd {
                Cmd::Restore(a) => a,
                _ => unreachable!(),
            }
        };
        let own = args(&[]);
        assert_eq!(restore_confirmation(&own, "fresh-digest", "fresh summary"), ("fresh-digest", "fresh summary"));
        let given = args(&["--expect-digest", "approved-digest", "--expect-summary", "approved summary"]);
        assert_eq!(restore_confirmation(&given, "fresh-digest", "fresh summary"), ("approved-digest", "approved summary"));
        for bad in [&["--expect-digest", "d"][..], &["--expect-summary", "s"], &["--dry-run", "--expect-digest", "d", "--expect-summary", "s"]] {
            let mut argv = vec!["nikon-fleet", "restore", "snap.json"];
            argv.extend_from_slice(bad);
            assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_stale_restore_plan_exits_five_and_a_json_refusal_leaves_out_the_allow_partial_advice() {
        use nikon_fleet::ops::apply::{ApplyError, EXIT_STALE_PLAN};
        use nikon_fleet::ops::baseline::NoBaselineReason;
        use nikon_fleet::ops::restore_flow::{Refusal, RestoreApplyError};
        let stale = restore_refusal(RestoreApplyError::Apply(ApplyError::StalePlan { what: "summary" }), true);
        assert_eq!(exit_code_for(&stale), EXIT_STALE_PLAN);
        let no_baseline = |json| {
            restore_refusal(RestoreApplyError::Apply(ApplyError::NoBaseline(NoBaselineReason::UnknownModel)), json).to_string()
        };
        assert!(no_baseline(false).contains("--allow-partial"), "a person at a terminal is told");
        assert!(!no_baseline(true).contains("--allow-partial"), "a program cannot act on it");
        let other_body = restore_refusal(
            RestoreApplyError::Refused(Refusal::SerialMismatch { snapshot: "A".into(), target: "B".into() }),
            true,
        );
        assert_eq!(exit_code_for(&other_body), 1, "before restore_run's own mapping to 6");
        assert!(other_body.to_string().contains("transplant"), "{other_body}");
    }

    #[test]
    fn expect_digest_and_expect_summary_come_together_and_never_with_a_dry_run() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["nikon-fleet", "transplant", "snap.json"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv)
        };
        assert!(parse(&[]).is_ok());
        assert!(parse(&["--expect-digest", "d", "--expect-summary", "s"]).is_ok());
        assert!(parse(&["--expect-digest", "d"]).is_err(), "a digest alone is half a confirmation");
        assert!(parse(&["--expect-summary", "s"]).is_err(), "a summary alone is half a confirmation");
        assert!(parse(&["--dry-run", "--expect-digest", "d", "--expect-summary", "s"]).is_err(), "a dry run writes nothing, so it has nothing to confirm");
    }

    #[test]
    fn exit_code_is_four_for_an_indeterminate_write_however_the_error_is_wrapped() {
        use anyhow::Context as _;
        let wrapped: [anyhow::Error; 4] = [
            indeterminate().into(),
            anyhow::Error::from(indeterminate()).context("while writing"),
            vendor_write_error(indeterminate()),
            Err::<(), _>(indeterminate()).context("a").context("b").unwrap_err(),
        ];
        for e in &wrapped {
            assert_eq!(exit_code_for(e), EXIT_WRITE_INDETERMINATE, "{e:?}");
        }
        assert_eq!(EXIT_WRITE_INDETERMINATE, 4, "documented in --help; scripts test for it");
        assert!(![1, 2, EXIT_ALREADY_ARCHIVED].contains(&EXIT_WRITE_INDETERMINATE), "distinct from every other documented status");
    }

    #[test]
    fn a_failure_before_the_data_was_sent_is_exit_one_not_four() {
        use nikon_fleet::ptp_usb::PtpUsbError as E;
        // Including a ShortWrite that is the *cause* of an indeterminate one: bare, it is an ordinary failure.
        let plain = [
            E::NoDevice,
            E::BodiesSkipped { skipped: vec!["a".into()] },
            E::ShortWrite { written: 1, expected: 2 },
            E::SessionPoisoned { cause: "x".into() },
            E::UnconfirmedModel("Z 5".into()),
        ];
        for e in plain {
            let e = vendor_write_error(e);
            assert_eq!(exit_code_for(&e), 1, "{e:?}");
        }
    }

    #[test]
    fn the_failures_print_their_own_actionable_text() {
        use nikon_fleet::ptp_usb::PtpUsbError as E;
        let printed = |e: E| format!("{:?}", vendor_write_error(e));
        let skipped = printed(E::BodiesSkipped { skipped: vec!["bus 1 addr 4".into()] });
        assert!(skipped.contains("could not be opened to read their serial") && skipped.contains("bus 1 addr 4"), "{skipped}");
        let busy = printed(E::CameraLock(nikon_fleet::camera_lock::LockError::Busy { path: PathBuf::from("/tmp/lock") }));
        assert!(busy.contains("camera busy: another nikon-fleet session"), "{busy}");
        let indeterminate = printed(indeterminate());
        assert!(indeterminate.contains("the camera may have changed"), "{indeterminate}");
        assert!(indeterminate.contains("may or may not have taken effect"), "the cause's own text is kept: {indeterminate}");
    }

    #[test]
    fn a_reply_that_did_not_match_the_command_is_noted_and_a_clean_one_is_not() {
        use nikon_fleet::ptp_usb::FtpWriteReceipt as R;
        let note = |matched, params: &[u32]| ftp_receipt_note(&R { response_params: params.to_vec(), reply_matched_command: matched });
        assert_eq!(note(true, &[]), None, "the ordinary receipt says nothing");
        let unmatched = note(false, &[]).expect("a reply that did not match is noted");
        assert!(unmatched.contains("different transaction id") && unmatched.contains("check it in the camera's menu"), "{unmatched}");
        assert!(!unmatched.contains("parameters"), "{unmatched}");
        let params = note(true, &[7, 9]).expect("response parameters are noted");
        assert!(params.contains("[7, 9]") && !params.contains("transaction id"), "{params}");
        let both = note(false, &[1]).unwrap();
        assert!(both.contains("transaction id") && both.contains("[1]"), "{both}");
    }

    #[test]
    fn vendor_read_prints_the_data_on_stdout_and_each_warning_on_stderr() {
        use nikon_fleet::ptp_usb::VendorRead;
        let clean = vendor_read_output(0xd053, &VendorRead { data: vec![0xde, 0xad], warnings: vec![] });
        assert_eq!(clean, ("0xd053: 2 byte(s): de ad".to_string(), vec![]));
        let warned = vendor_read_output(
            0x5001,
            &VendorRead { data: vec![], warnings: vec!["model \"Z 8\" is not surveyed".into(), "second".into()] },
        );
        assert_eq!(warned.0, "0x5001: 0 byte(s): ");
        assert_eq!(warned.1, ["warning: model \"Z 8\" is not surveyed", "warning: second"], "one line per warning, none dropped");
    }

    // ── device-info output ───────────────────────────────────────────────

    fn ptp_str(text: &str) -> nikon_fleet::ptp_usb::PtpString {
        nikon_fleet::ptp_usb::PtpString { text: text.to_string(), anomalies: vec![] }
    }

    fn device_info_report(ptp_serial: &str, usb_serial: &str) -> nikon_fleet::ptp_usb::DeviceInfoReport {
        nikon_fleet::ptp_usb::DeviceInfoReport {
            dataset: nikon_fleet::ptp_usb::DeviceInfoDataset {
                standard_version: 100,
                vendor_extension_id: 0x0a,
                vendor_extension_version: 0x64,
                vendor_extension_desc: ptp_str("Nikon"),
                functional_mode: 0,
                operations_supported: vec![0x1001, 0x943b],
                events_supported: vec![0x4006],
                device_properties_supported: vec![0x5001, 0xd053, 0xd0b4],
                capture_formats: vec![0x3801],
                playback_formats: vec![0x3000, 0x3801],
                manufacturer: ptp_str("Nikon Corporation"),
                model: ptp_str("Z 9"),
                device_version: ptp_str("5.31"),
                serial_number: ptp_str(ptp_serial),
                trailing: vec![0xaa, 0xbb, 0xcc],
            },
            usb_model: "Z 9".into(),
            usb_serial: usb_serial.into(),
            usb_warnings: vec![],
        }
    }

    #[test]
    fn device_info_text_ends_with_the_serial_verdict_line_for_each_verdict() {
        for (ptp, usb, line) in [
            ("0000003023668", "0000003023668", "serial: PTP \"0000003023668\" | USB \"0000003023668\" | match"),
            ("0000003023668", "0000003023669", "serial: PTP \"0000003023668\" | USB \"0000003023669\" | differ"),
            ("", "0000003023668", "serial: PTP \"\" | USB \"0000003023668\" | ptp_empty"),
            ("0000003023668", "", "serial: PTP \"0000003023668\" | USB \"\" | usb_empty"),
            ("", "", "serial: PTP \"\" | USB \"\" | both_empty"),
        ] {
            let text = device_info_text(&device_info_report(ptp, usb));
            assert_eq!(text.lines().last(), Some(line), "{text}");
        }
    }

    #[test]
    fn device_info_text_prints_every_scalar_string_and_count_of_the_dataset() {
        let text = device_info_text(&device_info_report("0000003023668", "0000003023668"));
        for line in [
            "USB model:                 \"Z 9\"",
            "standard version:          100 (0x0064)",
            "vendor extension id:       0x0000000a",
            "vendor extension version:  100 (0x0064)",
            "vendor extension desc:     \"Nikon\"",
            "functional mode:           0x0000",
            "operations supported:      2",
            "events supported:          1",
            "device properties:         3",
            "capture formats:           1",
            "playback formats:          2",
            "manufacturer:              \"Nikon Corporation\"",
            "model:                     \"Z 9\"",
            "device version:            \"5.31\"",
            "serial number:             \"0000003023668\"",
            "trailing bytes:            3",
        ] {
            assert!(text.lines().any(|l| l == line), "{line:?} missing from:\n{text}");
        }
    }

    #[test]
    fn device_info_text_shows_a_string_anomaly_next_to_the_string() {
        use nikon_fleet::ptp_usb::StringAnomaly;
        let mut report = device_info_report("x", "x");
        report.dataset.model.anomalies = vec![StringAnomaly::UnpairedSurrogate, StringAnomaly::EmbeddedNul];
        let text = device_info_text(&report);
        assert!(
            text.lines().any(|l| l == "model:                     \"Z 9\"  [anomaly: unpaired_surrogate, embedded_nul]"),
            "{text}"
        );
        // A clean string carries no marker.
        assert!(!device_info_text(&device_info_report("x", "x")).contains("anomaly"));
    }

    #[test]
    fn device_info_json_carries_every_dataset_field_with_the_lists_in_full_as_hex() {
        let v = device_info_json(&device_info_report("0000003023668", ""));
        let info = v["device_info"].as_object().expect("device_info object");
        let mut keys: Vec<&str> = info.keys().map(String::as_str).collect();
        keys.sort_unstable();
        // The 14 DeviceInfo fields of ISO 15740, plus what the command adds about the tail.
        let mut expected = vec![
            "standard_version", "vendor_extension_id", "vendor_extension_version", "vendor_extension_desc",
            "functional_mode", "operations_supported", "events_supported", "device_properties_supported",
            "capture_formats", "playback_formats", "manufacturer", "model", "device_version", "serial_number",
            "trailing_bytes", "trailing_hex",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected, "a dataset field was dropped from (or invented in) the JSON");
        assert_eq!(info["operations_supported"], json!(["0x1001", "0x943b"]));
        assert_eq!(info["events_supported"], json!(["0x4006"]));
        assert_eq!(info["device_properties_supported"], json!(["0x5001", "0xd053", "0xd0b4"]));
        assert_eq!(info["capture_formats"], json!(["0x3801"]));
        assert_eq!(info["playback_formats"], json!(["0x3000", "0x3801"]));
        assert_eq!(info["standard_version"], json!(100));
        assert_eq!(info["vendor_extension_id"], json!(10));
        assert_eq!(info["serial_number"], json!({"text": "0000003023668", "anomalies": []}));
        assert_eq!(info["trailing_bytes"], json!(3));
        assert_eq!(info["trailing_hex"], json!("aa bb cc"));
    }

    #[test]
    fn device_info_json_carries_the_verdict_and_the_usb_identity() {
        let mut report = device_info_report("0000003023668", "");
        report.usb_warnings = vec!["reading the USB serial string failed: timeout".into()];
        let v = device_info_json(&report);
        assert_eq!(v["serial_comparison"], json!({"ptp": "0000003023668", "usb": "", "verdict": "usb_empty"}));
        assert_eq!(
            v["usb"],
            json!({"model": "Z 9", "serial": "", "warnings": ["reading the USB serial string failed: timeout"]})
        );
    }

    // ── raw-USB commands: what `--serial id-N` means ─────────────────────

    #[test]
    fn the_bus_is_scanned_only_for_an_id_n_and_that_is_where_it_can_be_refused() {
        use std::cell::Cell;
        let scans = Cell::new(0);
        let scan = |cams: Vec<UsbCameraInfo>| {
            let scans = &scans;
            move || {
                scans.set(scans.get() + 1);
                cams
            }
        };
        // Real serials and usb-<bus>:<addr> pass through with no scan at all.
        assert_eq!(vendor_target_serial(Some("0000003023668"), scan(vec![])).unwrap(), Some("0000003023668".into()));
        assert_eq!(vendor_target_serial(Some("usb-20:5"), scan(vec![])).unwrap(), Some("usb-20:5".into()));
        assert_eq!(vendor_target_serial(None, scan(vec![])).unwrap(), None);
        assert_eq!(scans.get(), 0);
        // id-N resolves for the sole serial-less body: the vendor command then takes the sole camera.
        assert_eq!(vendor_target_serial(Some("id-1"), scan(vec![usb("")])).unwrap(), None);
        assert_eq!(scans.get(), 1);
        // ... and is refused, before any camera is opened, in every other case.
        for attached in [vec![], vec![usb("AAA")], vec![usb(""), usb("")], vec![usb(""), usb("AAA")]] {
            let n = attached.len();
            let err = vendor_target_serial(Some("id-1"), scan(attached)).unwrap_err().to_string();
            assert!(err.contains("SDK id") && err.contains(&format!("{n} attached")), "{err}");
        }
    }

    #[test]
    fn the_camera_lock_is_taken_before_an_id_n_is_resolved_against_the_bus() {
        use nikon_fleet::camera_lock::LockError;
        use nikon_fleet::ptp_usb::PtpUsbError;
        use std::cell::Cell;
        // Refused lock: the bus scan (which opens devices to read descriptors) never runs, and the
        // refusal is the same typed `camera_busy` a session open gives.
        let scanned = Cell::new(false);
        let busy = || Err(LockError::Busy { path: PathBuf::from("/tmp/camera.lock") });
        let scan = || {
            scanned.set(true);
            vec![usb("")]
        };
        let err = vendor_locked_target(busy, Some("id-1"), scan).unwrap_err();
        assert!(matches!(err.downcast_ref::<PtpUsbError>(), Some(PtpUsbError::CameraLock(LockError::Busy { .. }))), "{err:?}");
        assert!(!scanned.get(), "the bus was scanned before the lock was taken");

        // Granted lock: it is still held while the bus is scanned, and comes back held.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.lock");
        let held_during_scan = Cell::new(None);
        let scan = || {
            held_during_scan.set(Some(matches!(CameraLock::acquire_at(&path), Err(LockError::Busy { .. }))));
            vec![usb("")]
        };
        let (lock, serial) = vendor_locked_target(|| CameraLock::acquire_at(&path), Some("id-1"), scan).unwrap();
        assert_eq!(held_during_scan.get(), Some(true), "the lock was not held while the bus was scanned");
        assert_eq!(serial, None, "the sole serial-less body: the session takes the sole camera");
        assert!(matches!(CameraLock::acquire_at(&path), Err(LockError::Busy { .. })), "the lock is handed on, still held");
        drop(lock);
        assert!(CameraLock::acquire_at(&path).is_ok(), "and released with it");
    }

    #[test]
    fn a_refused_id_n_and_a_plain_serial_keep_their_answers_and_release_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.lock");
        let acquire = || CameraLock::acquire_at(&path);
        // Refused: the typed refusal is unchanged, and the lock does not outlive the refusal.
        let err = vendor_locked_target(acquire, Some("id-1"), || vec![usb("AAA")]).unwrap_err().to_string();
        assert!(err.contains("SDK id") && err.contains("1 attached"), "{err}");
        assert!(CameraLock::acquire_at(&path).is_ok(), "a refusal must not leave the lock held");
        // A real serial: passed through, no scan, lock held for the caller.
        let (lock, serial) = vendor_locked_target(acquire, Some("0000003023668"), || panic!("no scan for a real serial")).unwrap();
        assert_eq!(serial.as_deref(), Some("0000003023668"));
        drop(lock);
    }

    // ── ref set: the name comes from data, so it is checked ──────────────

    fn snapshot_with(dir: &Path, file: &str, model: &str, serial: &str, label: Option<&str>, at: &str) -> PathBuf {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.00".into() },
            Transport::File,
            at.into(),
        );
        s.label = label.map(String::from);
        let p = dir.join(file);
        s.save_to_file(&p).unwrap();
        p
    }

    #[test]
    fn ref_set_refuses_a_serial_or_model_that_would_leave_references() {
        let d = TempDir::new().unwrap();
        let refs = d.path().join("references");
        for (model, serial) in [("Z 9", "../../escape"), ("Z 9", "x/y"), ("Z 9", ".."), ("Z 9", ""), ("Z 9", "a\\b"), ("../Z", "1"), ("", "1")] {
            let src = snapshot_with(d.path(), "s.json", model, serial, None, "2026-01-01T00:00:00Z");
            let err = set_reference(&refs, &src).unwrap_err().to_string();
            assert!(err.contains("cannot be a reference"), "{model:?} {serial:?}: {err}");
        }
        // Nothing was created, in references/ or above it.
        assert!(!refs.exists());
        let created: Vec<String> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(created, ["s.json"]);
        // A serial with a space or a dot in it is a plain name and works.
        let ok = snapshot_with(d.path(), "ok.json", "Z 9", "a b.c", None, "2026-01-01T00:00:00Z");
        assert_eq!(set_reference(&refs, &ok).unwrap().target, refs.join("Z_9_a b.c.json"));
    }

    #[test]
    fn ref_set_copies_the_bytes_it_checked_and_reports_what_it_replaced() {
        let d = TempDir::new().unwrap();
        let refs = d.path().join("references");
        let first = snapshot_with(d.path(), "first.json", "Z 9", "ABC", Some("old"), "2026-01-01T00:00:00Z");
        let done = set_reference(&refs, &first).unwrap();
        assert!(done.replaced.is_none() && !done.unchanged);
        assert_eq!(fs::read(&done.target).unwrap(), fs::read(&first).unwrap(), "a byte copy, not a re-serialisation");
        // Promoting another snapshot of the same camera replaces it, and says what went.
        let second = snapshot_with(d.path(), "second.json", "Z 9", "ABC", Some("new"), "2026-02-02T00:00:00Z");
        let done = set_reference(&refs, &second).unwrap();
        assert_eq!(done.replaced, Some(("2026-01-01T00:00:00Z".to_string(), Some("old".to_string()))));
        assert!(!done.unchanged);
        assert_eq!(fs::read(&done.target).unwrap(), fs::read(&second).unwrap());
        // Promoting the same file again changes nothing, and says so.
        let again = set_reference(&refs, &second).unwrap();
        assert!(again.unchanged);
        // Other cameras' references are untouched, and no temp file lingers.
        let names: Vec<String> = fs::read_dir(&refs).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["Z_9_ABC.json"]);
    }

    #[test]
    fn ref_set_leaves_the_old_reference_when_the_new_snapshot_is_unreadable() {
        let d = TempDir::new().unwrap();
        let refs = d.path().join("references");
        let good = snapshot_with(d.path(), "good.json", "Z 9", "ABC", None, "2026-01-01T00:00:00Z");
        let target = set_reference(&refs, &good).unwrap().target;
        let bad = d.path().join("bad.json");
        fs::write(&bad, "{ not json").unwrap();
        assert!(set_reference(&refs, &bad).is_err());
        let wrong_version = d.path().join("v.json");
        fs::write(&wrong_version, fs::read_to_string(&good).unwrap().replace("\"format_version\": 1", "\"format_version\": 9")).unwrap();
        assert!(set_reference(&refs, &wrong_version).is_err());
        assert_eq!(fs::read(&target).unwrap(), fs::read(&good).unwrap());
    }

    #[test]
    fn ref_show_refuses_an_unsafe_name_instead_of_looking_it_up() {
        let d = TempDir::new().unwrap();
        let err = cmd_ref(d.path(), &RefCmd::Show { model: "Z 9".into(), serial: "../x".into() }).unwrap_err();
        assert!(err.to_string().contains("not a plain file name"), "{err}");
    }

}
