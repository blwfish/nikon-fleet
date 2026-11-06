//! `fleet` CLI — settings management for a fleet of Nikon Z cameras.
//!
//! All real logic lives in the `nikon_fleet` library; this file is just
//! the command-line surface.

mod cli_report;
use cli_report::not_written_error;
// The MCP server's child process; it takes an inherited file descriptor, which Windows has no
// equivalent of here (fleet-mcp is unix-only too).
#[cfg(unix)]
mod worker_cmd;

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use nikon_fleet::camera_lock::CameraLock;
use nikon_fleet::diff::{Diff, DiffOptions, diff_with_schema, is_volatile};
// The exit statuses, `ExitCodeError` and `exit_code_for` live in the library's `exit_status` module, so the GUI and the
// tests consume the same ones (a renumbering cannot reach one side only).
use nikon_fleet::exit_status::{
    EXIT_ALREADY_ARCHIVED, EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN, EXIT_WRITE_INDETERMINATE, ExitCodeError, exit_code_for,
    is_already_archived,
};
use nikon_fleet::firmware::{
    ArchiveListing, FirmwareError, FirmwareMeta, FIRMWARE_META_FORMAT_VERSION,
    archive_dir, archive_exists, format_size, ingest_bundle, load_meta,
    model_slug, save_meta, scan_archives, sha256_file, IngestOutcome, Provenance, RetrievalMethod,
};
use nikon_fleet::firmware_browse::{
    BrowserWindow, VersionEvidence, browse_firmware, nikon_is_release_page,
};
use nikon_fleet::firmware_source::{
    CameraRelation, DownloadAccess, FirmwareSource, FwVersion, ModelStatus,
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
    TargetError, body_firmware, raw_usb_serial_arg, select_by_cli_serial, select_for_snapshot_serial, storable_serial,
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

    /// Also append every diagnostics record of this run to FILE, uncompacted (JSON lines, owner-only).
    /// Diagnostics are always on: a bounded record of what each process was doing is kept in the
    /// diagnostics directory (`NIKON_FLEET_DIAG_DIR` replaces its default location) and read back
    /// with `diag-report`. Payloads such as an FTP profile are never recorded, only their length
    /// and a hash.
    #[arg(long, global = true, value_name = "FILE")]
    trace: Option<PathBuf>,

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

    /// Edit a COPY of a stored snapshot: change a setting, an owner string, an IPTC field, or the FTP profile list
    /// of what will later be restored. A snapshot read from a camera is never edited; `edit copy` makes a copy that
    /// is, and records where it came from and what changed. Reads and writes files only; no camera.
    #[command(subcommand)]
    Edit(EditCmd),

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

    /// Put a snapshot's owner strings, copyright and IPTC-preset switches, IPTC presets and FTP profiles back on a body
    /// (the `extended` section; `restore` does not touch them). Raw USB only: no SDK. Plans, then applies the plan it
    /// computed, or the one named by --expect-digest and --expect-summary. A restore point is written before the first
    /// write; restoring from it undoes the restore. The FTP list is rebuilt by deleting every stored profile and adding
    /// the snapshot's, so use --dry-run first.
    RestoreExtended(RestoreExtendedArgs),

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

    /// Read the camera's FTP profile (0x90E8 → 0x90EB → 0x90ED) over raw USB
    /// PTP and decode it. Writes nothing. The sequence is NX Field's own, as
    /// captured on a Z 9; it works over USB on a Z 9 and a Z6III (2026-11-05).
    /// The password is masked unless --show-password is given.
    VendorReadFtp(VendorReadFtpArgs),

    /// EXPERIMENTAL. Write a profile blob read from a camera (or built to the
    /// same layout) with 0x90EE(slot, param2), byte for byte. It changes the
    /// camera. Only two forms are sent: `--slot N --param2 0` (N below the
    /// stored count) replaces profile N, and `--slot COUNT --param2 1` adds one.
    /// Either way the written profile lands at the FRONT of the list (slot 0)
    /// and the others shift down. Anything else is refused before it is sent.
    /// The camera is read first so the plan can be printed, and the write is
    /// read back in the same session.
    #[command(after_help = VENDOR_WRITE_BLOB_HELP)]
    VendorWriteFtpBlob(VendorWriteFtpBlobArgs),

    /// EXPERIMENTAL, read-only. Send `0x944C(<op>, 0)` and print what the
    /// camera answers: the response code and the data bytes, with no
    /// interpretation. NX Field sends `0x944C(0x90ED, 0)` before its FTP
    /// screen; that the reply says whether the camera implements `<op>` is a
    /// guess, which this command exists to test. It sends nothing else (no
    /// setup, no data-out) and one query per run.
    VendorOpSupport(VendorOpSupportArgs),
    /// EXPERIMENTAL, read-only. Read one device property with the standard PTP `GetDevicePropValue`
    /// (0x1015), or its descriptor with `GetDevicePropDesc` (0x1014) via --desc, and print the
    /// bytes. A value is also decoded as a PTP string when it is one. This is a different path from
    /// `vendor-read` (0x943B), which on a Z 9 answers DevicePropNotSupported for the artist and
    /// image-comment strings. Sends nothing else.
    VendorGetProp(VendorGetPropArgs),
    /// EXPERIMENTAL, read-only. Ask the body for the descriptor (`GetDevicePropDesc`, 0x1014) of every property code in
    /// one or more blocks and save what it says (type, get/set flag, default, current value, allowed values) as a
    /// dump in `<data-dir>/sweeps/`. To find which property a camera menu setting is: sweep, change that one setting on
    /// the camera, sweep again, and compare with `vendor-sweep-diff`. Sends nothing but `GetDevicePropDesc`.
    VendorSweep(VendorSweepArgs),
    /// Compare two `vendor-sweep` dumps: which properties appeared, vanished or changed (current value, allowed values,
    /// access). With `--noise`, a third dump of the same body taken with nothing changed, the properties that move on their
    /// own (a clock, a counter) are set aside. Reads files only; no camera.
    VendorSweepDiff(VendorSweepDiffArgs),
    /// EXPERIMENTAL, read-only. Send `0x9412(<slot>)` (the IPTC profile read, the pair of the write `0x9413`) and print the
    /// bytes it returns as hex, plus a best-effort split into `[u32 length][UTF-8]` entries. Never sent over USB before
    /// 2026-11-06. Changes nothing on the camera.
    VendorReadIptc(VendorReadIptcArgs),
    /// EXPERIMENTAL. Write one IPTC slot with `0x9413(<slot>)`. Each `--entry` is one `[u32 length][UTF-8]` entry of the
    /// data phase, in order (an empty string is an empty entry). The slot is read first and again after, in the same
    /// session, and both are printed. It changes the camera. Confirmed only on a Z6III by replaying a captured blob; any
    /// other model needs `--force-unconfirmed-model`. `--dry-run` prints the data phase and sends nothing.
    VendorWriteIptc(VendorWriteIptcArgs),
    /// EXPERIMENTAL. Write a one-byte property with the standard `SetDevicePropValue` and read it back: 0xD053
    /// EnableCopyright (0 or 1) or 0xD060 IPTC preset (0..=13; 0 is none, and it changes which IPTC profile the camera
    /// embeds in photos). The value before is printed so it can be put back. Model-gated like the other writes.
    VendorSetByteProp(VendorSetBytePropArgs),
    #[command(after_help = VENDOR_SET_STRING_HELP)]
    /// EXPERIMENTAL. Write one string property (0xD072 Artist, 0xD073 Copyright, 0xD090 Image
    /// comment) with the standard PTP `SetDevicePropValue` (0x1016), then read it back and say whether
    /// the camera holds what was sent. In one session it reads the property's descriptor (refusing a
    /// property that is not a read-write string right now) and its current value, which it prints so
    /// the change can be undone. `--dry-run` shows the current value and sends nothing. No other
    /// property can be written with this. Confirmed over USB on a Z 9, a Z6III and a Z 5 for all three properties (limits 36 characters for Artist and Image comment, 54 for Copyright; one more is refused and leaves the value unchanged); see the handoff doc.
    VendorSetString(VendorSetStringArgs),
    /// Show what each recent nikon-fleet process (CLI, GUI, MCP server) was doing, from the
    /// diagnostics files they leave behind: the operation in flight when one hung or was killed, how
    /// long it had been open, whether the process is still running, and what ran just before. Reads
    /// only; starts no diagnostics of its own.
    DiagReport(DiagReportArgs),
    /// EXPERIMENTAL. Send ONE operation from a short fixed list (0x90E9, 0x90EA,
    /// 0x90EC, 0x90EF in the FTP-profile range, and 0x9FFF as a control that
    /// should not exist), optionally with up to two parameters and up to 32
    /// bytes of data-out, and print the response, with the stored profile list
    /// (every profile, and what 0x90EB reports) read before and after and the
    /// differences named. Only 0x9FFF, the control, is read-only. The others are
    /// treated as writes: they need a model confirmed for FTP-profile writes (or
    /// --force-unconfirmed-model), and 0x90EA (DELETES ALL stored profiles, no
    /// confirmation asked by the camera) and 0x90EF N (DELETES profile N) are
    /// sent only when named again with --allow-destructive-op, e.g. `vendor-probe-op
    /// 0x90EA --allow-destructive-op 0x90EA`. 0x90E9 and 0x90EC are unexplained
    /// (0x90EC with data blanked a Z 9's stored device name). A call that gets no
    /// reply can stall the USB pipe or poison the session, in which case the
    /// "after" listing is unavailable, the op may or may not have taken effect,
    /// and the command exits 4 (the code of an indeterminate write): run
    /// `vendor-read-ftp` in a new session to see the list.
    VendorProbeOp(VendorProbeOpArgs),
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
    /// Do not read the owner strings (Artist, Copyright, Image comment), the copyright and IPTC-preset
    /// switches and the stored FTP profiles into the snapshot. They are read by default, from whatever the
    /// body lists, before the SDK session opens.
    #[arg(long)]
    no_extended: bool,
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
enum EditCmd {
    /// Make an editable copy of a stored snapshot (or of a reference) in `snapshots/`. The original is not touched.
    /// Prints the new file's name; the other `edit` commands take it.
    Copy {
        /// The snapshot to copy: a file name in `snapshots/`, or a path.
        snapshot: PathBuf,
        /// The copy's label (default: `edit-` and the original's). Not one starting `mcp-`.
        #[arg(long)]
        label: Option<String>,
    },
    /// Set a setting by name. A choice takes one of the values the snapshot lists for it, a position on a scale takes
    /// a value on that scale, a number stays a whole number if it is one. The name is as `restore --only` takes it.
    Set {
        /// The edited copy, a file name in `snapshots/`.
        file: String,
        property: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Set Artist, Copyright or Image comment (by name or code, e.g. `artist` or `0xD072`).
    SetString {
        file: String,
        item: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Set EnableCopyright (0 or 1) or the IPTC preset select (0 to 13).
    SetSwitch {
        file: String,
        item: String,
        value: u8,
    },
    /// Set one field of an IPTC preset, or its name (`name`). Slots are numbered from 1.
    SetIptc {
        file: String,
        slot: u32,
        field: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Remove an FTP profile from the list. Index 0 is the front of the list, the one the camera uses first.
    FtpDelete { file: String, index: usize },
    /// Move an FTP profile so that it ends at position TO; the others shift. (Restoring a reordered list rebuilds it.)
    FtpMove { file: String, from: usize, to: usize },
    /// Show one FTP profile of an edited copy (the password and the Wi-Fi key are masked unless `--reveal`).
    FtpShow {
        file: String,
        index: usize,
        #[arg(long)]
        reveal: bool,
    },
    /// Change fields of one FTP profile; only the fields given change. The camera is not touched.
    FtpSet {
        file: String,
        index: usize,
        #[command(flatten)]
        fields: FtpFieldArgs,
    },
    /// Add an FTP profile (it needs `--name` and `--host`), starting from the encoder's template; at `--at` (default: the end).
    FtpAdd {
        file: String,
        #[arg(long)]
        at: Option<usize>,
        #[command(flatten)]
        fields: FtpFieldArgs,
    },
    /// Copy an FTP profile to the position after it, with its Wi-Fi name and key, login and the rest; `--name` (default:
    /// the name and " copy") and any other field given are changed in the copy.
    FtpDuplicate {
        file: String,
        index: usize,
        #[command(flatten)]
        fields: FtpFieldArgs,
    },
    /// Show where an edited copy came from and every change made in it.
    Log { file: String },
}

/// The fields of an FTP profile. A password given with `--password` is visible to other programs on this computer
/// while the command runs; `--password-stdin` reads it from standard input instead.
#[derive(Args, Debug, Clone, Default)]
struct FtpFieldArgs {
    #[arg(long, allow_hyphen_values = true)]
    name: Option<String>,
    /// The Wi-Fi name.
    #[arg(long, allow_hyphen_values = true)]
    wifi_name: Option<String>,
    /// The Wi-Fi key (stored in what the encoder calls the 5 GHz SSID field; probably a passphrase).
    #[arg(long, allow_hyphen_values = true)]
    wifi_key: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    host: Option<String>,
    /// The destination folder on the server (`--folder ""` for none).
    #[arg(long, allow_hyphen_values = true)]
    folder: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    user: Option<String>,
    #[arg(long, allow_hyphen_values = true, conflicts_with = "password_stdin")]
    password: Option<String>,
    /// Read the password from standard input (one trailing newline removed).
    #[arg(long)]
    password_stdin: bool,
    /// The server's port (21 for FTP, 22 for SFTP by convention).
    #[arg(long)]
    port: Option<u16>,
    /// `ftp` or `sftp`.
    #[arg(long, value_parser = ["ftp", "sftp"])]
    protocol: Option<String>,
    /// The second port (probably a proxy port).
    #[arg(long)]
    proxy_port: Option<u16>,
}

impl FtpFieldArgs {
    fn to_fields(&self) -> Result<nikon_fleet::ftp_edit::ProfileFields> {
        use nikon_fleet::ftp_edit::{ProfileFields, Protocol};
        let password = if self.password_stdin {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("reading the password from stdin")?;
            Some(strip_single_trailing_newline(buf))
        } else {
            self.password.clone()
        };
        Ok(ProfileFields {
            name: self.name.clone(),
            wifi_name: self.wifi_name.clone(),
            wifi_key: self.wifi_key.clone(),
            host: self.host.clone(),
            folder: self.folder.clone(),
            username: self.user.clone(),
            password,
            port: self.port,
            protocol: self.protocol.as_deref().map(|p| if p == "sftp" { Protocol::Sftp } else { Protocol::Ftp }),
            proxy_port: self.proxy_port,
        })
    }
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
    /// Show the FTP profiles' password and Wi-Fi key in the clear. They are stored in the snapshot and masked
    /// (asterisks) by default; with --json they are left out unless this is given.
    #[arg(long)]
    reveal: bool,
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

const ALREADY_ARCHIVED_HELP: &str = "\
Exit status:
  0  done (identical bytes that are already archived count as done)
  1  any other error
  2  command-line usage error
  3  refused: that model and version is already in the archive, nothing was changed
     (`add` without --force; `fetch`/`browse` when the bytes differ from the archived ones;
     with several models, 3 only when every failure was this)";

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
  0  done: a dry run, or a restore that was applied. The JSON report's `status` says whether it was
     verified (applied_verified) or the read-back was incomplete (applied_unverified)
  1  any other error, or a restore that wrote but did not fully take (the JSON report says which).
     A restore that ends 1 with no JSON report may have written
  2  command-line usage error: nothing ran
  5  the plan no longer matches, so nothing was written: --expect-digest was given and the camera or
     snapshot differs from that plan, or (without --expect-*) the camera changed between this run's
     own plan and its apply
  6  a restore (not a dry run) was refused before it could write anything: nothing was written
A dry run that names a property which cannot be restored (--only) prints its plan and then exits 1.
With --json, a refusal's message leaves out the advice to pass --allow-partial or to use another
command: a program reading it cannot act on that.";

const DEVICE_INFO_HELP: &str = "\
Read-only: sends only the standard GetDeviceInfo (0x1001) inside a PTP session and changes nothing
on the camera. The last line compares the PTP SerialNumber with the USB iSerialNumber:
  serial: PTP \"<x>\" | USB \"<y>\" | match|differ|ptp_empty|usb_empty|both_empty
  usb_empty means the descriptor has no serial but DeviceInfo does (the body could be identified by it).
With --json the same comparison is the serial_comparison object.
Exit status: 0 read and printed; 1 any error (camera busy, no such camera, malformed dataset); 2 usage error.";

const VENDOR_WRITE_HELP: &str = "\
Exit status:
  0  the camera accepted the profile and a read-back of slot 0 showed exactly what was sent
  1  any other error, including a write refused before anything was sent (a camera holding no profile
     refuses the replace of slot 0; use `vendor-write-ftp-blob --slot 0 --param2 1` to add one); nothing
     was sent to the camera unless the message says so
  2  command-line usage error
  4  the camera MAY have changed: the profile data was sent but the write then failed, or the camera
     answered OK but the read-back did not match, could not be decoded or could not be read.
     Read the list back before retrying (`vendor-read-ftp --slot 0`, or the camera's menu): a write that
     took effect put its profile at the front, slot 0
  --port is the u16 written AFTER the password (8080 in every profile read from a camera, probably a
  proxy port), not the FTP port: the encoder fixes the FTP port at 21.";

const VENDOR_SET_STRING_HELP: &str = "\
Exit status:
  0  the camera accepted the value and the read-back matched (a dry run: the plan was printed)
  1  any other error, including a write refused before anything was sent
  2  command-line usage error
  4  the value was sent but the camera MAY now hold something other than what it had: its reply was lost, or
     it accepted the write and the read-back did not confirm the value. Read it with `vendor-get-prop`
     before retrying";

const VENDOR_WRITE_BLOB_HELP: &str = "\
Exit status:
  0  the camera accepted the profile and a read-back of slot 0 showed exactly what was sent, with the
     stored count the write should leave (a dry run: the plan was printed)
  1  any other error, including a write refused before anything was sent (a second parameter other
     than 0 or 1, a replace of a slot that is not stored, an add at any slot but the count, a model
     outside the confirmed list); nothing was sent to the camera unless the message says so
  2  command-line usage error
  4  the camera MAY have changed: the profile data was sent but the write then failed, or the camera
     answered OK but the read-back did not match, could not be decoded or could not be read.
     Read the list (`vendor-read-ftp --slot 0`, or the camera's menu) BEFORE retrying: a repeated add
     stores a duplicate, and a repeated replace of a slot above 0 hits whichever profile is there now
Write rules (Z 9, 2026-11-05; the read-back is not yet verified on a Z6III):
  --slot N --param2 0   replace the stored profile N (N below the count); it moves to the FRONT (slot 0)
                        and the profiles that were in front of it shift down; the count stays
  --slot COUNT --param2 1  add a profile at the FRONT; the count rises by one
  --slot COUNT --param2 0  does NOT add: the camera stalls the USB pipe and stores nothing; refused here";

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
struct VendorOpSupportArgs {
    /// Operation code to ask about, e.g. "0x90ED". Hex needs the 0x prefix; a
    /// bare number is decimal.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    op: u16,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

/// Hex digits (optional spaces) to bytes, for the probe's `--data`.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() || t.len() % 2 != 0 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{s:?} is not an even number of hex digits"));
    }
    Ok((0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).expect("hex digits checked")).collect())
}

#[derive(Args, Debug)]
struct VendorProbeOpArgs {
    /// Operation code to send, e.g. "0x90E9". Must be one of 0x90E9, 0x90EA,
    /// 0x90EC, 0x90EF, 0x9FFF; anything else is refused before any I/O.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    op: u16,
    /// A parameter to send with the op (up to two; repeat the flag). Decimal
    /// or 0x-prefixed hex. 0xFFFFFFFF is refused: it hung a camera's network
    /// stack when used as a slot number.
    #[arg(long = "param", value_parser = nikon_fleet::ptp_usb::parse_u32_number)]
    params: Vec<u32>,
    /// Data-out to send with the op, as hex digits (at most 32 bytes), e.g.
    /// "01000000". Without it the op is sent with no data phase at all.
    #[arg(long)]
    data: Option<String>,
    /// Send the 0x90E8 setup (data-out 01 00 00) immediately before the op,
    /// as the profile ops need. Without it the op is sent bare in a fresh
    /// session.
    #[arg(long)]
    setup: bool,
    /// Required to send 0x90EA (deletes ALL stored FTP profiles) or 0x90EF
    /// (deletes the profile it is given): name the op again here, e.g.
    /// `--allow-destructive-op 0x90EA`. Naming one op does not allow the other,
    /// and the value must be the op being sent.
    #[arg(long, value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    allow_destructive_op: Option<u16>,
    /// Send an op other than the control to a model not confirmed for FTP-profile
    /// writes. The same gate as vendor-write-ftp-blob; it never replaces
    /// --allow-destructive-op.
    #[arg(long)]
    force_unconfirmed_model: bool,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorGetPropArgs {
    /// Property code, e.g. "0xD072" (Artist), "0xD073" (Copyright), "0xD090" (Image comment). Hex
    /// needs the 0x prefix; a bare number is decimal.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    prop: u16,
    /// Read the property's descriptor (0x1014: type, get/set flag, default, current value, form)
    /// instead of its value.
    #[arg(long)]
    desc: bool,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorSweepArgs {
    /// A block of property codes, `LO-HI` inclusive or one code, each in the usual format (`0x` hex, else decimal).
    /// Repeat for several; overlapping or touching blocks are merged. Default: 0x5000-0x50FF and 0xD000-0xD4FF.
    #[arg(long = "range", value_name = "LO-HI", value_parser = nikon_fleet::prop_sweep::CodeRange::parse)]
    ranges: Vec<nikon_fleet::prop_sweep::CodeRange>,
    /// What state the body is in for this sweep ("af-on-only"); it becomes part of the file name and is shown by the diff.
    #[arg(long)]
    label: Option<String>,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorSweepDiffArgs {
    /// The sweep before the change: a path, or a file name in `<data-dir>/sweeps/`.
    a: PathBuf,
    /// The sweep after it.
    b: PathBuf,
    /// A sweep of the same body in the same state as A (nothing changed between them). What differs between A and
    /// this is noise: it is set aside from the differences between A and B.
    #[arg(long, value_name = "SWEEP")]
    noise: Option<PathBuf>,
    /// Keep the noise in the differences instead of setting it aside (the count of what moved is still shown by
    /// listing it).
    #[arg(long, requires = "noise")]
    keep_noise: bool,
    /// Print the comparison as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct VendorReadIptcArgs {
    /// The IPTC slot (NX Field read slot 1; it also sent 0 when its IPTC screen opened).
    #[arg(long, default_value_t = 1, value_parser = nikon_fleet::ptp_usb::parse_u32_number)]
    slot: u32,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorWriteIptcArgs {
    #[arg(long, default_value_t = 1, value_parser = nikon_fleet::ptp_usb::parse_u32_number)]
    slot: u32,
    /// One entry of the data phase; repeat the flag, in order. Use `--entry ''` for an empty one.
    #[arg(long = "entry", allow_hyphen_values = true)]
    entries: Vec<String>,
    /// Send these bytes as the data phase instead of encoding `--entry` values: hex digits, spaces allowed.
    #[arg(long, conflicts_with = "entries")]
    data_hex: Option<String>,
    /// Print the data phase and send nothing (nothing is read from the camera either).
    #[arg(long)]
    dry_run: bool,
    /// Write to a model an IPTC write has not been confirmed on.
    #[arg(long)]
    force_unconfirmed_model: bool,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct VendorSetBytePropArgs {
    /// 0xD053 or 0xD060.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    prop: u16,
    /// The new value.
    value: u8,
    #[arg(long)]
    force_unconfirmed_model: bool,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Args, Debug)]
struct DiagReportArgs {
    /// How many of each process's most recent entries to show. Runs of routine successful
    /// operations (one SDK read of two hundred) are collapsed into one line first.
    #[arg(long, default_value_t = 20)]
    last: usize,
    /// Read this diagnostics directory instead of the default one.
    #[arg(long)]
    dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct VendorSetStringArgs {
    /// Property code: 0xD072 (Artist), 0xD073 (Copyright) or 0xD090 (Image comment). Hex needs the
    /// 0x prefix; a bare number is decimal. Any other code is refused before anything is sent.
    #[arg(value_parser = nikon_fleet::ptp_usb::parse_propcode)]
    prop: u16,
    /// The new text. Use "" to clear it. Latin-1 only (U+0000..U+00FF): on a Z 9 the camera keeps
    /// just the low byte of each character, so anything beyond is refused rather than corrupted.
    /// Control characters (newline, tab, NUL) are refused. The camera itself refuses a value over its
    /// limit (a Z 9 took 36 characters for the image comment and refused 37).
    text: String,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
    /// Write to a model outside the confirmed list (Z 9, Z6III, Z 5); it is refused without this.
    #[arg(long)]
    force_unconfirmed_model: bool,
    /// Read and print the current value and what would change, then stop. Sends no write. Runs the
    /// same checks as the write (property, model, text, whether the property is writable now) and
    /// fails where it would.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct VendorReadFtpArgs {
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
    /// Read this stored profile instead of the active one: slots 0 to count-1
    /// (`count` is on the first output line). Read-only: `0x90ED(N)`. A slot
    /// past the end is refused before sending, because on a Z 9 it stalled the
    /// USB pipe.
    #[arg(long)]
    slot: Option<u32>,
    /// Print the FTP password and the "5 GHz" field (probably a Wi-Fi
    /// password) in clear text instead of asterisks. Without
    /// --show-password the hex dump from --raw is masked too. The other fields
    /// (profile name, SSIDs, host, username) are always printed as they are, so
    /// one that happens to hold the same text as the password shows it.
    #[arg(long)]
    show_password: bool,
    /// Also print the profile blob as hex. Unless --show-password, every byte
    /// that spells the password or the "5 GHz" field is zeroed, wherever it
    /// appears in the blob (also inside another field, or as UTF-16), so the
    /// dump is not a way around the mask. The length is unchanged.
    #[arg(long)]
    raw: bool,
    /// Save the profile's true raw bytes (password included, unmasked) to this
    /// file, for `vendor-write-ftp-blob --file`. Created with mode 0600 on Unix; refuses
    /// to overwrite an existing file.
    #[arg(long)]
    save_blob: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct VendorWriteFtpBlobArgs {
    /// File holding the profile blob (raw bytes) to write.
    #[arg(long)]
    file: PathBuf,
    /// First 0x90EE parameter (slot). With --param2 0 it must be below the stored profile count
    /// (that profile is replaced); with --param2 1 it must equal the count (a profile is added).
    /// Either way the written profile ends up at the front, slot 0.
    #[arg(long, default_value_t = 0)]
    slot: u32,
    /// Second 0x90EE parameter. 0 replaces the profile in --slot (the default); 1 adds a new
    /// profile and needs --slot equal to the stored count. No other value has worked on a camera,
    /// so any other is refused. Adding is therefore `--slot COUNT --param2 1`; `--slot COUNT`
    /// alone is refused (on a Z 9 that form stalled the USB pipe and added nothing).
    #[arg(long, default_value_t = 0, value_parser = nikon_fleet::ptp_usb::parse_u32_number)]
    param2: u32,
    /// Camera serial to target; same rules as vendor-read's --serial.
    #[arg(long)]
    serial: Option<String>,
    /// Write even if the model is not in ptp_usb's confirmed list.
    #[arg(long)]
    force_unconfirmed_model: bool,
    /// Read the camera and print the plan, then stop. Applies the same checks as the write (model
    /// list, the two allowed forms, the stored count) and fails where it would; sends no write.
    #[arg(long)]
    dry_run: bool,
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
    /// The u16 written after the password: 8080 in every profile read from a camera (probably a
    /// proxy port; the decoder calls it "second port"). It is NOT the FTP port: the encoder fixes
    /// that at 21, and `--port 2121` does not change it.
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
    /// Target camera serial. Defaults to the serial recorded in the snapshot. A snapshot recorded
    /// with an SDK id instead of a USB serial (`id-N`, or a bare number) names whichever body holds
    /// that id now, so with more than one body of the snapshot's model attached the restore
    /// refuses; attach only one body of that model, or pass --serial with the real USB serial.
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
struct RestoreExtendedArgs {
    /// Snapshot to restore from. Resolved relative to snapshots/ if not absolute.
    snapshot_path: PathBuf,
    /// Target camera serial. Defaults to the serial recorded in the snapshot.
    #[arg(long)]
    serial: Option<String>,
    /// Print what would be written and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Restore only this category: strings, iptc, switches or ftp. Repeatable.
    #[arg(long = "only", value_name = "CATEGORY")]
    only: Vec<String>,
    /// Apply only the plan that has this digest (from a `--dry-run` of the same snapshot, target and `--only`). The plan is
    /// recomputed on the body as it is now; if it differs, nothing is written and the command exits 5. Needs --expect-summary.
    #[arg(long, value_name = "DIGEST", requires = "expect_summary", conflicts_with = "dry_run")]
    expect_digest: Option<String>,
    /// The plan's summary line, copied from the same dry run.
    #[arg(long, value_name = "SUMMARY", requires = "expect_digest", conflicts_with = "dry_run")]
    expect_summary: Option<String>,
    /// Print the plan or the result as JSON on stdout.
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

/// Where `vendor-sweep` keeps its dumps. Not `snapshots/`: a listing of snapshots must not meet a file that is not one.
fn sweeps_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sweeps")
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
    let opts = DiffOptions { include_volatile: args.include_volatile, ..Default::default() };
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
    if !d.extended.is_empty() {
        println!("\nOwner strings and FTP profiles ({}):", d.extended.len());
        for c in &d.extended {
            println!("  {}", c.item);
            if let Some(a) = &c.a {
                println!("    A: {a}");
            }
            if let Some(b) = &c.b {
                println!("    B: {b}");
            }
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
            EntryState::Ok { model, serial, firmware, label, captured_at, edited, .. } => println!(
                "{name}\n  {model} {serial}  label={label}  fw={firmware}  {captured_at}{marker}",
                name = entry.file,
                label = label.as_deref().unwrap_or("-"),
                marker = if *edited { "  [edited copy]" } else { "" },
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
    let mut opts = if args.raw { render::RenderOptions::raw() } else { render::RenderOptions::viewer(&labels) };
    opts.reveal_secrets = args.reveal;
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
    if snap.is_edited() {
        bail!(
            "{} is an edited copy, not a reading from a camera, and a reference is what `check` compares a body against: \
             choose a snapshot that was captured",
            snapshot_path.display()
        );
    }
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

/// `edit …`: see `EditCmd`. An edit that changes nothing says so and writes nothing.
fn cmd_edit(data_dir: &Path, sub: &EditCmd) -> Result<()> {
    use nikon_fleet::ops::edit_snapshot::{create_edit_copy, edit_in_place};
    use nikon_fleet::snapshot_edit as edit;
    let snaps = snapshots_dir(data_dir);
    let now = OffsetDateTime::now_utc().format(&Rfc3339).context("formatting the time")?;
    let say = |a: &edit::Applied| {
        if a.changed {
            println!("{}", a.change);
        } else {
            println!("no change: {}", a.change);
        }
    };
    let refuse = |e: nikon_fleet::ops::edit_snapshot::EditFileError| anyhow::anyhow!("{e} [{}]", e.tag());
    match sub {
        EditCmd::Copy { snapshot, label } => {
            let source = resolve_snapshot_path(data_dir, snapshot);
            let c = create_edit_copy(&snaps, &source, label.as_deref(), &now).map_err(refuse)?;
            println!("{}", c.file);
        }
        EditCmd::Set { file, property, value } => {
            let (name, a) = edit_in_place(&snaps, file, |s| edit::set_property_text(s, property, value, &now)).map_err(refuse)?;
            println!("{name}");
            say(&a);
        }
        EditCmd::SetString { file, item, text } => {
            let code = edit::resolve_extended_item(item).map_err(|e| anyhow::anyhow!("{e} [{}]", e.tag()))?;
            say(&edit_in_place(&snaps, file, |s| edit::set_owner_string(s, code, text, &now)).map_err(refuse)?);
        }
        EditCmd::SetSwitch { file, item, value } => {
            let code = edit::resolve_extended_item(item).map_err(|e| anyhow::anyhow!("{e} [{}]", e.tag()))?;
            say(&edit_in_place(&snaps, file, |s| edit::set_switch(s, code, *value, &now)).map_err(refuse)?);
        }
        EditCmd::SetIptc { file, slot, field, text } => {
            say(&edit_in_place(&snaps, file, |s| edit::set_iptc_field(s, *slot, field, text, &now)).map_err(refuse)?);
        }
        EditCmd::FtpDelete { file, index } => say(&edit_in_place(&snaps, file, |s| edit::ftp_delete(s, *index, &now)).map_err(refuse)?),
        EditCmd::FtpMove { file, from, to } => say(&edit_in_place(&snaps, file, |s| edit::ftp_move(s, *from, *to, &now)).map_err(refuse)?),
        EditCmd::FtpShow { file, index, reveal } => {
            let path = nikon_fleet::ops::paths::resolve_existing(&snaps, file).map_err(|e| anyhow::anyhow!("{e} [{}]", e.tag()))?;
            let mut snap = Snapshot::load_from_file(&path).with_context(|| format!("loading {}", path.display()))?;
            let v = nikon_fleet::ftp_edit::profile_view(&mut snap, *index).map_err(|e| anyhow::anyhow!("{e} [{}]", e.tag()))?;
            let mask = |s: &str| if *reveal { s.to_string() } else { "*".repeat(s.chars().count()) };
            println!("profile {index}: {:?}", v.name);
            println!("  Wi-Fi name {:?}   Wi-Fi key {}", v.wifi_name, mask(&v.wifi_key));
            println!("  server {:?} port {} {}   folder {:?}", v.host, v.port, if v.protocol == nikon_fleet::ftp_edit::Protocol::Sftp { "SFTP" } else { "FTP" }, v.folder);
            println!("  login {:?}   password {}   second port {}", v.username, mask(&v.password), v.proxy_port);
        }
        EditCmd::FtpSet { file, index, fields } => {
            let f = fields.to_fields()?;
            say(&edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_set_fields(s, *index, &f, &now)).map_err(refuse)?);
        }
        EditCmd::FtpAdd { file, at, fields } => {
            let f = fields.to_fields()?;
            say(&edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_add(s, &f, *at, &now)).map_err(refuse)?);
        }
        EditCmd::FtpDuplicate { file, index, fields } => {
            let f = fields.to_fields()?;
            say(&edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_duplicate(s, *index, None, &f, &now)).map_err(refuse)?);
        }
        EditCmd::Log { file } => {
            let path = nikon_fleet::ops::paths::resolve_existing(&snaps, file).map_err(|e| anyhow::anyhow!("{e} [{}]", e.tag()))?;
            let snap = Snapshot::load_from_file(&path).with_context(|| format!("loading {}", path.display()))?;
            match &snap.derived_from {
                None => println!("{file} was read from a camera: it has not been edited and cannot be (make a copy with `edit copy`)"),
                Some(d) => {
                    println!("{file}: a copy of {} (captured {}{}), made {}, last edited {}", d.source_file, d.source_captured_at, d.source_label.as_ref().map(|l| format!(", label {l:?}")).unwrap_or_default(), d.created_at, d.last_edited_at);
                    if d.edits.is_empty() {
                        println!("  no changes yet");
                    }
                    for e in &d.edits {
                        println!("  {}  {}  {}", e.at, e.item, e.change);
                    }
                }
            }
        }
    }
    Ok(())
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

/// The refusal for an SDK bundle that is not there, shared by `open_sdk` and the commands that do work before it.
fn require_sdk_bundle(bundle_path: &Path) -> Result<()> {
    if !bundle_path.exists() {
        bail!(
            "SDK bundle not found at {}.\n\
             Run scripts/setup-sdk-runtime.sh first, or pass --sdk-bundle <path>.",
            bundle_path.display()
        );
    }
    Ok(())
}

/// Take the camera lock, then load and initialise the SDK. The lock comes
/// first because `InitializeSDK` (and the USB reset before it) is exactly the
/// step that must not overlap another session (F2, F3 in
/// docs/mcp-hardware-findings.md). Contention fails at once with
/// "camera busy"; there is no wait or retry.
fn open_sdk(bundle_path: &Path, no_usb_reset: bool) -> Result<SdkSession> {
    require_sdk_bundle(bundle_path)?;
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
    // The owner strings and FTP profiles come over raw PTP, which cannot share the interface with an open SDK
    // session: read them first and let that session go before the SDK takes the camera. Failures are recorded in
    // the snapshot, not raised. `--serial id-N` names a body by its SDK id, which the raw side cannot resolve; that
    // case records the reason instead.
    require_sdk_bundle(bundle)?;
    let extended = if args.no_extended {
        None
    } else {
        say(args.json, "Reading owner strings and FTP profiles…\n");
        Some(nikon_fleet::extended::read_extended_for_serial(args.serial.as_deref()))
    };
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
    let extended_summary = nikon_fleet::ops::capture_snapshot::attach_extended(&mut snap, extended);
    if let Some(summary) = &extended_summary {
        say(args.json, &extended_line(summary));
    }

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
            extended_summary,
        );
        json_out.emit(&serde_json::to_string_pretty(&outcome)?)?;
    }
    // Read-only and finished: leave without closing the session (F24).
    nikon_fleet::process_exit::exit_now(0)
}

/// The one status line for the extended section of a capture.
fn extended_line(s: &nikon_fleet::extended::ExtendedSummary) -> String {
    let ftp = match (s.ftp.as_str(), s.ftp_profiles) {
        ("read", Some(n)) => format!("{n} FTP profile(s)"),
        ("not_on_model", _) => "no FTP profiles on this model".to_string(),
        _ => "FTP profiles not read".to_string(),
    };
    let iptc = match (s.iptc.as_str(), s.iptc_presets_in_use) {
        ("read", Some(n)) => format!("{n} IPTC preset(s) in use"),
        ("not_on_model", _) => "no IPTC on this model".to_string(),
        _ => "IPTC not read".to_string(),
    };
    let dropped = s.dropped.as_deref().map(|d| format!("; NOT kept: {d}")).unwrap_or_default();
    format!(
        "  extended: {} propert(ies) read, {} not on this model, {} failed; {ftp}; {iptc}{dropped}\n",
        s.props_read, s.props_not_supported, s.props_failed
    )
}

/// Read one vendor-only property via raw USB PTP (`0x943B`), bypassing the
/// MAID SDK entirely — no --sdk-bundle needed. See `nikon_fleet::ptp_usb`.
fn cmd_vendor_read(args: &VendorReadArgs) -> Result<()> {
    let read = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::read_vendor_property_report_locked(lock, serial, args.propcode)
    })
        .with_context(|| format!("reading vendor property 0x{:04x} over USB PTP", args.propcode))?;
    let (stdout, warnings) = vendor_read_output(args.propcode, &read);
    for w in warnings {
        eprintln!("{w}");
    }
    println!("{stdout}");
    Ok(())
}

/// Write a profile blob byte for byte via `0x90EE(slot, param2)`. EXPERIMENTAL.
///
/// The library does the guarding and the read-back in one session each: `plan_ftp_write_locked`
/// (the model gate, the blob must decode, the two allowed forms, the stored count: read-only; a dry
/// run stops after it, so it fails exactly where the real write would) and then
/// `write_ftp_profile_blob_verified_locked`, which repeats those checks with the count read in the
/// write's own session and reads slot 0 back.
fn cmd_vendor_write_ftp_blob(args: &VendorWriteFtpBlobArgs) -> Result<()> {
    use nikon_fleet::ptp_usb::{plan_ftp_write_locked, write_ftp_profile_blob_verified_locked};
    let blob = std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let plan = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        plan_ftp_write_locked(lock, serial, args.slot, args.param2, &blob, args.force_unconfirmed_model)
    })
        .map_err(vendor_write_anyhow_error)?;
    println!("{}", ftp_blob_plan_text(&plan, args.slot, args.param2, blob.len()));
    if args.dry_run {
        return Ok(());
    }
    let outcome = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        write_ftp_profile_blob_verified_locked(
            lock,
            serial,
            args.slot,
            args.param2,
            &blob,
            args.force_unconfirmed_model,
        )
    })
        .map_err(vendor_write_anyhow_error)?;
    println!("{}", ftp_blob_outcome_text(&outcome));
    if let Some(note) = ftp_receipt_note(&outcome.receipt) {
        eprintln!("{note}");
    }
    ftp_verification_result(&outcome.verification)
}

/// `Ok` when the read-back confirmed an FTP-profile write, else the exit-4 error. The camera answered OK, so
/// this is not a refusal, but nothing shows the profile is what was sent: the same status as a write that failed
/// after the data went out (the camera MAY have changed).
fn ftp_verification_result(v: &nikon_fleet::ptp_usb::FtpWriteVerification) -> Result<()> {
    match ftp_blob_unconfirmed(v) {
        None => Ok(()),
        Some(why) => Err(ExitCodeError {
            code: EXIT_WRITE_INDETERMINATE,
            message: format!(
                "the camera answered OK to the write but the read-back did not confirm it ({why}); the camera MAY have \
                 changed. Read the list with `vendor-read-ftp --slot 0` before deciding whether to repeat the write"
            ),
        }
        .into()),
    }
}

/// `vendor_write_error` for an error that may or may not be a [`nikon_fleet::ptp_usb::PtpUsbError`].
fn vendor_write_anyhow_error(e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<nikon_fleet::ptp_usb::PtpUsbError>() {
        Ok(e) => vendor_write_error(e),
        Err(other) => other,
    }
}

/// The plan `vendor-write-ftp-blob` prints before it writes (and all that `--dry-run` prints). What
/// the camera does with each form is the Z 9 rule of `docs/nx-field-ftp-profile-session-2026-07-10.md`:
/// the written profile goes to the FRONT, the others shift down.
fn ftp_blob_plan_text(plan: &nikon_fleet::ptp_usb::FtpWritePlan, slot: u32, param2: u32, blob_len: usize) -> String {
    use nikon_fleet::ptp_usb::FtpWriteKind;
    let d = &plan.decoded;
    let count = plan.count;
    let what = match plan.kind {
        FtpWriteKind::Replace if slot == 0 => format!("REPLACE the profile in slot 0 (it stays at the front; the count stays {count})"),
        FtpWriteKind::Replace => format!(
            "REPLACE the profile in slot {slot} and move it to the FRONT (slot 0): the {slot} profile(s) now in front of it shift down by one, and the count stays {count}"
        ),
        FtpWriteKind::Add => format!(
            "ADD a new profile at the FRONT (slot 0): the {count} stored profile(s) shift down by one and the count becomes {}",
            count.saturating_add(1)
        ),
    };
    format!(
        "{}: {count} stored profile(s). Would {what}: 0x90EE({slot}, {param2}) with {blob_len} byte(s), profile {:?} ({}:{}, folder {:?}{}).",
        plan.model,
        d.profile_name,
        d.host,
        d.ftp_port,
        d.folder,
        if d.is_sftp() { ", SFTP" } else { "" }
    )
}

/// What `vendor-write-ftp-blob` prints after a write the camera answered OK to: what the read-back of
/// slot 0 showed. Field NAMES that differ are listed, never their values (they include the password).
fn ftp_blob_outcome_text(o: &nikon_fleet::ptp_usb::FtpWriteOutcome) -> String {
    use nikon_fleet::ptp_usb::FtpWriteVerification as V;
    let kind = match o.kind {
        nikon_fleet::ptp_usb::FtpWriteKind::Replace => "a replace",
        nikon_fleet::ptp_usb::FtpWriteKind::Add => "an add",
    };
    let head = format!("The camera accepted the write ({kind}; {} stored profile(s) before it).", o.count_before);
    match &o.verification {
        V::Verified { count_after } => format!(
            "{head}\nRead back: slot 0 holds exactly the profile that was sent, and the camera stores {count_after} profile(s). \
             A written profile goes to the front of the list; read any other slot with `vendor-read-ftp --slot N`."
        ),
        V::Mismatch { count_after, count_expected, differing_fields } => format!(
            "{head}\nREAD-BACK MISMATCH: the camera now stores {count_after} profile(s) (expected {count_expected}); fields of slot 0 that differ \
             from what was sent: {}. Either the camera did not take the profile as sent, or this body does not put a written profile at slot 0 \
             (only a Z 9 is known to). Read the list with `vendor-read-ftp --slot N` before repeating the write.",
            if differing_fields.is_empty() { "none (only the count is off)".to_string() } else { differing_fields.join(", ") }
        ),
        V::ReadBackUndecodable { error } => format!(
            "{head}\nREAD-BACK UNDECODABLE: slot 0 was read but is not in a layout this decoder knows ({error}). Read the list with `vendor-read-ftp --slot N` before repeating the write."
        ),
        V::ReadBackFailed { error } => format!(
            "{head}\nREAD-BACK FAILED: slot 0 could not be read back ({error}). The write is neither confirmed nor contradicted. Read the list with `vendor-read-ftp --slot 0` from a new session before repeating the write."
        ),
    }
}

/// `None` when the read-back confirmed the write; otherwise a short reason, for the exit-4 message.
fn ftp_blob_unconfirmed(v: &nikon_fleet::ptp_usb::FtpWriteVerification) -> Option<&'static str> {
    use nikon_fleet::ptp_usb::FtpWriteVerification as V;
    match v {
        V::Verified { .. } => None,
        V::Mismatch { .. } => Some("slot 0 or the count differs from what the write should leave"),
        V::ReadBackUndecodable { .. } => Some("slot 0 did not decode"),
        V::ReadBackFailed { .. } => Some("slot 0 could not be read"),
    }
}

/// Ask the camera `0x944C(op, 0)` over raw USB PTP and print the answer verbatim. Read-only in
/// intent; see `nikon_fleet::ptp_usb::PtpUsbSession::query_vendor_op` for what is unknown.
fn cmd_vendor_op_support(args: &VendorOpSupportArgs) -> Result<()> {
    let q = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::query_vendor_op_locked(lock, serial, args.op)
    })
        .with_context(|| format!("asking 0x944C about operation 0x{:04x} over USB PTP", args.op))?;
    for w in &q.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", vendor_op_query_text(&q));
    Ok(())
}

/// Send one op from the fixed probe list and print the list before and after.
fn cmd_vendor_probe_op(args: &VendorProbeOpArgs) -> Result<()> {
    probe_op_with(args, CameraLock::acquire, usb_camera_list, |lock, serial, data, consent| {
        nikon_fleet::ptp_usb::probe_ftp_op_locked(lock, serial, args.op, args.setup, &args.params, data, consent)
    })
}

/// [`cmd_vendor_probe_op`] with the lock, the USB listing and the probe injected, so what happens before
/// the camera is touched is testable. Everything that needs no camera is refused first
/// (`check_ftp_probe_request`, the function the library applies again): the destructive ops are not sent
/// without `--allow-destructive-op` naming them, and any op but the control passes the FTP-write model
/// gate inside the library. The lock is not taken for a request that is refused.
fn probe_op_with(
    args: &VendorProbeOpArgs,
    acquire_lock: impl FnOnce() -> Result<CameraLock, nikon_fleet::camera_lock::LockError>,
    attached: impl FnOnce() -> Vec<UsbCameraInfo>,
    probe: impl FnOnce(
        CameraLock,
        Option<&str>,
        Option<&[u8]>,
        nikon_fleet::ptp_usb::FtpProbeConsent,
    ) -> Result<nikon_fleet::ptp_usb::FtpOpProbe, nikon_fleet::ptp_usb::PtpUsbError>,
) -> Result<()> {
    let data = args.data.as_deref().map(parse_hex_bytes).transpose().map_err(|e| anyhow::anyhow!("--data: {e}"))?;
    let consent = nikon_fleet::ptp_usb::FtpProbeConsent {
        force_unconfirmed_model: args.force_unconfirmed_model,
        destructive_op: args.allow_destructive_op,
    };
    nikon_fleet::ptp_usb::check_ftp_probe_request(args.op, &args.params, data.as_deref(), consent)
        .with_context(|| format!("probing operation 0x{:04x}", args.op))?;
    let result = vendor_locked_target(acquire_lock, args.serial.as_deref(), attached)
        .and_then(|(lock, serial)| probe(lock, serial.as_deref(), data.as_deref(), consent).map_err(anyhow::Error::new))
        .with_context(|| format!("probing operation 0x{:04x} over USB PTP", args.op))?;
    report_probe(&result)
}

/// Print a finished probe. A probe whose reply was lost after the op went out ends the command with
/// [`probe_indeterminate_error`], not success.
fn report_probe(probe: &nikon_fleet::ptp_usb::FtpOpProbe) -> Result<()> {
    for w in &probe.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", ftp_probe_text(probe));
    match probe_indeterminate_error(probe) {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// The failure a probe whose reply was lost after the op went out must end the command with: exit status
/// `EXIT_WRITE_INDETERMINATE`, never a success (the camera may have acted). `None` for a probe that got an
/// answer or never sent the op.
fn probe_indeterminate_error(probe: &nikon_fleet::ptp_usb::FtpOpProbe) -> Option<ExitCodeError> {
    let nikon_fleet::ptp_usb::FtpProbeOutcome::Indeterminate { data_sent, cause } = &probe.outcome else {
        return None;
    };
    Some(ExitCodeError {
        code: EXIT_WRITE_INDETERMINATE,
        message: format!(
            "operation 0x{:04x} was sent{} and no usable reply came ({cause}): the camera may have acted on it. \
             Run vendor-read-ftp from a new session to see what it holds before repeating anything",
            probe.op,
            if *data_sent { " with its data-out" } else { "" }
        ),
    })
}

/// What `vendor-probe-op` prints: the response verbatim, the profile list before and after, and every
/// difference between them by name (never by value: the profiles hold a password).
fn ftp_probe_text(p: &nikon_fleet::ptp_usb::FtpOpProbe) -> String {
    use nikon_fleet::ptp_usb::{hex_bytes, FtpListChange, FtpListState, FtpProbeOutcome, PtpResponseCode};
    fn list(s: &FtpListState) -> String {
        let items: Vec<String> = s
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let name = match &e.decoded {
                    Some(d) => d.profile_name.clone(),
                    None => format!("(undecodable, {} bytes)", e.raw.len()),
                };
                if i as u32 == s.info.slot { format!("[{i}] {name} (selected)") } else { format!("[{i}] {name}") }
            })
            .collect();
        let mut out = format!(
            "{} profile(s), limit {}, selected index {}, device name {:?}: {}",
            s.info.count,
            s.info.limit,
            s.info.slot,
            s.info.device_name.text,
            items.join(", ")
        );
        if s.truncated {
            out.push_str(&format!(" (only the first {} were read)", s.entries.len()));
        }
        if s.undecodable_count() > 0 {
            out.push_str(&format!(" [{} undecodable]", s.undecodable_count()));
        }
        out
    }
    let mut args = if p.params.is_empty() {
        String::new()
    } else {
        format!("({})", p.params.iter().map(|v| format!("0x{v:x}")).collect::<Vec<_>>().join(", "))
    };
    if let Some(d) = &p.data {
        args.push_str(&format!(" + data {}", hex_bytes(d)));
    }
    let mut out = format!(
        "0x{:04x}{}{}: ",
        p.op,
        args,
        if p.setup_first { " after the 0x90E8 setup" } else if p.params.is_empty() && p.data.is_none() { " bare" } else { "" }
    );
    match &p.outcome {
        FtpProbeOutcome::Replied(r) => {
            out.push_str(&format!("response {}", PtpResponseCode(r.response_code)));
            if !r.response_params.is_empty() {
                out.push_str(&format!(", response parameters {:?}", r.response_params));
            }
            if r.data_containers == 0 {
                out.push_str(", no data phase");
            } else {
                out.push_str(&format!(", {} byte(s) in {} data container(s): {}", r.data.len(), r.data_containers, hex_bytes(&r.data)));
            }
            for d in &r.deviations {
                out.push_str(&format!("\n{}", reply_deviation_note(d)));
            }
            if r.unrecognized_containers > 0 {
                out.push_str(&format!("\nnote: {} container(s) of an unknown kind were skipped", r.unrecognized_containers));
            }
        }
        FtpProbeOutcome::NotSent { cause } => out.push_str(&format!("NOT SENT: {cause}")),
        FtpProbeOutcome::Indeterminate { data_sent, cause } => out.push_str(&format!(
            "NO USABLE REPLY after the op was sent{} ({cause}). THE CAMERA MAY HAVE ACTED ON IT",
            if *data_sent { " with its data-out" } else { "" }
        )),
    }
    out.push_str(&format!("\nbefore: {}", list(&p.before)));
    match &p.after {
        Ok(a) => {
            out.push_str(&format!("\nafter:  {}", list(a)));
            let changes = p.before.changes_to(a);
            if changes.is_empty() {
                out.push_str("\nthe profile list is unchanged");
            } else {
                out.push_str("\n*** the profile list CHANGED ***");
                for c in changes {
                    out.push_str(&match c {
                        FtpListChange::Info(f) => format!("\n  {} differs", f.name()),
                        FtpListChange::ProfileChanged { index, fields } if fields.is_empty() => {
                            format!("\n  profile [{index}]: its bytes differ (no decoded field accounts for it)")
                        }
                        FtpListChange::ProfileChanged { index, fields } => {
                            format!("\n  profile [{index}]: {} differ", fields.join(", "))
                        }
                        FtpListChange::ProfileAdded { index } => format!("\n  profile [{index}] is new"),
                        FtpListChange::ProfileRemoved { index } => format!("\n  profile [{index}] is gone"),
                    });
                }
            }
        }
        Err(e) => out.push_str(&format!("\nafter:  could not be read in this session ({e}); run vendor-read-ftp to see the list")),
    }
    out
}

/// `vendor-get-prop`: one standard-PTP property read, printed as bytes (and as a string when the
/// bytes are one). Read-only.
fn print_iptc_bytes(data: &[u8]) {
    println!("{} byte(s): {}", data.len(), nikon_fleet::ptp_usb::hex_bytes(data));
    let (entries, rest) = split_length_prefixed(data);
    for (i, e) in entries.iter().enumerate() {
        println!("  entry {i}: {e:?}");
    }
    if !rest.is_empty() {
        println!("  {} byte(s) after the last whole entry: {}", rest.len(), nikon_fleet::ptp_usb::hex_bytes(rest));
    }
}

fn cmd_vendor_set_byte_prop(args: &VendorSetBytePropArgs) -> Result<()> {
    let r = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::set_byte_property_locked(lock, serial, args.prop, args.value, args.force_unconfirmed_model)
    })
    .with_context(|| format!("writing property {:#06x} over USB PTP; if the reply never came the camera MAY have changed: read it with vendor-get-prop", args.prop))?;
    println!("{:#06x} was {}; wrote {}", r.propcode, nikon_fleet::ptp_usb::hex_bytes(&r.previous), r.written);
    match &r.read_back {
        Ok(b) => println!("read back {}: {}", nikon_fleet::ptp_usb::hex_bytes(b), if r.verified() { "verified" } else { "NOT what was written" }),
        Err(e) => println!("could not be read back: {e}"),
    }
    if r.verified() {
        Ok(())
    } else {
        Err(ExitCodeError { code: EXIT_WRITE_INDETERMINATE, message: "the camera answered OK but the read-back does not confirm the preset".to_string() }.into())
    }
}

fn cmd_vendor_write_iptc(args: &VendorWriteIptcArgs) -> Result<()> {
    let entries: Vec<&str> = args.entries.iter().map(String::as_str).collect();
    let data = match &args.data_hex {
        Some(hex) => nikon_fleet::extended::hex_decode(&hex.split_whitespace().collect::<String>())
            .map_err(|e| anyhow::anyhow!("--data-hex: {e}"))?,
        None => nikon_fleet::ptp_usb::encode_iptc_entries(&entries),
    };
    println!("data phase for 0x9413({}):", args.slot);
    print_iptc_bytes(&data);
    if args.dry_run {
        return Ok(());
    }
    let out = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::write_iptc_slot_locked(lock, serial, args.slot, &data, args.force_unconfirmed_model)
    })
    .with_context(|| format!("writing IPTC slot {} (0x9413) over USB PTP; if the reply never came the camera MAY have changed: read the slot again with vendor-read-iptc", args.slot))?;
    println!("\nbefore:");
    print_iptc_bytes(&out.before.data);
    println!("\n0x9413 answered OK (response params {:?})\n\nafter:", out.response_params);
    match &out.after {
        Ok(after) => print_iptc_bytes(&after.data),
        Err(e) => println!("  could not be read back: {e}"),
    }
    Ok(())
}

fn cmd_vendor_read_iptc(args: &VendorReadIptcArgs) -> Result<()> {
    let read = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::read_iptc_slot_locked(lock, serial, args.slot)
    })
        .with_context(|| format!("reading IPTC slot {} (0x9412) over USB PTP", args.slot))?;
    for w in &read.warnings {
        eprintln!("warning: {w}");
    }
    println!("0x9412({}) returned {} byte(s), response params {:?}", args.slot, read.data.len(), read.response_params);
    println!("{}", nikon_fleet::ptp_usb::hex_bytes(&read.data));
    let (entries, rest) = split_length_prefixed(&read.data);
    for (i, e) in entries.iter().enumerate() {
        println!("entry {i}: {e:?}");
    }
    if !rest.is_empty() {
        println!("{} byte(s) left over after the last whole entry: {}", rest.len(), nikon_fleet::ptp_usb::hex_bytes(rest));
    }
    Ok(())
}

/// Best-effort split of `[u32 LE length incl. NUL][bytes]` entries (the 0x9413 data-phase shape). Stops at the first entry that
/// does not fit; the unsplit tail is returned so nothing is hidden.
fn split_length_prefixed(data: &[u8]) -> (Vec<String>, &[u8]) {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if pos + 4 + len > data.len() {
            break;
        }
        let body = &data[pos + 4..pos + 4 + len];
        out.push(String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(body)).into_owned());
        pos += 4 + len;
    }
    (out, &data[pos..])
}

fn cmd_vendor_get_prop(args: &VendorGetPropArgs) -> Result<()> {
    let read = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::get_device_prop_locked(lock, serial, args.prop, args.desc)
    })
        .with_context(|| {
            format!(
                "reading property 0x{:04x} with {} over USB PTP",
                args.prop,
                if args.desc { "GetDevicePropDesc (0x1014)" } else { "GetDevicePropValue (0x1015)" }
            )
        })?;
    for w in &read.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", vendor_get_prop_text(args.prop, args.desc, &read.data));
    Ok(())
}

/// `vendor-sweep`: read every descriptor in the blocks and keep the dump. A sweep that had to stop early is still
/// saved (what it found is real) and the command then fails, so a script does not take it for a whole one.
fn cmd_vendor_sweep(data_dir: &Path, args: &VendorSweepArgs) -> Result<()> {
    use nikon_fleet::prop_sweep::{SweepOptions, default_ranges, normalize, sweep};
    let ranges = if args.ranges.is_empty() { default_ranges() } else { args.ranges.clone() };
    let total: u32 = normalize(&ranges).iter().map(|r| r.count()).sum();
    let opts = SweepOptions {
        ranges,
        label: args.label.clone(),
        captured_at: OffsetDateTime::now_utc().format(&Rfc3339).context("formatting the time")?,
    };
    eprintln!("sweeping {total} property code(s); read-only");
    let dump = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        let mut session = nikon_fleet::ptp_usb::PtpUsbSession::open_locked(lock, serial)?;
        sweep(&mut session, &opts, &mut |p| {
            if p.done > 0 && p.done % 256 == 0 {
                eprintln!("  {} / {total} (at 0x{:04x})", p.done, p.code);
            }
        })
    })
    .context("sweeping property descriptors over USB PTP")?;
    let path = dump.save_new(&sweeps_dir(data_dir)).context("saving the sweep")?;
    let c = dump.counts;
    println!(
        "{}: asked {}, present {}, not supported {}, refused {}, no usable answer {}
saved {}",
        dump.body.model,
        c.asked,
        c.present,
        c.not_supported,
        c.refused,
        c.failed,
        path.display()
    );
    if let Some(ab) = &dump.aborted {
        bail!("the sweep stopped at {}: {}. What it found before that is saved; it is not a whole sweep", ab.at, ab.reason);
    }
    Ok(())
}

/// A sweep named on the command line: the path as given, or else a file name in `<data-dir>/sweeps/`.
fn resolve_sweep_path(data_dir: &Path, given: &Path) -> PathBuf {
    if given.exists() || given.components().count() > 1 {
        given.to_path_buf()
    } else {
        sweeps_dir(data_dir).join(given)
    }
}

fn cmd_vendor_sweep_diff(data_dir: &Path, args: &VendorSweepDiffArgs) -> Result<()> {
    use nikon_fleet::prop_sweep::{SweepDump, diff_sweeps};
    let load = |p: &Path| -> Result<SweepDump> {
        let path = resolve_sweep_path(data_dir, p);
        SweepDump::load(&path).with_context(|| format!("reading {}", path.display()))
    };
    let (a, b) = (load(&args.a)?, load(&args.b)?);
    let noise = args.noise.as_deref().map(load).transpose()?;
    let d = diff_sweeps(&a, &b, noise.as_ref(), args.keep_noise);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&d)?);
    } else {
        print!("{}", d.to_text(&a, &b));
    }
    Ok(())
}

/// `vendor-set-string`: write one string property and say whether the camera holds it.
fn cmd_vendor_set_string(args: &VendorSetStringArgs) -> Result<()> {
    use nikon_fleet::ptp_usb::{plan_string_property_locked, set_string_property_locked, PtpUsbError};
    if args.dry_run {
        // Everything the real write checks before it sends (the property, the model and property gates,
        // the text, the descriptor), by the same library function: a dry run must not suggest a change
        // that cannot be made.
        let plan = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
            plan_string_property_locked(lock, serial, args.prop, &args.text, args.force_unconfirmed_model)
        })
            .map_err(|e| match e.downcast::<PtpUsbError>() {
                Ok(e) => anyhow::Error::new(e).context("checking the write over USB PTP (dry run: nothing was sent)"),
                Err(other) => other,
            })?;
        for w in &plan.warnings {
            eprintln!("warning: {w}");
        }
        println!("{}", vendor_set_string_dry_run_text(&plan, &args.text));
        return Ok(());
    }
    let receipt = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        set_string_property_locked(lock, serial, args.prop, &args.text, args.force_unconfirmed_model)
    })
        .map_err(|e| match e.downcast::<PtpUsbError>() {
            Ok(e @ PtpUsbError::PropWriteIndeterminate { .. }) => anyhow::Error::new(e)
                .context("the camera may have changed: the value was sent before this failed; read it with vendor-get-prop before deciding whether to repeat the write"),
            Ok(other) => anyhow::Error::new(other).context("writing the string property over USB PTP"),
            Err(other) => other,
        })?;
    for w in &receipt.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", vendor_set_string_text(&receipt));
    if let Some(e) = string_write_unconfirmed(&receipt) {
        return Err(e.into());
    }
    Ok(())
}

/// The exit-4 error for a string write the camera accepted but whose read-back did not confirm the value sent
/// (`None` when it did). The camera MAY now hold something other than what it had, so this is the same status as a
/// write whose reply was lost, not a plain failure that invites a blind retry.
fn string_write_unconfirmed(receipt: &nikon_fleet::ptp_usb::PropWriteReceipt) -> Option<ExitCodeError> {
    if receipt.verified {
        return None;
    }
    Some(ExitCodeError {
        code: EXIT_WRITE_INDETERMINATE,
        message: format!(
            "the camera accepted the write but the read-back did not confirm the value that was sent; the camera MAY have \
             changed. The previous value was {:?}; read the property with vendor-get-prop before deciding whether to repeat the write",
            receipt.previous
        ),
    })
}

/// The name `STRING_PROPS` gives `propcode`, for messages. One lookup for both `vendor-set-string` texts.
fn string_prop_label(propcode: u16) -> &'static str {
    nikon_fleet::ptp_usb::STRING_PROPS.iter().find(|(c, _)| *c == propcode).map(|(_, n)| *n).unwrap_or("?")
}

/// What `vendor-set-string --dry-run` prints once every check the write makes has passed.
fn vendor_set_string_dry_run_text(plan: &nikon_fleet::ptp_usb::StringPropPlan, text: &str) -> String {
    format!(
        "{} ({:#06x}): currently {:?}. Would write {text:?}. Nothing was sent.",
        string_prop_label(plan.propcode),
        plan.propcode,
        plan.previous
    )
}

/// What `vendor-set-string` prints for a write the camera accepted. The previous value is always
/// on the first line: it is what to write back to undo this.
fn vendor_set_string_text(r: &nikon_fleet::ptp_usb::PropWriteReceipt) -> String {
    let name = string_prop_label(r.propcode);
    let head = format!("{name} ({:#06x}): previous {:?}, sent {:?}.", r.propcode, r.previous, r.written);
    match (&r.read_back, r.verified) {
        (Ok(v), true) => format!("{head}\nRead back {v:?}: the camera holds what was sent."),
        (Ok(v), false) => format!("{head}\nRead back {v:?}: NOT what was sent."),
        (Err(why), _) => format!("{head}\nNot confirmed: {why}."),
    }
}

/// What `vendor-get-prop` prints. A value that does not decode as a PTP string is reported as such
/// next to its bytes, never dropped: a property that is not a string is a normal case here.
fn vendor_get_prop_text(prop: u16, desc: bool, data: &[u8]) -> String {
    let what = if desc { "descriptor" } else { "value" };
    let mut out = format!("0x{prop:04x} {what}: {} byte(s): {}", data.len(), nikon_fleet::ptp_usb::hex_bytes(data));
    if !desc {
        match nikon_fleet::ptp_usb::decode_ptp_string_value(data) {
            Ok(text) => out.push_str(&format!("\nas a PTP string ({} char(s)): {text:?}", text.chars().count())),
            Err(why) => out.push_str(&format!("\nnot a PTP string: {why}")),
        }
    }
    out
}

/// What `vendor-op-support` prints. Deliberately says nothing like "supported": the response
/// code and the bytes are the whole report.
fn vendor_op_query_text(q: &nikon_fleet::ptp_usb::VendorOpQuery) -> String {
    use nikon_fleet::ptp_usb::{hex_bytes, PtpResponseCode};
    // `PtpResponseCode`'s Display already carries the hex code, e.g. "OK (0x2001)".
    let mut line = format!("0x944C(0x{:04x}, 0): response {}", q.target_op, PtpResponseCode(q.response_code));
    if q.data_containers == 0 {
        line.push_str(", no data phase");
    } else {
        line.push_str(&format!(
            ", {} byte(s) in {} data container(s): {}",
            q.data.len(),
            q.data_containers,
            hex_bytes(&q.data)
        ));
    }
    if !q.response_params.is_empty() {
        line.push_str(&format!("\nresponse parameters: {:?}", q.response_params));
    }
    for d in &q.deviations {
        line.push_str(&format!("\n{}", reply_deviation_note(d)));
    }
    if q.unrecognized_containers > 0 {
        line.push_str(&format!("\nnote: {} container(s) of an unknown kind were skipped", q.unrecognized_containers));
    }
    line
}

/// Read and decode the camera's FTP profile via raw USB PTP. Read-only. See
/// `nikon_fleet::ptp_usb::PtpUsbSession::read_ftp_profile`.
fn cmd_vendor_read_ftp(args: &VendorReadFtpArgs) -> Result<()> {
    let readout = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::read_ftp_profile_locked(lock, serial, args.slot)
    })
        .context("reading the FTP profile over USB PTP")?;
    for w in &readout.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", ftp_readout_text(&readout, args.show_password, args.raw));
    if let Some(path) = &args.save_blob {
        use std::io::Write;
        let Some(profile) = &readout.profile else {
            bail!("--save-blob: the camera reported no active profile, so nothing was read; pass --slot N");
        };
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        // The file holds the profile's password. Owner-only where the platform has modes;
        // elsewhere the directory's own permissions apply.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(path)
            .with_context(|| format!("creating {} (it must not already exist)", path.display()))?;
        f.write_all(&profile.raw).with_context(|| format!("writing {}", path.display()))?;
        f.sync_all().with_context(|| format!("syncing {}", path.display()))?;
        println!("Saved {} raw byte(s) to {}", profile.raw.len(), path.display());
    }
    Ok(())
}

/// The one-line note for a way a tolerated reply departed from its command.
fn reply_deviation_note(d: &nikon_fleet::ptp_usb::ReplyDeviation) -> String {
    use nikon_fleet::ptp_usb::ReplyDeviation;
    match d {
        ReplyDeviation::TransactionId { got } => format!("note: a reply container carried transaction id {got}, not the command's"),
        ReplyDeviation::DataOp { got } => format!("note: a data container carried operation code 0x{got:04x}, not the command's"),
    }
}

/// What `vendor-read-ftp` prints. The password and the 5 GHz field (probably a
/// Wi-Fi password) are masked, in the decoded fields and in the hex dump,
/// unless `show_password`. After the profile come the notes of each reply that carried
/// response parameters, a transaction-id or operation-code departure, or a skipped container.
fn ftp_readout_text(r: &nikon_fleet::ptp_usb::FtpProfileReadout, show_password: bool, raw: bool) -> String {
    let mut out = ftp_readout_body(r, show_password, raw);
    for n in r.replies.iter().filter(|n| !n.is_empty()) {
        out.push_str(&format!("\n0x{:04x} reply:", n.op));
        if !n.response_params.is_empty() {
            out.push_str(&format!(" response parameters {:?};", n.response_params));
        }
        for d in &n.deviations {
            out.push_str(&format!("\n  {}", reply_deviation_note(d)));
        }
        if n.unrecognized_containers > 0 {
            out.push_str(&format!("\n  note: {} container(s) of an unknown kind were skipped", n.unrecognized_containers));
        }
    }
    out
}

fn ftp_readout_body(r: &nikon_fleet::ptp_usb::FtpProfileReadout, show_password: bool, raw: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let i = &r.info;
    let _ = writeln!(
        out,
        "camera: {}  (0x90EB: count {}, limit {}, slot {})",
        i.device_name.text,
        i.count,
        i.limit,
        if i.has_active_profile() { i.slot.to_string() } else { "none (0xff)".to_string() }
    );
    out.push_str(&format!(
        "info GUID:      {}  (device name {:?}, trailer 0x{:04x})\n",
        nikon_fleet::ptp_usb::hex_bytes(&i.guid),
        i.device_name.text,
        i.trailer
    ));
    let Some(p) = &r.profile else {
        out.push_str(
            "the camera reports no active FTP profile (slot 0xff; 0x90ED not sent). Profiles may still be stored on it: this does not say there are none.",
        );
        return out;
    };
    match &p.decoded {
        Ok(d) => {
            let shown = if show_password { d.clone() } else { d.with_secrets_masked() };
            let n = d.camera_network;
            let _ = writeln!(out, "profile name:   {}", shown.profile_name);
            let _ = writeln!(out, "SSID 2.4 GHz:   {}", shown.ssid_24ghz);
            if d.ssid24_lead != 0x03 {
                let _ = writeln!(out, "                (tag lead byte 0x{:02x}, not the usual 0x03; seen on profiles made in the camera's menu)", d.ssid24_lead);
            }
            let _ = writeln!(out, "5 GHz field:    {}  (probably the Wi-Fi password; tag 0x{:02x}04)", shown.ssid_5ghz, d.ssid5_kind);
            let _ = writeln!(out, "host:           {}", shown.host);
            let _ = writeln!(out, "FTP port:       {}", shown.ftp_port);
            let _ = writeln!(out, "folder:          {}", shown.folder);
            let _ = writeln!(out, "username:       {}", shown.username);
            let _ = writeln!(out, "password:       {}", shown.password);
            let _ = writeln!(
                out,
                "option flags:   0x{:04x}{}",
                shown.option_flags,
                if d.is_sftp() { "  (bit 0x08 = SFTP)" } else { "" }
            );
            let _ = writeln!(out, "second port:    {}  (8080 in most blobs; probably the proxy port)", shown.second_port);
            let _ = write!(
                out,
                "camera address: {}.{}.{}.{}/{}",
                n.ipv4[0], n.ipv4[1], n.ipv4[2], n.ipv4[3], n.prefix_len
            );
        }
        Err(e) => {
            let _ = write!(out, "the {}-byte profile data was not decoded: {e}", p.raw.len());
        }
    }
    if raw {
        let masked = if show_password {
            Some(p.raw.clone())
        } else {
            // A blob that did not decode has no known password position, so nothing is printed rather
            // than something that might carry it.
            p.decoded.as_ref().ok().and_then(|_| mask_ftp_secrets_in_dump(&p.raw))
        };
        let Some(bytes) = masked else {
            let _ = write!(
                out,
                "\nraw dump withheld: the {}-byte profile did not decode, so the password cannot be masked in it. \
                 Pass --show-password to print the bytes as they are.",
                p.raw.len()
            );
            return out;
        };
        let _ = write!(out, "\nraw ({} bytes): {}", bytes.len(), nikon_fleet::ptp_usb::hex_bytes(&bytes));
    }
    out
}

/// `raw` (a profile blob that decodes) with every byte of its two secrets, the password and the string
/// the encoder calls the 5 GHz SSID, zeroed. Two layers, because either alone fails:
/// - by POSITION, from the decoder's own offsets (`decode_ftp_profile_spans`): the real bytes are
///   zeroed whatever they spell, so a secret that also appears in an earlier field cannot be mistaken
///   for it. (The content pass below also reaches these bytes, so removing this pass changes no
///   output today; it stays as the primary mask so that a change to the content pass cannot unmask
///   the field itself.)
/// - by CONTENT, every occurrence in the blob, overlapping ones included, in UTF-8 and in UTF-16LE (the
///   encoding of the profile name and the timestamps): a copy of the secret in another field (a
///   username equal to the password, a password inside the SSID) must not carry it into the dump.
///
/// `None` when `raw` does not decode. The length never changes. An empty secret masks nothing.
fn mask_ftp_secrets_in_dump(raw: &[u8]) -> Option<Vec<u8>> {
    let (decoded, spans) = nikon_fleet::ptp_usb::decode_ftp_profile_spans(raw).ok()?;
    let mut out = raw.to_vec();
    for span in spans.ranges() {
        out[span.clone()].fill(0);
    }
    for secret in &decoded.secret_texts() {
        if secret.is_empty() {
            continue;
        }
        let utf16le: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
        for needle in [secret.as_bytes(), utf16le.as_slice()] {
            for (at, window) in raw.windows(needle.len()).enumerate() {
                if window == needle {
                    out[at..at + needle.len()].fill(0);
                }
            }
        }
    }
    Some(out)
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

/// The preamble every raw-USB vendor command shares: take the camera lock, resolve the serial
/// (`vendor_locked_target`), and run `f` with both. A library error stays a typed
/// `PtpUsbError` inside the `anyhow::Error`, so callers can still downcast it.
fn with_vendor_camera<T>(
    serial: Option<&str>,
    f: impl FnOnce(CameraLock, Option<&str>) -> Result<T, nikon_fleet::ptp_usb::PtpUsbError>,
) -> Result<T> {
    vendor_locked_target(CameraLock::acquire, serial, usb_camera_list)
        .and_then(|(lock, serial)| f(lock, serial.as_deref()).map_err(anyhow::Error::new))
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

    // A guarded replace of slot 0 (what this command has always sent) with the read-back of the blob write:
    // the camera's OK shows little, so only a read-back that matches is a clean finish.
    let outcome = with_vendor_camera(args.serial.as_deref(), |lock, serial| {
        nikon_fleet::ptp_usb::write_ftp_profile_verified_locked(
            lock,
            serial,
            &profile,
            &nikon_fleet::ptp_usb::FtpCameraNetwork::CAPTURED,
            args.force_unconfirmed_model,
        )
    })
        .map_err(|e| match e.downcast::<nikon_fleet::ptp_usb::PtpUsbError>() {
            Ok(e) => vendor_write_error(e),
            Err(other) => other,
        })?;
    println!("{}", ftp_write_success_text(&args.profile_name, &args.username, &args.host, args.port));
    if let Some(note) = ftp_receipt_note(&outcome.receipt) {
        eprintln!("{note}");
    }
    ftp_verification_result(&outcome.verification)
}

/// What `vendor-write-ftp` prints for a write the camera answered OK to. `port` is the u16 the encoder
/// writes AFTER the password (`DecodedFtpProfile::second_port`), not the FTP port: the encoder fixes
/// that at 21, so the line must not read like `user@host:port`.
fn ftp_write_success_text(profile_name: &str, username: &str, host: &str, port: u16) -> String {
    format!("Wrote FTP profile {profile_name:?} ({username}@{host}; FTP port 21 as the encoder fixes it, second port {port})")
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
/// to go on (this path does not read the profile back; `vendor-write-ftp-blob` does). A reply that did not
/// match the command, or that carried parameters (none seen in any capture), is worth saying so the OK is
/// not over-trusted.
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
            "note: the camera answered OK, but {}. The OK does not show what the profile now holds: check it in the camera's menu (or with `vendor-read-ftp --slot 0`: a written profile goes to the front).",
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

    // The live capture is MAID only; a reference's extended section is not a difference from it.
    let d = diff_with_schema(&reference, &live, &DiffOptions { skip_extended: true, ..Default::default() }, &schema)?;
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
    held_to(args.expect_digest.as_deref(), args.expect_summary.as_deref(), digest, summary)
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

fn cmd_restore_extended(data_dir: &Path, args: &RestoreExtendedArgs) -> Result<()> {
    use nikon_fleet::ops::restore_extended::{
        Category, ExtendedApplyError, ExtendedApplyRequest, ExtendedRestoreInput, apply_extended_restore, build_extended_restore_plan,
    };
    use nikon_fleet::ptp_usb::PtpUsbSession;
    let refused = |e: &dyn std::fmt::Display| -> anyhow::Error {
        ExitCodeError { code: EXIT_REFUSED_BEFORE_WRITE, message: format!("{e}") }.into()
    };
    let path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot = Snapshot::from_json(std::str::from_utf8(&bytes).context("the snapshot is not UTF-8")?)
        .with_context(|| format!("loading {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let only: Option<std::collections::BTreeSet<Category>> = if args.only.is_empty() {
        None
    } else {
        Some(args.only.iter().map(|c| Category::parse(c)).collect::<Result<_, _>>().map_err(|e| refused(&e))?)
    };
    let mut target = snapshot.camera.clone();
    if let Some(s) = &args.serial {
        target.serial = s.clone();
    }
    let serial = args.serial.clone().or_else(|| Some(snapshot.camera.serial.clone())).filter(|s| !s.is_empty());
    let input = ExtendedRestoreInput { source: &snapshot, source_bytes: &bytes, source_name: &name, target: &target, only: only.as_ref() };
    let snapshots = snapshots_dir(data_dir);

    // One raw session does everything, so the plan an apply recomputes is of the body it just read.
    let outcome = with_vendor_camera(serial.as_deref(), |lock, serial| {
        let mut session = PtpUsbSession::open_locked(lock, serial)?;
        let planned = if args.expect_digest.is_some() { None } else { Some(build_extended_restore_plan(&mut session, &input)) };
        if args.dry_run {
            return Ok((planned, None));
        }
        let (digest, summary) = match (&planned, &args.expect_digest, &args.expect_summary) {
            (_, Some(d), Some(s)) => (d.clone(), s.clone()),
            (Some(Ok(p)), ..) => (p.digest.clone(), p.summary.clone()),
            (Some(Err(_)), ..) => return Ok((planned, None)),
            _ => unreachable!("an expected digest comes with its summary (clap `requires`)"),
        };
        let req = ExtendedApplyRequest {
            input,
            snapshots_dir: &snapshots,
            expected_digest: &digest,
            expected_summary: &summary,
            captured_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default(),
        };
        let applied = apply_extended_restore(&mut session, &req, &mut |p| {
            if !args.json {
                eprintln!("  [{p}]");
            }
        });
        Ok((planned, Some(applied)))
    })?;
    let (planned, applied) = outcome;

    if let Some(plan) = &planned {
        match plan {
            Ok(p) if args.dry_run || applied.is_none() => {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(p)?);
                } else {
                    print!("{}", p.to_text());
                }
                return Ok(());
            }
            Ok(p) if !args.json => print!("{}", p.to_text()),
            Ok(_) => {}
            Err(e) => return Err(refused(e)),
        }
    }
    match applied {
        None => Ok(()),
        Some(Err(ExtendedApplyError::Apply(nikon_fleet::ops::apply::ApplyError::StalePlan { what }))) => Err(ExitCodeError {
            code: EXIT_STALE_PLAN,
            message: format!("the plan no longer matches the body ({what} differs); nothing was written. Run --dry-run again"),
        }
        .into()),
        Some(Err(e)) => Err(refused(&e)),
        Some(Ok(report)) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.to_text());
            }
            use nikon_fleet::ops::apply::ApplyStatus;
            match report.status {
                ApplyStatus::AppliedVerified => Ok(()),
                ApplyStatus::Indeterminate => Err(ExitCodeError {
                    code: EXIT_WRITE_INDETERMINATE,
                    message: "a write may or may not have taken effect: read the body again (`snapshot`) before deciding what to do".to_string(),
                }
                .into()),
                _ => bail!("the restore did not complete cleanly (status {:?}); see the lines above", report.status),
            }
        }
    }
}

/// Restore settings onto the camera. An apply that fails before it has a report in hand (every
/// `RestoreApplyError` is raised before the first write) exits `EXIT_REFUSED_BEFORE_WRITE`, as a
/// transplant does (see `transplant_exit`).
fn cmd_restore(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &RestoreArgs, no_usb_reset: bool) -> Result<()> {
    let mut report_in_hand = false;
    let result = restore_run(data_dir, bundle, schema_path, args, no_usb_reset, &mut report_in_hand);
    restore_finish(result, args, report_in_hand)
}

/// How a restore ends: an apply that failed with no report in hand wrote nothing and exits
/// `EXIT_REFUSED_BEFORE_WRITE`; a stale plan keeps its own status; anything once a report is in hand,
/// and any dry run, keeps the plain failure (`transplant_exit`).
fn restore_finish(result: Result<()>, args: &RestoreArgs, report_in_hand: bool) -> Result<()> {
    result.map_err(|e| transplant_exit(e, args.dry_run, !report_in_hand))
}

/// One line of what a restore prints, in the order it is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestoreOut {
    /// Progress and results: stdout, or stderr when `--json` keeps stdout for the report.
    Say(String),
    /// A warning: stderr.
    Warn(String),
    /// The JSON document `--json` prints on stdout.
    Json(String),
}

/// What `restore_on_port` produced, in order. Nothing is printed while it runs, so a test can read
/// it, and so the caller prints it whether the restore succeeded or failed afterwards.
#[derive(Debug, Default)]
struct RestoreProduct {
    out: Vec<RestoreOut>,
}

impl RestoreProduct {
    fn say(&mut self, text: String) {
        self.out.push(RestoreOut::Say(text));
    }

    /// The JSON the run printed, if it printed any.
    fn json(&self) -> Option<&str> {
        self.out.iter().find_map(|o| if let RestoreOut::Json(j) = o { Some(j.as_str()) } else { None })
    }

    fn print(&self, json: bool, json_out: &mut nikon_fleet::json_out::JsonOut) -> Result<()> {
        for o in &self.out {
            match o {
                RestoreOut::Say(t) => say(json, t),
                RestoreOut::Warn(w) => eprintln!("{w}"),
                RestoreOut::Json(j) => json_out.emit(j)?,
            }
        }
        Ok(())
    }
}

/// The restore proper, on any camera: plan, then (unless `--dry-run`) apply held to the
/// confirmation and report. Everything the call-site decides lives here, so it is tested with a fake
/// camera: which digest and summary the apply is held to, and that `report_in_hand` is set exactly
/// when `apply_restore` has returned a report (after which a failure may have followed a write).
struct RestoreSetup<'a> {
    snap: &'a Snapshot,
    source_name: &'a str,
    source_bytes: &'a [u8],
    only: Option<&'a std::collections::BTreeSet<String>>,
    identity: &'a Camera,
    snapshots_dir: &'a Path,
    name_lookup: &'a HashMap<u32, String>,
    captured_at: String,
}

fn restore_on_port<P: nikon_fleet::ops::port::CameraPort + ?Sized>(
    port: &P,
    args: &RestoreArgs,
    setup: &RestoreSetup<'_>,
    product: &mut RestoreProduct,
    report_in_hand: &mut bool,
) -> Result<()> {
    use nikon_fleet::ops::baseline::GatePolicy;
    use nikon_fleet::ops::restore_flow::{BodyPolicy, RestoreApplyRequest, RestorePlanInput, apply_restore, build_restore_plan};

    let json = args.json;
    // The CLI picked the body by serial (including the id-N and SDK-id forms the MCP refuses), so
    // only the model is checked here. A different model is a transplant, not a restore.
    let input = RestorePlanInput {
        source: setup.snap,
        source_name: setup.source_name,
        source_bytes: setup.source_bytes,
        target: setup.identity,
        only: setup.only,
        body_policy: BodyPolicy::SameModel,
    };
    let plan = build_restore_plan(port, &input).map_err(|r| {
        // A program reading --json cannot act on the advice to run another command.
        if json { anyhow::anyhow!("{r}") } else { anyhow::anyhow!("{r}\nUse `nikon-fleet transplant` to copy settings between models.") }
    })?;
    let named = setup.only.map(|o| o.len());

    if args.dry_run {
        product.say(cli_report::render_restore_dry_run(&plan.steps, &plan.counts, setup.only.is_some()));
        for line in cli_report::restore_caveat_lines(&plan.caveats) {
            product.say(format!("{line}\n"));
        }
        product.say(format!("Plan: {}\nDigest: {}\n", plan.summary, plan.digest));
        if json {
            product.out.push(RestoreOut::Json(serde_json::to_string_pretty(&plan)?));
        }
        // The plan is printed first and then a plan that names what cannot be restored fails the
        // command (exit 1): a reader of --json gets the plan either way and tests the plan's own
        // `will_not_write` list, not the status.
        if let Some(msg) = not_written_error(named, &plan.will_not_write) {
            bail!("{msg}");
        }
        return Ok(());
    }

    // A value the read-back cannot confirm is said before anything is written, not after a
    // "verified" verdict.
    for line in cli_report::restore_caveat_lines(&plan.caveats) {
        product.out.push(RestoreOut::Warn(line));
    }

    // Unless the caller presents a plan it approved (--expect-digest, the way the GUI does), the
    // plan above is this command's own and running it is the consent.
    let confirmation = restore_confirmation(args, &plan.digest, &plan.summary);
    let req = RestoreApplyRequest {
        plan: input,
        snapshots_dir: setup.snapshots_dir,
        name_lookup: setup.name_lookup,
        expected_digest: confirmation.0,
        expected_summary: confirmation.1,
        captured_at: setup.captured_at.clone(),
        gate_policy: if args.allow_partial { GatePolicy::AllowPartial } else { GatePolicy::Strict },
    };
    let report = after_restore_apply(apply_restore(port, &req, &mut |_| {}), json, report_in_hand)?;

    for w in cli_report::restore_write_warnings(&report.steps) {
        product.out.push(RestoreOut::Warn(w));
    }
    product.say(cli_report::restore_tally("Restore complete:", &report.counts, setup.only.is_some()));
    product.say(cli_report::render_restore_apply(&report));
    if let Some(v) = &report.verification {
        let (line, _) = restore_verification_text(v);
        product.say(format!("{line}\n"));
    }
    if json {
        product.out.push(RestoreOut::Json(serde_json::to_string_pretty(&report)?));
    }

    let readback_failed = report.verification.as_ref().map(|v| restore_verification_text(v).1).unwrap_or(false);
    if let Some(msg) = cli_report::restore_failure(&report, named, readback_failed) {
        bail!("{msg}");
    }
    Ok(())
}

/// The result of `apply_restore`: a `RestoreApplyError` is a refusal before any write; a report in
/// hand means the camera may have been written, which `report_in_hand` records for `transplant_exit`.
fn after_restore_apply<R>(
    result: Result<R, nikon_fleet::ops::restore_flow::RestoreApplyError>,
    json: bool,
    report_in_hand: &mut bool,
) -> Result<R> {
    let report = result.map_err(|e| restore_refusal(e, json))?;
    *report_in_hand = true;
    Ok(report)
}

/// The body a restore writes to: the one `--serial` names, else the one the snapshot's own serial
/// names. A serial given with `--serial` is the person's say; the snapshot's own is trusted only
/// when it is a USB serial or no second body of its model could be mistaken for it
/// (`select_for_snapshot_serial`). A refusal here is an error before any write, so a real restore
/// ends with `EXIT_REFUSED_BEFORE_WRITE` (`restore_finish`) like every other target refusal.
fn restore_target(
    enriched: Vec<(DeviceInfo, Option<UsbCameraInfo>)>,
    serial_arg: Option<&str>,
    snap: &Snapshot,
) -> Result<(DeviceInfo, Option<UsbCameraInfo>)> {
    let target_serial = serial_arg.unwrap_or(&snap.camera.serial);
    let selected = match serial_arg {
        Some(s) => select_by_cli_serial(enriched, s),
        None => select_for_snapshot_serial(enriched, &snap.camera.serial, &snap.camera.model),
    };
    selected.map_err(|e| match e {
        TargetError::NotFound { .. } => anyhow::anyhow!(
            "no connected camera with serial {}  (snapshot is for {} {})",
            target_serial, snap.camera.model, snap.camera.serial
        ),
        other => anyhow::anyhow!("{other}"),
    })
}

fn restore_run(
    data_dir: &Path,
    bundle: &Path,
    schema_path: &Path,
    args: &RestoreArgs,
    no_usb_reset: bool,
    report_in_hand: &mut bool,
) -> Result<()> {
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

    let (target, usb_info) = restore_target(enriched, args.serial.as_deref(), &snap)?;

    say(json, &format!("Connecting to {:?} (id={})…\n", target.name, target.id));
    let device = sdk.connect(target.id)?;
    // Everything from here runs with the camera session open. However it ends, an early `?` included, `end_session`
    // decides the exit and bounds the disconnect before `device` and `sdk` drop (finding 8): the teardown is armed in
    // this one place, not after whichever step happened to print the result.
    let result = (|| -> Result<()> {
        ensure_capabilities_present(device.capabilities.len())?;

        let schema = MaidLayerConfig::parse_file(schema_path)
            .with_context(|| format!("loading schema from {}", schema_path.display()))?;
        let name_lookup = schema.name_map_for_model(&target.name);
        let identity = target_identity(&target, &usb_info);
        let snapshots = snapshots_dir(data_dir);
        let setup = RestoreSetup {
            snap: &snap,
            source_name: &source_name,
            source_bytes: &source_bytes,
            only: only.as_ref(),
            identity: &identity,
            snapshots_dir: &snapshots,
            name_lookup: &name_lookup,
            captured_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        };
        let mut product = RestoreProduct::default();
        let result = restore_on_port(&device, args, &setup, &mut product, report_in_hand);
        product.print(json, &mut json_out)?;
        result
    })();
    end_session(&result, args.dry_run, *report_in_hand);
    result
}

/// The one-line read-back report for the CLI, and whether it is a failure.
/// `residual_found` fails; an incomplete read-back is a warning: nothing is
/// claimed either way, and the command's other checks have already passed.
fn restore_verification_text(v: &restore::Verification) -> (String, bool) {
    (restore::readback_line(v.verdict, v.residual.as_ref()), v.verdict.is_failure())
}

/// The digest and summary an apply is held to: the plan a person approved and presented
/// (`--expect-digest`, `--expect-summary`), else the one just computed, whose approval is the
/// running of the command itself.
fn transplant_confirmation<'a>(args: &'a TransplantArgs, digest: &'a str, summary: &'a str) -> (&'a str, &'a str) {
    held_to(args.expect_digest.as_deref(), args.expect_summary.as_deref(), digest, summary)
}

/// The one rule behind `transplant_confirmation` and `restore_confirmation`: what the caller
/// presented, else what this run computed.
fn held_to<'a>(expect_digest: Option<&'a str>, expect_summary: Option<&'a str>, digest: &'a str, summary: &'a str) -> (&'a str, &'a str) {
    (expect_digest.unwrap_or(digest), expect_summary.unwrap_or(summary))
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
    if keeps_its_own_status(&e, dry_run, wrote_nothing) {
        return e;
    }
    ExitCodeError { code: nikon_fleet::ops::apply::EXIT_REFUSED_BEFORE_WRITE, message: format!("{e:?}") }.into()
}

/// Whether [`transplant_exit`] leaves `e` as it is. The one place the rule lives, shared by the error mapping and
/// [`transplant_exit_status`].
fn keeps_its_own_status(e: &anyhow::Error, dry_run: bool, wrote_nothing: bool) -> bool {
    dry_run || !wrote_nothing || e.downcast_ref::<ExitCodeError>().is_some()
}

/// The exit status a failed transplant or restore will end with, for a caller that has to know it before the error
/// is returned (the teardown watchdog, which may have to end the process itself). Always equal to
/// `exit_code_for(&transplant_exit(e, ..))`; a test pins that.
fn transplant_exit_status(e: &anyhow::Error, dry_run: bool, wrote_nothing: bool) -> i32 {
    if keeps_its_own_status(e, dry_run, wrote_nothing) {
        exit_code_for(e)
    } else {
        nikon_fleet::ops::apply::EXIT_REFUSED_BEFORE_WRITE
    }
}

/// How a command that held a camera session leaves once its result is out. The decision is a value, apart from the
/// act, so a test can pin which status and which line each way of ending gets (finding 79: flipping the `report_in_hand`
/// test below once survived every test).
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    /// A dry run: nothing was written, so end the process now without closing the session (F24: the SDK's
    /// `DisconnectDevice` hangs on about 1 run in 14 and there is nothing left to close for).
    Leave { status: i32, error_line: Option<String> },
    /// An apply: the camera may have been written, so keep the ordinary disconnect, bounded by the teardown watchdog
    /// (`DisconnectDevice` hung in 10 of 20 write applies on a Z 9, every time after a verified result;
    /// docs/mcp-hardware-findings.md F26).
    Bound { status: i32, error_line: Option<String> },
}

/// The line `main` prints for a failed run, `Error: ` and the error with its causes: what returning the error from
/// `main` used to print. One function, because the teardown watchdog prints the same line itself when it ends the
/// process before `main` gets to.
fn error_line(e: &anyhow::Error) -> String {
    format!("Error: {e:?}")
}

/// The exit a command that held a camera session will make, from how it ended. `report_in_hand` is whether the apply
/// returned a report (so the camera may have been written); `status` is what `main` will end with for `result`
/// (`transplant_exit_status`), and `error_line` the line `main` would print for it, which the watchdog prints itself
/// because `main` is never reached when it fires.
fn session_end(result: &Result<()>, dry_run: bool, report_in_hand: bool) -> SessionEnd {
    let (status, error_line) = match result {
        Ok(()) => (0, None),
        Err(e) => (transplant_exit_status(e, dry_run, !report_in_hand), Some(error_line(e))),
    };
    if dry_run { SessionEnd::Leave { status, error_line } } else { SessionEnd::Bound { status, error_line } }
}

/// Carry out [`session_end`]: the one place a restore or transplant arms the teardown watchdog or leaves with
/// `exit_now`, called once after the connected part of the command has ended whichever way (not after whichever step
/// printed the result). `Bound` returns, so `device` and `sdk` drop with the watchdog already armed.
fn end_session(result: &Result<()>, dry_run: bool, report_in_hand: bool) {
    match session_end(result, dry_run, report_in_hand) {
        SessionEnd::Leave { status, error_line } => {
            if let Some(line) = error_line {
                eprintln!("{line}");
            }
            nikon_fleet::process_exit::exit_now(status)
        }
        SessionEnd::Bound { status, error_line } => {
            nikon_fleet::process_exit::arm_teardown_watchdog(nikon_fleet::process_exit::TEARDOWN_LIMIT, status, error_line)
        }
    }
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
    // Everything from here runs with the camera session open: however it ends, an early `?` (a refusal such as a stale
    // plan, a failed print) included, `end_session` decides the exit and bounds the disconnect before `device` and
    // `sdk` drop (finding 8). A dry run wrote nothing and leaves at once; an apply keeps the ordinary disconnect, bounded.
    let result = (|| -> Result<()> {
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
        // A failing report ends 1, as this `bail!` does (a report is in hand).
        if let Some(msg) = closing.failing {
            bail!("{msg}");
        }
        Ok(())
    })();
    end_session(&result, args.dry_run, *report_in_hand);
    result
}

// ─────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────

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
        Cmd::Edit(sub) => cmd_edit(&cli.data_dir, sub),
        Cmd::Baseline(sub) => cmd_baseline(sub),
        Cmd::Settings(sub) => cmd_settings(sub),
        #[cfg(unix)]
        Cmd::Worker(args) => {
            let code = worker_cmd::run(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args);
            nikon_fleet::diag::shutdown(code);
            std::process::exit(code)
        }
        Cmd::Restore(args) => cmd_restore(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Transplant(args) => cmd_transplant(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::VendorRead(args) => cmd_vendor_read(args),
        Cmd::DeviceInfo(args) => cmd_device_info(args),
        Cmd::VendorWriteFtp(args) => cmd_vendor_write_ftp(args),
        Cmd::VendorReadFtp(args) => cmd_vendor_read_ftp(args),
        Cmd::VendorWriteFtpBlob(args) => cmd_vendor_write_ftp_blob(args),
        Cmd::VendorOpSupport(args) => cmd_vendor_op_support(args),
        Cmd::RestoreExtended(args) => cmd_restore_extended(&cli.data_dir, args),
        Cmd::VendorGetProp(args) => cmd_vendor_get_prop(args),
        Cmd::VendorSweep(args) => cmd_vendor_sweep(&cli.data_dir, args),
        Cmd::VendorSweepDiff(args) => cmd_vendor_sweep_diff(&cli.data_dir, args),
        Cmd::VendorReadIptc(args) => cmd_vendor_read_iptc(args),
        Cmd::VendorWriteIptc(args) => cmd_vendor_write_iptc(args),
        Cmd::VendorSetByteProp(args) => cmd_vendor_set_byte_prop(args),
        Cmd::VendorSetString(args) => cmd_vendor_set_string(args),
        Cmd::DiagReport(args) => cmd_diag_report(args),
        Cmd::VendorProbeOp(args) => cmd_vendor_probe_op(args),
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

/// `diag-report`: what each recent process was doing, from the files they left.
fn cmd_diag_report(args: &DiagReportArgs) -> Result<()> {
    let dir = match &args.dir {
        Some(d) => d.clone(),
        None => nikon_fleet::diag::dir().context(
            "there is no diagnostics directory: no home directory is set and NIKON_FLEET_DIAG_DIR is not set",
        )?,
    };
    let traces = nikon_fleet::diag::load(&dir, &nikon_fleet::diag::pid_alive)
        .with_context(|| format!("reading the diagnostics directory {}", dir.display()))?;
    println!("diagnostics directory: {}", dir.display());
    print!("{}", nikon_fleet::diag::render(&traces, nikon_fleet::diag::unix_ms(), args.last));
    Ok(())
}

/// Start diagnostics for this process and open the span that covers the whole command. Never
/// fatal: if they cannot be set up the command runs without them and says so once.
fn start_diagnostics(trace: Option<&Path>, command: &str) -> Option<nikon_fleet::diag::Span<'static>> {
    use nikon_fleet::diag::{self, Class, Subsystem, Val};
    diag::start(Subsystem::Cli, trace)?;
    Some(diag::span(Subsystem::Cli, "command", Class::Top, &[("name", Val::text(command))]))
}

/// The subcommand name clap matched ("vendor-read", "firmware add"), from clap itself, never by
/// looking at the argument text.
fn matched_command_name(matches: &clap::ArgMatches) -> String {
    match matches.subcommand() {
        Some((name, sub)) => match sub.subcommand_name() {
            Some(inner) => format!("{name} {inner}"),
            None => name.to_string(),
        },
        None => String::new(),
    }
}

fn main() {
    use nikon_fleet::diag::{self, Outcome};
    let mut matches = Cli::command().get_matches();
    // Before `from_arg_matches_mut`, which takes the matches apart: afterwards there is no subcommand left to name.
    let command_name = matched_command_name(&matches);
    let cli = match Cli::from_arg_matches_mut(&mut matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // Test hook, not an option: with this variable set the command line is parsed and nothing runs.
    // tests/gui_argv_cli.rs feeds it every argv the GUI builds. An environment
    // variable rather than a flag, so no wrapper script can turn a real command into a silent
    // exit 0 by adding an argument.
    if std::env::var_os("NIKON_FLEET_PARSE_ONLY").is_some_and(|v| v == "1") {
        return;
    }
    // `diag-report` only reads: it must not start a recorder of its own and add a process to what it reports.
    let span = if matches!(cli.cmd, Cmd::DiagReport(_)) {
        None
    } else {
        start_diagnostics(cli.trace.as_deref(), &command_name)
    };
    match run(cli) {
        Ok(()) => {
            if let Some(s) = span {
                s.finish(Outcome::Ok);
            }
            diag::shutdown(0);
        }
        Err(e) => {
            // Exactly what returning the error from `main` printed, with the exit status chosen
            // by `exit_code_for` instead of always 1.
            let code = exit_code_for(&e);
            if let Some(s) = span {
                s.finish(nikon_fleet::exit_status::diag_outcome_for_exit(code));
            }
            diag::shutdown(code);
            eprintln!("{}", error_line(&e));
            std::process::exit(code);
        }
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
    // The GUI uses: format!("{model with ' ' as '_'}_{serial}.json")
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
    fn not_written_error_is_none_only_when_everything_named_was_written() {
        use nikon_fleet::ops::restore::NotWritten;
        assert_eq!(not_written_error(Some(3), &[]), None);
        let one = not_written_error(Some(1), &[NotWritten::new("ISO", "read_only")]).unwrap();
        assert!(one.contains("1 of 1 named property was not written") && one.contains("ISO (read_only)"), "{one}");
        let two = not_written_error(Some(5), &[NotWritten::new("A", "never_copied"), NotWritten::new("B", "write_failed")]).unwrap();
        assert!(two.contains("2 of 5 named properties were not written"), "{two}");
        assert!(two.contains("A (never_copied), B (write_failed)"), "{two}");
    }

    #[test]
    fn a_full_restore_never_fails_for_routine_skips() {
        // Without --only a restore skips absent, read-only and state properties
        // all the time; that must not become an error.
        use nikon_fleet::ops::restore::NotWritten;
        let skips = [NotWritten::new("A", "read_only"), NotWritten::new("B", "never_copied")];
        assert_eq!(not_written_error(None, &skips), None);
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
        use nikon_fleet::firmware_browse::BrowseError;
        use nikon_fleet::firmware_source::FetchError;
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
        use nikon_fleet::firmware_browse::BrowseError;
        use nikon_fleet::firmware_source::FetchError;
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
        // vendor-set-string's own indeterminate error (a property write whose reply was lost), wrapped as that command
        // wraps it: exit 4, the same as the FTP write (the arm for it could be deleted with every test green).
        let prop = || nikon_fleet::ptp_usb::PtpUsbError::PropWriteIndeterminate {
            propcode: 0xD090,
            cause: Box::new(nikon_fleet::ptp_usb::PtpUsbError::SessionPoisoned { cause: "x".into() }),
        };
        let set_string: [anyhow::Error; 3] = [
            prop().into(),
            anyhow::Error::new(prop()).context("the camera may have changed: the value was sent before this failed"),
            Err::<(), _>(prop()).context("a").context("writing the string property over USB PTP").unwrap_err(),
        ];
        for e in &set_string {
            assert_eq!(exit_code_for(e), EXIT_WRITE_INDETERMINATE, "vendor-set-string: {e:?}");
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

    fn ftp_readout_with_profile() -> nikon_fleet::ptp_usb::FtpProfileReadout {
        use nikon_fleet::ptp_usb::*;
        ftp_readout_for(FtpProfile {
            profile_name: "CLAUDE".into(),
            ssid_24ghz: "nikon-snoop".into(),
            ssid_5ghz: "nikon5ghz".into(),
            host: "192.168.66.1".into(),
            port: 8080,
            username: "fleet".into(),
            password: "s3cretpw".into(),
        })
    }

    fn ftp_readout_for(profile: nikon_fleet::ptp_usb::FtpProfile) -> nikon_fleet::ptp_usb::FtpProfileReadout {
        use nikon_fleet::ptp_usb::*;
        let blob = encode_ftp_profile(&profile).unwrap();
        let mut info = vec![0xc8, 0x00];
        info.extend_from_slice(&7u32.to_le_bytes());
        info.extend_from_slice(&0x14u32.to_le_bytes());
        info.extend_from_slice(&0u32.to_le_bytes());
        info.extend_from_slice(&[0xAB; 16]);
        info.extend_from_slice(&[0x03, b'Z', 0, b'9', 0, 0, 0]);
        info.extend_from_slice(&[0xf4, 0x01]);
        FtpProfileReadout {
            info: parse_ftp_profile_info(&info).unwrap(),
            profile: Some(FtpProfileBlob { decoded: decode_ftp_profile(&blob), raw: blob }),
            warnings: vec![],
            reply_matched_command: true,
            replies: vec![],
        }
    }

    #[test]
    fn vendor_read_ftp_masks_the_password_in_fields_and_in_the_hex_dump_by_default() {
        let r = ftp_readout_with_profile();
        let masked = ftp_readout_text(&r, false, true);
        assert!(masked.contains("password:       ********"), "{masked}");
        assert!(!masked.contains("s3cretpw") && !masked.contains("nikon5ghz"), "{masked}");
        assert!(
            !masked.contains(&nikon_fleet::ptp_usb::hex_bytes(b"nikon5ghz")),
            "the 5 GHz field (probably a Wi-Fi password) is zeroed in the dump too: {masked}"
        );
        let hex_of_pw = nikon_fleet::ptp_usb::hex_bytes(b"s3cretpw");
        assert!(!masked.contains(&hex_of_pw), "the dump must not carry the password bytes: {masked}");
        assert!(masked.contains("profile name:   CLAUDE") && masked.contains("FTP port:       21") && masked.contains("second port:    8080"), "{masked}");
        let shown = ftp_readout_text(&r, true, true);
        assert!(shown.contains("s3cretpw") && shown.contains("nikon5ghz") && shown.contains(&hex_of_pw), "{shown}");
    }

    #[test]
    fn vendor_read_ftp_withholds_the_hex_dump_when_the_blob_did_not_decode_and_the_password_is_unmasked() {
        let mut r = ftp_readout_with_profile();
        let p = r.profile.as_mut().unwrap();
        p.raw[0] = 0; // not a recognised blob any more
        p.decoded = nikon_fleet::ptp_usb::decode_ftp_profile(&p.raw);
        assert!(p.decoded.is_err());
        let withheld = ftp_readout_text(&r, false, true);
        assert!(withheld.contains("raw dump withheld") && !withheld.contains("raw ("), "{withheld}");
        assert!(!withheld.contains(&nikon_fleet::ptp_usb::hex_bytes(b"s3cretpw")), "{withheld}");
        let shown = ftp_readout_text(&r, true, true);
        assert!(shown.contains(&nikon_fleet::ptp_usb::hex_bytes(b"s3cretpw")), "--show-password prints the bytes: {shown}");
    }

    #[test]
    fn vendor_read_ftp_prints_the_notes_of_each_reply_that_has_any_and_nothing_for_clean_ones() {
        use nikon_fleet::ptp_usb::{FtpReplyNotes, ReplyDeviation};
        let note = |op| FtpReplyNotes { op, response_params: vec![], deviations: vec![], unrecognized_containers: 0 };
        let mut r = ftp_readout_with_profile();
        r.replies = vec![note(0x90E8), note(0x90EB), note(0x90ED)];
        let clean = ftp_readout_text(&r, false, false);
        assert!(!clean.contains("reply:") && !clean.contains("note:"), "{clean}");
        r.replies = vec![
            FtpReplyNotes { response_params: vec![5, 6], ..note(0x90E8) },
            note(0x90EB),
            FtpReplyNotes {
                deviations: vec![ReplyDeviation::TransactionId { got: 99 }, ReplyDeviation::DataOp { got: 0x1234 }],
                unrecognized_containers: 2,
                ..note(0x90ED)
            },
        ];
        let text = ftp_readout_text(&r, true, true);
        assert!(text.contains("0x90e8 reply: response parameters [5, 6];"), "{text}");
        assert!(!text.contains("0x90eb reply"), "a clean reply prints nothing: {text}");
        assert!(text.contains("0x90ed reply:") && text.contains("transaction id 99") && text.contains("operation code 0x1234"), "{text}");
        assert!(text.contains("2 container(s) of an unknown kind were skipped"), "{text}");
        // The notes follow the whole profile, the raw dump included, and the dump still parses.
        assert!(text.find("raw (").unwrap() < text.find("0x90e8 reply").unwrap(), "{text}");
        assert_eq!(dump_bytes(&text), r.profile.as_ref().unwrap().raw);
        // And when the camera reports no profile the notes are still there.
        r.profile = None;
        r.info.slot = 0xff;
        assert!(ftp_readout_text(&r, false, false).contains("0x90e8 reply: response parameters [5, 6];"));
    }

    #[test]
    fn vendor_read_ftp_says_so_when_the_camera_holds_no_profile() {
        let mut r = ftp_readout_with_profile();
        r.profile = None;
        r.info.slot = 0xff;
        let text = ftp_readout_text(&r, false, false);
        assert!(text.contains("slot none (0xff)") && text.contains("no active FTP profile"), "{text}");
    }

    /// The bytes of the `raw (N bytes): ...` line of a `vendor-read-ftp --raw` output.
    fn dump_bytes(text: &str) -> Vec<u8> {
        let line = text.lines().find(|l| l.starts_with("raw (")).unwrap_or_else(|| panic!("no raw dump in:\n{text}"));
        let hex = line.split_once("): ").expect("raw line has the byte count").1;
        hex.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex byte")).collect()
    }

    fn occurrences(hay: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() {
            return 0;
        }
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// A profile whose secrets are the given strings and whose other fields are the defaults of
    /// `ftp_readout_with_profile`.
    fn profile_with(f: impl FnOnce(&mut nikon_fleet::ptp_usb::FtpProfile)) -> nikon_fleet::ptp_usb::FtpProfile {
        let mut p = nikon_fleet::ptp_usb::FtpProfile {
            profile_name: "CLAUDE".into(),
            ssid_24ghz: "nikon-snoop".into(),
            ssid_5ghz: "nikon5ghz".into(),
            host: "192.168.66.1".into(),
            port: 8080,
            username: "fleet".into(),
            password: "s3cretpw".into(),
        };
        f(&mut p);
        p
    }

    /// b7d3#01/#75: the masked dump used to zero only the FIRST occurrence of each secret in the blob,
    /// and the username, SSIDs, host and name all come before the password. Any secret that is equal to,
    /// or inside, an earlier field was left in the dump. Every case below leaked before.
    #[test]
    fn vendor_read_ftp_raw_mask_zeroes_every_copy_of_the_secrets_not_just_the_first() {
        use nikon_fleet::ptp_usb::decode_ftp_profile_spans;
        type Edit = fn(&mut nikon_fleet::ptp_usb::FtpProfile);
        let cases: [(&str, Edit); 11] = [
            ("username equals the password", |p| {
                p.username = "samepw".into();
                p.password = "samepw".into();
            }),
            ("password inside the username", |p| {
                p.username = "xxsamepwxx".into();
                p.password = "samepw".into();
            }),
            ("password inside the 2.4 GHz SSID", |p| {
                p.ssid_24ghz = "net-samepw-24".into();
                p.password = "samepw".into();
            }),
            ("5 GHz field equals the 2.4 GHz SSID", |p| {
                p.ssid_24ghz = "shared-net".into();
                p.ssid_5ghz = "shared-net".into();
            }),
            ("5 GHz field inside the host", |p| {
                p.host = "ftp.wifi5g.example".into();
                p.ssid_5ghz = "wifi5g".into();
            }),
            ("password equals the 5 GHz field and the username", |p| {
                p.username = "allthree".into();
                p.ssid_5ghz = "allthree".into();
                p.password = "allthree".into();
            }),
            ("overlapping matches (aa inside aaaa)", |p| {
                p.username = "aaaa".into();
                p.password = "aa".into();
            }),
            ("password equals the profile name (stored as UTF-16)", |p| {
                p.profile_name = "Name1".into();
                p.password = "Name1".into();
            }),
            ("password inside the folder-less host and the name", |p| {
                p.host = "pwhost.example".into();
                p.profile_name = "my-pwhost".into();
                p.password = "pwhost".into();
            }),
            ("password empty", |p| {
                p.password = String::new();
            }),
            ("both secrets empty", |p| {
                p.password = String::new();
                p.ssid_5ghz = String::new();
            }),
        ];
        for (name, edit) in cases {
            let profile = profile_with(edit);
            let secrets = [profile.password.clone(), profile.ssid_5ghz.clone()];
            let r = ftp_readout_for(profile);
            let raw = r.profile.as_ref().unwrap().raw.clone();
            let (_, spans) = decode_ftp_profile_spans(&raw).unwrap();
            let out = ftp_readout_text(&r, false, true);
            let dump = dump_bytes(&out);
            assert_eq!(dump.len(), raw.len(), "{name}: the length never changes");
            assert_eq!(dump[..2], raw[..2], "{name}: structural bytes outside the secrets are kept");
            assert_eq!(spans.spans.len(), 2, "the password and the 5 GHz field");
            for span in spans.ranges() {
                assert!(dump[span.clone()].iter().all(|b| *b == 0), "{name}: the field itself is zeroed by position");
            }
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                let utf16: Vec<u8> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
                assert_eq!(occurrences(&dump, secret.as_bytes()), 0, "{name}: {secret:?} (UTF-8) still in the dump:\n{out}");
                assert_eq!(occurrences(&dump, &utf16), 0, "{name}: {secret:?} (UTF-16) still in the dump:\n{out}");
            }
            // --show-password prints the true bytes.
            assert_eq!(dump_bytes(&ftp_readout_text(&r, true, true)), raw, "{name}");
        }
    }

    #[test]
    fn vendor_read_ftp_raw_mask_leaves_unrelated_fields_and_an_all_empty_secret_dump_untouched() {
        // Nothing to mask: the dump is the blob.
        let p = profile_with(|p| {
            p.password = String::new();
            p.ssid_5ghz = String::new();
        });
        let r = ftp_readout_for(p);
        assert_eq!(dump_bytes(&ftp_readout_text(&r, false, true)), r.profile.as_ref().unwrap().raw);
        // Distinct secrets: only they change; the host, the SSID and the username survive byte for byte.
        let r = ftp_readout_with_profile();
        let dump = dump_bytes(&ftp_readout_text(&r, false, true));
        for kept in ["nikon-snoop", "fleet"] {
            assert_eq!(occurrences(&dump, kept.as_bytes()), 1, "{kept}");
        }
        // The host is a PTP string: UTF-16LE.
        let host: Vec<u8> = "192.168.66.1".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(occurrences(&dump, &host), 1);
    }

    /// A run of overlapping copies is zeroed as a whole: "aa" inside "aaa" has matches at 0 and 1, and
    /// skipping past the first match would leave the last byte of the run, a byte of the password.
    #[test]
    fn overlapping_copies_of_a_secret_are_zeroed_as_one_run() {
        let r = ftp_readout_for(profile_with(|p| {
            p.username = "aaa".into();
            p.password = "aa".into();
        }));
        let raw = r.profile.as_ref().unwrap().raw.clone();
        let dump = dump_bytes(&ftp_readout_text(&r, false, true));
        let at = raw.windows(7).position(|w| w == [3, 0, 0, 0, b'a', b'a', b'a']).expect("the username field");
        assert_eq!(dump[at + 4..at + 7], [0, 0, 0], "all three bytes of the run, not just the first match");
        assert_eq!(dump[at..at + 4], raw[at..at + 4], "its length prefix is kept");
    }

    #[test]
    fn the_raw_mask_comes_from_the_blob_not_from_the_decoded_copy_that_was_handed_in() {
        // The masking decodes the raw bytes itself: a readout whose `decoded` side was altered cannot
        // steer which bytes are zeroed.
        let mut r = ftp_readout_with_profile();
        let p = r.profile.as_mut().unwrap();
        if let Ok(d) = p.decoded.as_mut() {
            d.password = "not-the-password".into();
            d.ssid_5ghz = "not-the-5ghz".into();
        }
        let dump = dump_bytes(&ftp_readout_text(&r, false, true));
        assert_eq!(occurrences(&dump, b"s3cretpw"), 0);
        assert_eq!(occurrences(&dump, b"nikon5ghz"), 0);
    }

    fn blob_plan(kind: nikon_fleet::ptp_usb::FtpWriteKind, count: u32) -> nikon_fleet::ptp_usb::FtpWritePlan {
        let blob = nikon_fleet::ptp_usb::encode_ftp_profile(&profile_with(|_| {})).unwrap();
        nikon_fleet::ptp_usb::FtpWritePlan {
            model: "Z 9".into(),
            kind,
            count,
            decoded: nikon_fleet::ptp_usb::decode_ftp_profile(&blob).unwrap(),
            info_replies_matched: true,
        }
    }

    /// b7d3#05/#12: the plan must say where the profile ends up and what the count becomes, and must
    /// not call the add "untested" (it was done on a Z 9 over USB on 2026-11-05).
    #[test]
    fn the_blob_plan_names_the_reorder_and_the_count_for_each_form() {
        use nikon_fleet::ptp_usb::FtpWriteKind::*;
        let replace = ftp_blob_plan_text(&blob_plan(Replace, 3), 2, 0, 325);
        assert!(replace.contains("Z 9: 3 stored profile(s). Would REPLACE the profile in slot 2"), "{replace}");
        assert!(replace.contains("FRONT (slot 0)") && replace.contains("the count stays 3") && replace.contains("0x90EE(2, 0)"), "{replace}");
        assert!(replace.contains("CLAUDE") && replace.contains("325 byte(s)"), "{replace}");
        let front = ftp_blob_plan_text(&blob_plan(Replace, 3), 0, 0, 325);
        assert!(front.contains("REPLACE the profile in slot 0 (it stays at the front"), "{front}");
        let add = ftp_blob_plan_text(&blob_plan(Add, 3), 3, 1, 325);
        assert!(add.contains("ADD a new profile at the FRONT (slot 0)") && add.contains("the count becomes 4") && add.contains("0x90EE(3, 1)"), "{add}");
        for text in [&replace, &front, &add] {
            assert!(!text.contains("untested"), "{text}");
        }
        // The count's u32 edge does not wrap in the sentence.
        assert!(ftp_blob_plan_text(&blob_plan(Add, u32::MAX), u32::MAX, 1, 1).contains("the count becomes 4294967295"));
    }

    fn outcome_with(v: nikon_fleet::ptp_usb::FtpWriteVerification) -> nikon_fleet::ptp_usb::FtpWriteOutcome {
        nikon_fleet::ptp_usb::FtpWriteOutcome {
            receipt: nikon_fleet::ptp_usb::FtpWriteReceipt { response_params: vec![], reply_matched_command: true },
            kind: nikon_fleet::ptp_usb::FtpWriteKind::Replace,
            count_before: 3,
            verification: v,
        }
    }

    #[test]
    fn the_blob_outcome_reports_the_read_back_and_never_sends_the_reader_to_the_written_slot() {
        use nikon_fleet::ptp_usb::{FtpWriteVerification as V, PtpUsbError};
        let verified = ftp_blob_outcome_text(&outcome_with(V::Verified { count_after: 3 }));
        assert!(verified.contains("slot 0 holds exactly the profile that was sent") && verified.contains("3 profile(s)"), "{verified}");
        assert!(verified.contains("front of the list"), "{verified}");
        assert!(!verified.contains("--slot 2") && !verified.contains("Read it back with"), "{verified}");
        let mismatch = ftp_blob_outcome_text(&outcome_with(V::Mismatch {
            count_after: 4,
            count_expected: 3,
            differing_fields: vec!["host", "password"],
        }));
        assert!(mismatch.contains("READ-BACK MISMATCH") && mismatch.contains("4 profile(s) (expected 3)") && mismatch.contains("host, password"), "{mismatch}");
        let count_only = ftp_blob_outcome_text(&outcome_with(V::Mismatch { count_after: 2, count_expected: 3, differing_fields: vec![] }));
        assert!(count_only.contains("none (only the count is off)"), "{count_only}");
        let undecodable = ftp_blob_outcome_text(&outcome_with(V::ReadBackUndecodable {
            error: PtpUsbError::FtpBlobMalformed { what: "blob header", offset: 0, detail: "x".into() },
        }));
        assert!(undecodable.contains("READ-BACK UNDECODABLE"), "{undecodable}");
        let failed = ftp_blob_outcome_text(&outcome_with(V::ReadBackFailed { error: PtpUsbError::NoDataPhase { op: 0x90ED } }));
        assert!(failed.contains("READ-BACK FAILED") && failed.contains("neither confirmed nor contradicted"), "{failed}");
    }

    /// The encoder-path write (`vendor-write-ftp`) ends like the blob write: only a verified read-back is exit 0,
    /// every other verification is exit 4 ("the camera MAY have changed").
    #[test]
    fn a_verified_write_is_a_clean_finish_and_every_other_verification_is_exit_4() {
        use nikon_fleet::ptp_usb::{FtpWriteVerification as V, PtpUsbError};
        assert!(ftp_verification_result(&V::Verified { count_after: 4 }).is_ok());
        let others = [
            V::Mismatch { count_after: 4, count_expected: 4, differing_fields: vec!["host"] },
            V::Mismatch { count_after: 5, count_expected: 4, differing_fields: vec![] },
            V::ReadBackUndecodable { error: PtpUsbError::NoDataPhase { op: 0x90ED } },
            V::ReadBackFailed { error: PtpUsbError::NoDataPhase { op: 0x90ED } },
        ];
        for v in &others {
            let e = ftp_verification_result(v).expect_err("an unconfirmed write is an error");
            assert_eq!(exit_code_for(&e), 4, "{v:?}");
            assert!(e.to_string().contains("MAY have changed"), "{e}");
        }
    }

    /// #14: a string write the camera accepted but whose read-back differs (it stored something else) or
    /// could not be read is exit 4 ("the camera MAY have changed"), not a plain failure; only a verified
    /// read-back is a clean finish.
    #[test]
    fn a_string_write_that_is_not_confirmed_ends_with_the_may_have_changed_status() {
        use nikon_fleet::ptp_usb::PropWriteReceipt;
        let receipt = |read_back: Result<String, String>, verified: bool| PropWriteReceipt {
            propcode: 0xD090,
            previous: "old".into(),
            written: "new".into(),
            read_back,
            verified,
            warnings: vec![],
        };
        assert!(string_write_unconfirmed(&receipt(Ok("new".into()), true)).is_none());
        for r in [receipt(Ok("ne".into()), false), receipt(Err("read failed".into()), false)] {
            let e = string_write_unconfirmed(&r).expect("an unconfirmed write is an error");
            assert_eq!(e.code, EXIT_WRITE_INDETERMINATE);
            assert!(e.message.contains("\"old\""), "the previous value is named so it can be restored: {}", e.message);
            let any: anyhow::Error = e.into();
            assert_eq!(exit_code_for(&any), 4);
        }
    }

    /// Only a verified read-back is a success; every other result must end the command with the
    /// "camera MAY have changed" status, or a script reads an unconfirmed write as done.
    #[test]
    fn only_a_verified_read_back_is_a_clean_finish() {
        use nikon_fleet::ptp_usb::{FtpWriteVerification as V, PtpUsbError};
        assert_eq!(ftp_blob_unconfirmed(&V::Verified { count_after: 1 }), None);
        let others = [
            V::Mismatch { count_after: 1, count_expected: 2, differing_fields: vec![] },
            V::Mismatch { count_after: 2, count_expected: 2, differing_fields: vec!["host"] },
            V::ReadBackUndecodable { error: PtpUsbError::NoDataPhase { op: 0x90ED } },
            V::ReadBackFailed { error: PtpUsbError::NoDataPhase { op: 0x90ED } },
        ];
        for v in &others {
            assert!(ftp_blob_unconfirmed(v).is_some(), "{v:?}");
        }
        // The status is the documented one.
        let e: anyhow::Error = ExitCodeError { code: EXIT_WRITE_INDETERMINATE, message: "x".into() }.into();
        assert_eq!(exit_code_for(&e), 4);
    }

    #[test]
    fn the_indeterminate_write_advice_is_not_idempotent_and_points_at_the_front() {
        let e = vendor_write_error(nikon_fleet::ptp_usb::PtpUsbError::FtpWriteIndeterminate {
            cause: Box::new(nikon_fleet::ptp_usb::PtpUsbError::NoDataPhase { op: 0x90EE }),
        });
        let text = format!("{e:#}");
        assert!(!text.contains("idempotent"), "{text}");
        assert!(text.contains("FRONT") && text.contains("duplicate") && text.contains("vendor-read-ftp --slot 0"), "{text}");
    }

    #[test]
    fn vendor_write_ftp_success_line_does_not_present_the_second_port_as_the_ftp_port() {
        let line = ftp_write_success_text("CLAUDE", "user", "ftp.example.com", 2121);
        assert!(line.contains("second port 2121") && line.contains("FTP port 21"), "{line}");
        assert!(!line.contains("ftp.example.com:2121"), "{line}");
    }

    #[test]
    fn vendor_set_string_dry_run_text_names_the_property_the_current_value_and_the_new_one() {
        let plan = nikon_fleet::ptp_usb::StringPropPlan { propcode: 0xD090, previous: "old".into(), warnings: vec![], data: vec![] };
        let text = vendor_set_string_dry_run_text(&plan, "new");
        assert_eq!(text, "Image comment (0xd090): currently \"old\". Would write \"new\". Nothing was sent.");
        assert_eq!(string_prop_label(0xD072), "Artist");
        assert_eq!(string_prop_label(0x1234), "?");
    }

    #[test]
    fn parse_hex_bytes_takes_even_runs_of_hex_digits_with_optional_spaces() {
        assert_eq!(parse_hex_bytes("01 aB ff"), Ok(vec![0x01, 0xAB, 0xFF]));
        assert_eq!(parse_hex_bytes("0102"), Ok(vec![1, 2]));
        assert_eq!(parse_hex_bytes(" 0 1 0 2 "), Ok(vec![1, 2]), "whitespace is dropped before pairing");
        assert!(parse_hex_bytes("").is_err(), "empty is not a payload");
        assert!(parse_hex_bytes("   ").is_err());
        assert!(parse_hex_bytes("1").is_err(), "odd length");
        assert!(parse_hex_bytes("012").is_err(), "odd length");
        assert!(parse_hex_bytes("0x01").is_err(), "no 0x prefix here");
        assert!(parse_hex_bytes("zz").is_err());
        assert!(parse_hex_bytes("0g").is_err());
    }

    #[test]
    fn vendor_read_prints_the_data_on_stdout_and_each_warning_on_stderr() {
        use nikon_fleet::ptp_usb::VendorRead;
        let clean = vendor_read_output(0xd053, &VendorRead { data: vec![0xde, 0xad], response_params: vec![], warnings: vec![] });
        assert_eq!(clean, ("0xd053: 2 byte(s): de ad".to_string(), vec![]));
        let warned = vendor_read_output(
            0x5001,
            &VendorRead {
                data: vec![],
                response_params: vec![],
                warnings: vec!["model \"Z 8\" is not surveyed".into(), "second".into()],
            },
        );
        assert_eq!(warned.0, "0x5001: 0 byte(s): ");
        assert_eq!(warned.1, ["warning: model \"Z 8\" is not surveyed", "warning: second"], "one line per warning, none dropped");
    }

    #[test]
    fn vendor_set_string_text_always_leads_with_the_previous_value() {
        use nikon_fleet::ptp_usb::PropWriteReceipt;
        let mut r = PropWriteReceipt {
            propcode: 0xD090,
            previous: "old".into(),
            written: "new".into(),
            read_back: Ok("new".into()),
            verified: true,
            warnings: vec![],
        };
        let t = vendor_set_string_text(&r);
        assert!(t.starts_with("Image comment (0xd090): previous \"old\", sent \"new\"."), "{t}");
        assert!(t.ends_with("the camera holds what was sent."), "{t}");
        r.read_back = Ok("ne".into());
        r.verified = false;
        assert!(vendor_set_string_text(&r).ends_with("Read back \"ne\": NOT what was sent."));
        r.read_back = Err("boom".into());
        let t = vendor_set_string_text(&r);
        assert!(t.contains("previous \"old\"") && t.ends_with("Not confirmed: boom."), "{t}");
    }

    #[test]
    fn the_status_a_transplant_error_will_end_with_is_known_before_it_is_returned() {
        // `transplant_exit_status` must equal what `main` derives from the mapped error, for every combination:
        // a plain error, one that carries its own status, and every (dry run, wrote nothing) pair.
        let fresh: Vec<Box<dyn Fn() -> anyhow::Error>> = vec![
            Box::new(|| anyhow::anyhow!("plain")),
            Box::new(|| ExitCodeError { code: 5, message: "stale".into() }.into()),
            Box::new(|| {
                anyhow::Error::new(nikon_fleet::ptp_usb::PtpUsbError::PropWriteIndeterminate {
                    propcode: 0xD090,
                    cause: Box::new(nikon_fleet::ptp_usb::PtpUsbError::SessionPoisoned { cause: "x".into() }),
                })
            }),
        ];
        for make in &fresh {
            for dry_run in [false, true] {
                for wrote_nothing in [false, true] {
                    let want = exit_code_for(&transplant_exit(make(), dry_run, wrote_nothing));
                    assert_eq!(transplant_exit_status(&make(), dry_run, wrote_nothing), want, "{:?} dry_run={dry_run} wrote_nothing={wrote_nothing}", make());
                }
            }
        }
        // Pinned values: a plain error before any write is "refused before write"; after a report it is the plain 1.
        let plain = anyhow::anyhow!("plain");
        assert_eq!(transplant_exit_status(&plain, false, true), nikon_fleet::ops::apply::EXIT_REFUSED_BEFORE_WRITE);
        assert_eq!(transplant_exit_status(&plain, false, false), 1);
        assert_eq!(transplant_exit_status(&plain, true, true), 1, "a dry run keeps the plain failure");
        let stale: anyhow::Error = ExitCodeError { code: 5, message: "stale".into() }.into();
        assert_eq!(transplant_exit_status(&stale, false, true), 5, "a stale plan keeps its own status");
    }

    // ── how a command that held a camera session leaves (findings 8 and 79) ──────

    fn plain_err() -> Result<()> {
        Err(anyhow::anyhow!("plain"))
    }

    fn stale_err() -> Result<()> {
        Err(ExitCodeError { code: nikon_fleet::exit_status::EXIT_STALE_PLAN, message: "stale".into() }.into())
    }

    /// (leaves at once, status, the error line `main` would print). A line is checked by its start: with
    /// RUST_BACKTRACE set, anyhow's Debug adds a trace after the message.
    fn ended(end: SessionEnd) -> (bool, i32, Option<String>) {
        match end {
            SessionEnd::Leave { status, error_line } => (true, status, error_line),
            SessionEnd::Bound { status, error_line } => (false, status, error_line),
        }
    }

    #[test]
    fn an_apply_that_failed_before_a_report_is_in_hand_ends_6_and_one_with_a_report_ends_1() {
        // The watchdog's status must be the one `main` will end with. Flip `report_in_hand` (the glue: `!report_in_hand`
        // is "wrote nothing") and these two lines swap, which no test used to notice.
        let (leaves, status, line) = ended(session_end(&plain_err(), false, false));
        assert!(!leaves);
        assert_eq!(status, nikon_fleet::ops::apply::EXIT_REFUSED_BEFORE_WRITE, "nothing was written: refused before write");
        assert!(line.is_some_and(|l| l.starts_with("Error: plain")), "the line `main` would print");
        let (_, status, _) = ended(session_end(&plain_err(), false, true));
        assert_eq!(status, 1, "a report is in hand: the camera may have been written");
        // The same status the CLI's own mapping gives the error `cmd_*` returns.
        for report_in_hand in [false, true] {
            let mapped = transplant_exit(anyhow::anyhow!("plain"), false, !report_in_hand);
            let SessionEnd::Bound { status, .. } = session_end(&plain_err(), false, report_in_hand) else { panic!() };
            assert_eq!(status, exit_code_for(&mapped), "report_in_hand={report_in_hand}");
        }
    }

    #[test]
    fn a_stale_plan_keeps_its_own_status_whether_or_not_a_report_is_in_hand() {
        // The stale-plan refusal happens before any write and before connecting finishes: the GUI must see 5, not TimedOut.
        for report_in_hand in [false, true] {
            let (_, status, _) = ended(session_end(&stale_err(), false, report_in_hand));
            assert_eq!(status, 5, "report_in_hand={report_in_hand}");
        }
    }

    #[test]
    fn a_result_that_succeeded_ends_0_with_no_error_line() {
        assert_eq!(ended(session_end(&Ok(()), false, true)), (false, 0, None));
        assert_eq!(ended(session_end(&Ok(()), false, false)), (false, 0, None), "an apply that returned Ok without a report still ends 0");
        assert_eq!(ended(session_end(&Ok(()), true, false)), (true, 0, None));
    }

    #[test]
    fn a_dry_run_leaves_at_once_with_the_plain_failure_status_and_an_apply_never_does() {
        // A dry run wrote nothing: its failures keep the ordinary status whatever report_in_hand says.
        for report_in_hand in [false, true] {
            let (leaves, status, line) = ended(session_end(&plain_err(), true, report_in_hand));
            assert!(leaves && status == 1 && line.is_some_and(|l| l.starts_with("Error: plain")), "report_in_hand={report_in_hand}");
        }
        let (leaves, status, _) = ended(session_end(&stale_err(), true, false));
        assert_eq!((leaves, status), (true, 5));
        // An apply is bounded, never left: `Leave` skips the disconnect, which a write must not.
        for result in [Ok(()), plain_err(), stale_err()] {
            for report_in_hand in [false, true] {
                assert!(matches!(session_end(&result, false, report_in_hand), SessionEnd::Bound { .. }));
            }
        }
    }

    #[test]
    fn the_command_name_comes_from_clap_and_must_be_read_before_the_matches_are_consumed() {
        let mut m = Cli::command().get_matches_from(["fleet", "ls"]);
        assert_eq!(matched_command_name(&m), "ls");
        let _ = Cli::from_arg_matches_mut(&mut m).unwrap();
        // The pin: after the matches are consumed the name is gone, which is why main reads it first.
        assert_eq!(matched_command_name(&m), "", "if this starts returning the name, main's ordering can be simplified");
        let m = Cli::command().get_matches_from(["fleet", "firmware", "ls"]);
        assert_eq!(matched_command_name(&m), "firmware ls");
        let m = Cli::command().get_matches_from(["fleet", "--trace", "/tmp/t.jsonl", "diag-report"]);
        assert_eq!(matched_command_name(&m), "diag-report");
    }

    #[test]
    fn vendor_get_prop_text_decodes_strings_and_reports_what_is_not_one() {
        // "ab" as a PTP string, then padded with a trailing space, then the empty string.
        let t = vendor_get_prop_text(0xD072, false, &[3, b'a', 0, b'b', 0, 0, 0]);
        assert_eq!(t, "0xd072 value: 7 byte(s): 03 61 00 62 00 00 00\nas a PTP string (2 char(s)): \"ab\"");
        assert!(vendor_get_prop_text(0xD072, false, &[3, b'a', 0, b' ', 0, 0, 0]).ends_with("\"a \""));
        assert!(vendor_get_prop_text(0xD072, false, &[0]).ends_with("(0 char(s)): \"\""));
        // Not a string (the four zero bytes 0x943B gave for 0xD073): reported with the reason, not dropped.
        let t = vendor_get_prop_text(0xD073, false, &[0, 0, 0, 0]);
        assert!(t.starts_with("0xd073 value: 4 byte(s): 00 00 00 00\nnot a PTP string: "), "{t}");
        // A descriptor is never string-decoded.
        let t = vendor_get_prop_text(0xD072, true, &[0x72, 0xD0, 0xFF, 0xFF, 1]);
        assert_eq!(t, "0xd072 descriptor: 5 byte(s): 72 d0 ff ff 01");
    }

    #[test]
    fn vendor_op_support_text_reports_code_and_bytes_and_never_says_supported() {
        use nikon_fleet::ptp_usb::VendorOpQuery;
        let base = VendorOpQuery {
            target_op: 0x90ed,
            response_code: 0x2001,
            data: vec![1, 1, 0],
            data_containers: 1,
            reply_matched_command: true,
            response_params: vec![],
            deviations: vec![],
            unrecognized_containers: 0,
            warnings: vec![],
        };
        assert_eq!(vendor_op_query_text(&base), "0x944C(0x90ed, 0): response OK (0x2001), 3 byte(s) in 1 data container(s): 01 01 00");
        let refused = VendorOpQuery { response_code: 0x2005, data: vec![], data_containers: 0, ..base.clone() };
        let text = vendor_op_query_text(&refused);
        assert_eq!(text, "0x944C(0x90ed, 0): response OperationNotSupported (0x2005), no data phase");
        assert!(!text.contains("is supported") && !text.contains("yes"), "no verdict, only the code and bytes: {text}");
        let empty_phase = VendorOpQuery { data: vec![], ..base.clone() };
        assert!(vendor_op_query_text(&empty_phase).contains("0 byte(s) in 1 data container(s)"), "an empty data phase is not 'no data phase'");
        // The reply is the finding for this op: its parameters, how it deviated and any skipped container are printed.
        use nikon_fleet::ptp_usb::ReplyDeviation;
        let mismatched = VendorOpQuery {
            reply_matched_command: false,
            deviations: vec![ReplyDeviation::TransactionId { got: 7 }, ReplyDeviation::DataOp { got: 0x90ec }],
            response_params: vec![5, 6],
            unrecognized_containers: 2,
            ..base
        };
        let text = vendor_op_query_text(&mismatched);
        assert!(text.contains("response parameters: [5, 6]"), "{text}");
        assert!(text.contains("transaction id 7") && text.contains("operation code 0x90ec"), "{text}");
        assert!(text.contains("2 container(s) of an unknown kind"), "{text}");
    }

    // ── vendor-probe-op (b7d3#07, #16, #17, #18) ─────────────────────────

    fn probe_args(extra: &[&str]) -> VendorProbeOpArgs {
        use clap::Parser;
        let mut argv = vec!["nikon-fleet", "vendor-probe-op"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).unwrap().cmd {
            Cmd::VendorProbeOp(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn vendor_probe_op_parses_the_destructive_opt_in_and_the_model_flag_apart() {
        let plain = probe_args(&["0x90EA"]);
        assert_eq!((plain.op, plain.allow_destructive_op, plain.force_unconfirmed_model), (0x90EA, None, false));
        let named = probe_args(&["0x90EA", "--allow-destructive-op", "0x90EA", "--force-unconfirmed-model"]);
        assert_eq!((named.allow_destructive_op, named.force_unconfirmed_model), (Some(0x90EA), true));
        assert!(probe_args(&["0x9FFF"]).allow_destructive_op.is_none());
        use clap::Parser;
        assert!(Cli::try_parse_from(["nikon-fleet", "vendor-probe-op", "0x90EA", "--allow-destructive-op"]).is_err(), "the flag names an op");
    }

    #[test]
    fn a_destructive_probe_that_is_not_named_is_refused_before_the_camera_is_touched() {
        use nikon_fleet::ptp_usb::PtpUsbError;
        // Refused with the typed error and exit status 1: not a USB error (no camera is looked for in
        // this test), not a model error, and not a success.
        for (extra, op) in [
            (&["0x90EA"][..], 0x90EAu16),
            (&["0x90EF", "--param", "1"][..], 0x90EF),
            (&["0x90EA", "--allow-destructive-op", "0x90EF"][..], 0x90EA),
            (&["0x90EF", "--param", "1", "--allow-destructive-op", "0x90EA"][..], 0x90EF),
            (&["0x90EA", "--force-unconfirmed-model"][..], 0x90EA),
        ] {
            let e = refused_probe(extra);
            assert!(
                matches!(e.downcast_ref::<PtpUsbError>(), Some(PtpUsbError::FtpProbeDestructiveNotConfirmed { op: o, .. }) if *o == op),
                "{extra:?}: {e:#}"
            );
            assert_eq!(exit_code_for(&e), 1);
        }
        // The other refusals that need no camera come out the same way.
        let e = refused_probe(&["0x1016"]);
        assert!(matches!(e.downcast_ref::<PtpUsbError>(), Some(PtpUsbError::FtpProbeOpNotAllowed { op: 0x1016 })), "{e:#}");
        let e = refused_probe(&["0x90EC", "--data", &"aa".repeat(33)]);
        assert!(matches!(e.downcast_ref::<PtpUsbError>(), Some(PtpUsbError::FtpProbeDataTooLong { len: 33 })), "{e:#}");
    }

    /// Run `vendor-probe-op` with a lock, a USB listing and a probe that fail the test if they are reached.
    fn refused_probe(extra: &[&str]) -> anyhow::Error {
        let touched = std::cell::Cell::new(false);
        let e = probe_op_with(
            &probe_args(extra),
            || {
                touched.set(true);
                Err(nikon_fleet::camera_lock::LockError::NoPath)
            },
            || {
                touched.set(true);
                vec![]
            },
            |_, _, _, _| panic!("the probe ran for a refused request"),
        )
        .unwrap_err();
        assert!(!touched.get(), "{extra:?}: the camera lock / USB bus was reached before the refusal");
        e
    }

    #[test]
    fn an_allowed_probe_reaches_the_camera_with_its_consent_and_a_lost_reply_ends_with_exit_four() {
        use nikon_fleet::ptp_usb::{FtpProbeConsent, FtpProbeOutcome, FtpProbeReply, PtpUsbError};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("camera.lock");
        let run = |extra: &[&str], outcome: FtpProbeOutcome| {
            let seen = std::cell::RefCell::new(None);
            let r = probe_op_with(
                &probe_args(extra),
                || nikon_fleet::camera_lock::CameraLock::acquire_at(&path),
                Vec::new,
                |_lock, _serial, data, consent| {
                    *seen.borrow_mut() = Some((data.map(<[u8]>::to_vec), consent));
                    let state = probe_state(0, 0xff, vec![]);
                    Ok(probe_of(outcome, state.clone(), Ok(state)))
                },
            );
            (r, seen.into_inner())
        };
        let replied = || {
            FtpProbeOutcome::Replied(FtpProbeReply {
                response_code: 0x2001,
                response_params: vec![],
                data: vec![],
                data_containers: 0,
                deviations: vec![],
                unrecognized_containers: 0,
            })
        };
        let (r, seen) = run(&["0x90EA", "--allow-destructive-op", "0x90EA", "--force-unconfirmed-model"], replied());
        r.unwrap();
        assert_eq!(seen.unwrap().1, FtpProbeConsent { force_unconfirmed_model: true, destructive_op: Some(0x90EA) });
        let (r, seen) = run(&["0x90EC", "--data", "01 02"], replied());
        r.unwrap();
        let (data, consent) = seen.unwrap();
        assert_eq!((data, consent), (Some(vec![1, 2]), FtpProbeConsent::default()));
        // The reply was lost after the op went out: not success, and the code that says "look at the camera".
        let lost = FtpProbeOutcome::Indeterminate { data_sent: false, cause: PtpUsbError::Usb(rusb::Error::Timeout) };
        let (r, _) = run(&["0x90EA", "--allow-destructive-op", "0x90EA"], lost);
        let e = r.unwrap_err();
        assert_eq!(exit_code_for(&e), EXIT_WRITE_INDETERMINATE, "{e:#}");
        // The op never left: an ordinary result, exit 0.
        let unsent = FtpProbeOutcome::NotSent { cause: PtpUsbError::NoDevice };
        run(&["0x9FFF"], unsent).0.unwrap();
    }

    #[test]
    fn the_probe_help_names_every_probe_op_and_the_opt_in() {
        // The help text is hand-written; the list it names is nikon_fleet::ptp_usb::FTP_PROBE_OPS. Nothing
        // generates one from the other, so this fails when they drift.
        let mut cmd = Cli::command();
        let sub = cmd.find_subcommand_mut("vendor-probe-op").expect("the subcommand exists");
        let help = sub.render_long_help().to_string();
        let about = sub.get_long_about().or_else(|| sub.get_about()).map(|s| s.to_string()).unwrap_or_default();
        let all = format!("{help}\n{about}");
        for op in nikon_fleet::ptp_usb::FTP_PROBE_OPS {
            assert!(all.to_lowercase().contains(&format!("0x{op:04x}")), "{op:#06x} is not in the help: {all}");
        }
        assert!(all.contains("--allow-destructive-op") && all.contains("--force-unconfirmed-model"), "{all}");
        assert!(all.contains("DELETES ALL"), "the delete-everything op is described as what it is: {all}");
        assert!(!all.contains("What these ops do is unknown"), "the old, understated wording is gone: {all}");
    }

    fn probe_state(count: u32, slot: u32, entries: Vec<nikon_fleet::ptp_usb::FtpListEntry>) -> nikon_fleet::ptp_usb::FtpListState {
        nikon_fleet::ptp_usb::FtpListState {
            info: nikon_fleet::ptp_usb::FtpProfileInfo {
                header: 200,
                count,
                limit: 20,
                slot,
                guid: [0; 16],
                device_name: ptp_str("Z_9_0000000"),
                trailer: 500,
            },
            entries,
            truncated: false,
        }
    }

    fn probe_of(
        outcome: nikon_fleet::ptp_usb::FtpProbeOutcome,
        before: nikon_fleet::ptp_usb::FtpListState,
        after: Result<nikon_fleet::ptp_usb::FtpListState, nikon_fleet::ptp_usb::PtpUsbError>,
    ) -> nikon_fleet::ptp_usb::FtpOpProbe {
        nikon_fleet::ptp_usb::FtpOpProbe {
            op: 0x90EC,
            risk: nikon_fleet::ptp_usb::FtpProbeRisk::Unexplained,
            params: vec![],
            data: Some(vec![1]),
            setup_first: false,
            before,
            outcome,
            after,
            warnings: vec![],
        }
    }

    #[test]
    fn the_probe_text_reports_a_lost_reply_as_possibly_acted_on_and_says_whether_data_went_out() {
        use nikon_fleet::ptp_usb::{FtpProbeOutcome, PtpUsbError};
        let before = probe_state(0, 0xff, vec![]);
        let lost = probe_of(
            FtpProbeOutcome::Indeterminate { data_sent: true, cause: PtpUsbError::Usb(rusb::Error::Timeout) },
            before.clone(),
            Err(PtpUsbError::SessionPoisoned { cause: "x".into() }),
        );
        let text = ftp_probe_text(&lost);
        let failure: anyhow::Error = probe_indeterminate_error(&lost).expect("a lost reply fails the command").into();
        assert_eq!(exit_code_for(&failure), EXIT_WRITE_INDETERMINATE);
        assert!(failure.to_string().contains("with its data-out") && failure.to_string().contains("may have acted"), "{failure}");
        assert!(text.contains("NO USABLE REPLY after the op was sent with its data-out"), "{text}");
        assert!(text.contains("MAY HAVE ACTED"), "{text}");
        assert!(text.contains("after:  could not be read in this session"), "{text}");
        let bare = probe_of(
            FtpProbeOutcome::Indeterminate { data_sent: false, cause: PtpUsbError::Usb(rusb::Error::Pipe) },
            before.clone(),
            Err(PtpUsbError::SessionPoisoned { cause: "x".into() }),
        );
        let text = ftp_probe_text(&bare);
        assert!(text.contains("after the op was sent (") && !text.contains("data-out ("), "{text}");
        let unsent = probe_of(
            FtpProbeOutcome::NotSent { cause: PtpUsbError::OperationFailed { op: 0x90E8, code: nikon_fleet::ptp_usb::PtpResponseCode(0x2002) } },
            before.clone(),
            Ok(before),
        );
        let text = ftp_probe_text(&unsent);
        assert!(text.contains("NOT SENT") && !text.contains("MAY HAVE ACTED"), "{text}");
        assert!(probe_indeterminate_error(&unsent).is_none(), "an op that never left is an ordinary result");
        let failure: anyhow::Error = probe_indeterminate_error(&bare).unwrap().into();
        assert!(!failure.to_string().contains("with its data-out"), "{failure}");
    }

    #[test]
    fn the_probe_text_names_what_changed_without_printing_any_value() {
        use nikon_fleet::ptp_usb::{FtpListEntry, FtpProbeOutcome, FtpProbeReply};
        let readout = ftp_readout_with_profile();
        let blob = readout.profile.as_ref().unwrap();
        let decoded = blob.decoded.as_ref().unwrap().clone();
        let entry = FtpListEntry { raw: blob.raw.clone(), decoded: Some(decoded.clone()) };
        let mut other = decoded.clone();
        other.password = "pw-changed-sentinel".into();
        let changed = FtpListEntry { raw: vec![9; blob.raw.len()], decoded: Some(other) };
        let reply = FtpProbeOutcome::Replied(FtpProbeReply {
            response_code: 0x2001,
            response_params: vec![3],
            data: vec![],
            data_containers: 0,
            deviations: vec![nikon_fleet::ptp_usb::ReplyDeviation::TransactionId { got: 55 }],
            unrecognized_containers: 1,
        });
        let mut after = probe_state(1, 0, vec![changed]);
        after.info.device_name = ptp_str("");
        let p = probe_of(reply, probe_state(1, 0, vec![entry]), Ok(after));
        let text = ftp_probe_text(&p);
        assert!(text.contains("response OK (0x2001), response parameters [3], no data phase"), "{text}");
        assert!(text.contains("transaction id 55") && text.contains("1 container(s) of an unknown kind"), "{text}");
        assert!(text.contains("*** the profile list CHANGED ***"), "{text}");
        assert!(text.contains("device name differs"), "{text}");
        assert!(text.contains("profile [0]: ") && text.contains("password"), "{text}");
        assert!(!text.contains("pw-changed-sentinel") && !text.contains(&decoded.password), "no value is printed: {text}");
        // An unchanged list says so, and an undecodable profile is counted.
        let same = probe_state(1, 0, vec![FtpListEntry { raw: vec![1, 2, 3], decoded: None }]);
        let p = probe_of(
            FtpProbeOutcome::Replied(FtpProbeReply {
                response_code: 0x2005,
                response_params: vec![],
                data: vec![],
                data_containers: 0,
                deviations: vec![],
                unrecognized_containers: 0,
            }),
            same.clone(),
            Ok(same),
        );
        let text = ftp_probe_text(&p);
        assert!(text.contains("the profile list is unchanged") && text.contains("[1 undecodable]") && text.contains("(undecodable, 3 bytes)"), "{text}");
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

    // ── restore, composed with a fake camera (everything but the SDK) ────────────

    mod restore_on_a_fake_camera {
        use super::*;
        use clap::Parser;
        use nikon_fleet::ops::apply::{EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
        use nikon_fleet::ops::baseline::baseline_for;
        use nikon_fleet::ops::port::{FakeCamera, Failure};
        use nikon_fleet::sdk::{OP_GET, OP_SET};
        use nikon_fleet::snapshot::Transport;

        const RW: u32 = OP_GET | OP_SET;

        struct World {
            cam: FakeCamera,
            names: HashMap<u32, String>,
            targets: Vec<(String, u32)>,
            snap: Snapshot,
            bytes: Vec<u8>,
            identity: Camera,
            dir: tempfile::TempDir,
        }

        /// A Z 9 holding "old" strings and 1 where the stored snapshot says other values.
        fn world(fail_write: Option<usize>) -> World {
            let b = baseline_for("Z 9").unwrap();
            let targets: Vec<(String, u32)> = b.readable.iter().skip(6).take(3).map(|c| (c.name.clone(), c.code)).collect();
            let identity = Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() };
            let mut snap = Snapshot::new(identity.clone(), Transport::Usb, "2026-10-27T10:00:00Z".into());
            let mut cam = FakeCamera::new();
            let mut names = HashMap::new();
            for c in &b.readable {
                let at = targets.iter().position(|(_, code)| *code == c.code);
                let (live, stored) = match at {
                    Some(2) => (serde_json::json!(1), serde_json::json!(55)),
                    Some(i) => (serde_json::json!("old"), serde_json::json!(format!("text {i}"))),
                    None => (serde_json::json!(1), serde_json::json!(1)),
                };
                cam = cam.with(c.code, RW, live);
                names.insert(c.code, c.name.clone());
                snap.insert(c.name.clone(), c.code, stored);
            }
            if let Some(i) = fail_write {
                cam = cam.failing_write(targets[i].1, Failure::Call(-125));
            }
            let bytes = snap.to_pretty_json().unwrap().into_bytes();
            World { cam, names, targets, snap, bytes, identity, dir: tempfile::tempdir().unwrap() }
        }

        fn args(extra: &[&str]) -> RestoreArgs {
            let mut argv = vec!["nikon-fleet", "restore", "--json"];
            argv.extend_from_slice(extra);
            argv.push("snap.json");
            match Cli::try_parse_from(argv).unwrap().cmd {
                Cmd::Restore(a) => a,
                _ => unreachable!(),
            }
        }

        /// What `cmd_restore` does after the SDK: the restore on the camera and how it ends.
        fn run(w: &World, a: &RestoreArgs, only: Option<&std::collections::BTreeSet<String>>) -> (Result<()>, RestoreProduct, bool) {
            let setup = RestoreSetup {
                snap: &w.snap,
                source_name: "snap.json",
                source_bytes: &w.bytes,
                only,
                identity: &w.identity,
                snapshots_dir: w.dir.path(),
                name_lookup: &w.names,
                captured_at: "2026-10-27T17:26:00.688812Z".into(),
            };
            let mut product = RestoreProduct::default();
            let mut in_hand = false;
            let r = restore_on_port(&w.cam, a, &setup, &mut product, &mut in_hand);
            (restore_finish(r, a, in_hand), product, in_hand)
        }

        fn plan_of(w: &World) -> (String, String) {
            let (r, p, _) = run(w, &args(&["--dry-run"]), None);
            r.unwrap();
            let v: serde_json::Value = serde_json::from_str(p.json().expect("a plan")).unwrap();
            (v["digest"].as_str().unwrap().to_string(), v["summary"].as_str().unwrap().to_string())
        }

        #[test]
        fn a_dry_run_prints_the_plan_as_json_and_writes_nothing() {
            let w = world(None);
            let (r, p, in_hand) = run(&w, &args(&["--dry-run"]), None);
            r.unwrap();
            assert!(!in_hand);
            let v: serde_json::Value = serde_json::from_str(p.json().unwrap()).unwrap();
            assert!(v["digest"].as_str().is_some_and(|d| d.len() == 64), "{v}");
            assert!(w.cam.writes().is_empty());
            // without --json no document is printed
            let mut plain = args(&["--dry-run"]);
            plain.json = false;
            assert!(run(&w, &plain, None).1.json().is_none());
        }

        #[test]
        fn an_apply_is_held_to_the_plan_it_was_given_and_a_report_in_hand_is_recorded_when_it_ran() {
            let w = world(None);
            let (digest, summary) = plan_of(&w);
            let held = args(&["--expect-digest", &digest, "--expect-summary", &summary]);
            let (r, p, in_hand) = run(&w, &held, None);
            r.unwrap();
            assert!(in_hand, "apply_restore returned a report: from here a failure may have followed a write");
            assert!(!w.cam.writes().is_empty());
            let report: serde_json::Value = serde_json::from_str(p.json().unwrap()).unwrap();
            assert_eq!(report["plan_digest"], digest, "the report names the plan it was held to");
            // with no --expect-* the command applies the plan it has just made itself
            let w2 = world(None);
            let (r, _, in_hand) = run(&w2, &args(&[]), None);
            r.unwrap();
            assert!(in_hand);
        }

        #[test]
        fn a_plan_that_no_longer_matches_writes_nothing_and_ends_with_the_stale_status() {
            for (what, digest, summary) in [("digest", Some("0".repeat(64)), None), ("summary", None, Some("another summary".to_string()))] {
                let w = world(None);
                let (good_digest, good_summary) = plan_of(&w);
                let a = args(&[
                    "--expect-digest",
                    &digest.unwrap_or(good_digest),
                    "--expect-summary",
                    &summary.unwrap_or(good_summary),
                ]);
                let (r, p, in_hand) = run(&w, &a, None);
                let e = r.unwrap_err();
                assert_eq!(exit_code_for(&e), EXIT_STALE_PLAN, "{what}: {e:?}");
                assert!(!in_hand && w.cam.writes().is_empty() && p.json().is_none(), "{what}");
            }
        }

        #[test]
        fn a_refusal_before_the_writes_ends_with_the_refused_status_and_a_failure_after_them_does_not() {
            // another model: refused while planning, before anything is read for writing
            let mut w = world(None);
            w.identity.model = "Z6_3".into();
            let (r, _, in_hand) = run(&w, &args(&[]), None);
            assert_eq!(exit_code_for(&r.unwrap_err()), EXIT_REFUSED_BEFORE_WRITE);
            assert!(!in_hand && w.cam.writes().is_empty());
            // the advice to run another command is for a person at a terminal
            let msg = |json: bool| {
                let mut a = args(&["--dry-run"]);
                a.json = json;
                run(&w, &a, None).0.unwrap_err().to_string()
            };
            assert!(msg(false).contains("nikon-fleet transplant") && !msg(true).contains("nikon-fleet transplant"), "{} / {}", msg(false), msg(true));
            // a write that failed: a report is in hand, the camera may have changed, plain failure
            let w = world(Some(1));
            let (r, p, in_hand) = run(&w, &args(&[]), None);
            let e = r.unwrap_err();
            assert!(in_hand, "the report came after the writes began");
            assert_eq!(exit_code_for(&e), 1, "{e:?}");
            assert!(p.json().is_some(), "the report is printed before the command fails");
            // an apply that did not even get as far as a stale check: no baseline for the model
        }

        #[test]
        fn a_dry_run_that_names_what_cannot_be_restored_prints_its_plan_and_then_fails() {
            let mut w = world(None);
            // a name in the snapshot the camera does not have: planned as absent_on_target, named by --only
            w.snap.insert("kNkMAIDCapability_NotOnThisBody", 999_999, serde_json::json!(1));
            w.bytes = w.snap.to_pretty_json().unwrap().into_bytes();
            let only = std::collections::BTreeSet::from(["kNkMAIDCapability_NotOnThisBody".to_string(), w.targets[0].0.clone()]);
            let (r, p, in_hand) = run(&w, &args(&["--dry-run", "--only", "NotOnThisBody", "--only", &w.targets[0].0]), Some(&only));
            let e = r.unwrap_err();
            assert_eq!(exit_code_for(&e), 1, "a dry run keeps the plain failure: {e:?}");
            assert!(!in_hand);
            let plan: serde_json::Value = serde_json::from_str(p.json().expect("the plan is printed before the failure")).unwrap();
            assert_eq!(plan["will_not_write"][0]["name"], "kNkMAIDCapability_NotOnThisBody", "{plan}");
            assert!(w.cam.writes().is_empty());
        }

        #[test]
        fn the_status_a_run_ends_with_is_chosen_by_whether_a_report_was_in_hand() {
            let a = args(&[]);
            let boom = || anyhow::anyhow!("boom");
            assert_eq!(exit_code_for(&restore_finish(Err(boom()), &a, false).unwrap_err()), EXIT_REFUSED_BEFORE_WRITE);
            assert_eq!(exit_code_for(&restore_finish(Err(boom()), &a, true).unwrap_err()), 1);
            let dry = args(&["--dry-run"]);
            assert_eq!(exit_code_for(&restore_finish(Err(boom()), &dry, false).unwrap_err()), 1);
            assert!(restore_finish(Ok(()), &a, false).is_ok());
        }

        // ── a restore that cannot be confirmed by its read-back (F25) ───────────

        /// A Z6III holding 17 shots per burst, and a snapshot of it that stores `stored_index`.
        fn burst_restore(stored_index: u64, model: &str, a: &RestoreArgs) -> RestoreProduct {
            let range = |i: u64| serde_json::json!({"value": 0.0, "default": 0.0, "value_index": i, "default_index": 200, "lower": 0.0, "upper": 200.0, "steps": 201});
            let identity = Camera { model: model.into(), serial: "T1".into(), firmware: "2.00".into() };
            let mut snap = Snapshot::new(identity.clone(), Transport::Usb, "2026-10-28T10:00:00Z".into());
            snap.insert("kNkMAIDCapability_ShootingLimit", 33086, range(stored_index));
            let bytes = snap.to_pretty_json().unwrap().into_bytes();
            let cam = FakeCamera::new().with(33086, RW, range(17)).with_sdk_range_quirk();
            let names = HashMap::from([(33086, "kNkMAIDCapability_ShootingLimit".to_string())]);
            let dir = tempfile::tempdir().unwrap();
            let setup = RestoreSetup {
                snap: &snap,
                source_name: "z6iii.json",
                source_bytes: &bytes,
                only: None,
                identity: &identity,
                snapshots_dir: dir.path(),
                name_lookup: &names,
                captured_at: "2026-10-28T17:26:00Z".into(),
            };
            let mut product = RestoreProduct::default();
            let mut in_hand = false;
            let _ = restore_on_port(&cam, a, &setup, &mut product, &mut in_hand);
            product
        }

        fn caveat_lines(p: &RestoreProduct) -> Vec<&RestoreOut> {
            p.out.iter().filter(|o| matches!(o, RestoreOut::Say(t) | RestoreOut::Warn(t) if t.contains("cannot confirm"))).collect()
        }

        #[test]
        fn a_restore_that_would_write_the_ambiguous_burst_value_says_so_in_the_plan_and_before_the_write() {
            // Dry run: the caveat is in the printed plan (text) and in the JSON document.
            let dry = burst_restore(1, "Z6_3", &args(&["--dry-run"]));
            let lines = caveat_lines(&dry);
            assert_eq!(lines.len(), 1, "{:?}", dry.out);
            assert!(matches!(lines[0], RestoreOut::Say(t) if t.contains("kNkMAIDCapability_ShootingLimit")), "{:?}", lines[0]);
            let plan: serde_json::Value = serde_json::from_str(dry.json().unwrap()).unwrap();
            assert_eq!(plan["caveats"][0]["kind"], "readback_cannot_confirm", "{plan}");
            // A real restore: a warning on stderr, pushed before the apply runs (whatever the apply then does).
            let real = burst_restore(1, "Z6_3", &args(&["--allow-partial"]));
            assert!(matches!(caveat_lines(&real).as_slice(), [RestoreOut::Warn(_)]), "{:?}", real.out);
        }

        #[test]
        fn neighbouring_values_and_other_models_print_no_caveat() {
            for (index, model) in [(0, "Z6_3"), (2, "Z6_3"), (1, "Z 9"), (1, "Z 5")] {
                let dry = burst_restore(index, model, &args(&["--dry-run"]));
                assert!(caveat_lines(&dry).is_empty(), "{model} index {index}: {:?}", dry.out);
                let plan: serde_json::Value = serde_json::from_str(dry.json().unwrap()).unwrap();
                assert_eq!(plan["caveats"], serde_json::json!([]), "{model} index {index}");
            }
        }

        // ── which body a restore writes to (R17) ─────────────────────────────────

        fn z9(id: u32, usb_serial: Option<&str>) -> (DeviceInfo, Option<UsbCameraInfo>) {
            (dev(id), usb_serial.map(usb))
        }

        fn snapshot_of_serial(serial: &str, model: &str) -> Snapshot {
            Snapshot::new(Camera { model: model.into(), serial: serial.into(), firmware: "5.31".into() }, Transport::Usb, "2026-10-27T10:00:00Z".into())
        }

        #[test]
        fn a_snapshot_stamped_with_an_sdk_id_is_not_restored_onto_one_of_two_same_model_bodies() {
            let two = || vec![z9(1, None), z9(2, None)];
            for serial in ["id-1", "id-2", "1", "2"] {
                let err = restore_target(two(), None, &snapshot_of_serial(serial, "Z 9")).unwrap_err();
                let typed = err.to_string();
                assert!(typed.contains("attach only one") && typed.contains("--serial"), "{serial}: {typed}");
            }
            // Exactly the typed refusal, not NotFound or a pick.
            assert_eq!(
                select_for_snapshot_serial(two(), "id-1", "Z 9").unwrap_err(),
                TargetError::SnapshotSerialAmbiguous { requested: "id-1".into(), model: "Z 9".into(), count: 2 }
            );
        }

        #[test]
        fn with_one_body_of_the_model_a_snapshot_sdk_id_still_picks_it_and_a_second_model_does_not_count() {
            let one = restore_target(vec![z9(1, None)], None, &snapshot_of_serial("id-1", "Z 9")).unwrap();
            assert_eq!(one.0.id, 1);
            let other_model = (DeviceInfo { name: "Z6_3".into(), ..dev(2) }, None);
            let mixed = restore_target(vec![z9(1, None), other_model], None, &snapshot_of_serial("id-1", "Z 9")).unwrap();
            assert_eq!(mixed.0.id, 1);
        }

        #[test]
        fn a_real_usb_serial_in_the_snapshot_and_an_explicit_serial_are_not_second_guessed() {
            let two = || vec![z9(1, Some("AAA")), z9(2, Some("BBB"))];
            assert_eq!(restore_target(two(), None, &snapshot_of_serial("BBB", "Z 9")).unwrap().0.id, 2);
            // The person named the body with --serial, even as an id-N: their say, as before.
            let serialless = || vec![z9(1, None), z9(2, None)];
            assert_eq!(restore_target(serialless(), Some("id-2"), &snapshot_of_serial("id-1", "Z 9")).unwrap().0.id, 2);
            // And a --serial that finds nobody keeps the old message naming the snapshot.
            let missing = restore_target(two(), Some("NOPE"), &snapshot_of_serial("AAA", "Z 9")).unwrap_err().to_string();
            assert!(missing.contains("no connected camera with serial NOPE") && missing.contains("snapshot is for Z 9 AAA"), "{missing}");
        }

        #[test]
        fn the_ambiguous_target_refusal_ends_a_real_restore_with_the_refused_before_write_status() {
            let err = restore_target(vec![z9(1, None), z9(2, None)], None, &snapshot_of_serial("id-1", "Z 9")).unwrap_err();
            // Same path every other refusal before a report takes: nothing was written, and says so.
            let real = restore_finish(Err(err), &args(&[]), false).unwrap_err();
            assert_eq!(exit_code_for(&real), EXIT_REFUSED_BEFORE_WRITE, "{real:?}");
            let err = restore_target(vec![z9(1, None), z9(2, None)], None, &snapshot_of_serial("id-1", "Z 9")).unwrap_err();
            let dry = restore_finish(Err(err), &args(&["--dry-run"]), false).unwrap_err();
            assert_eq!(exit_code_for(&dry), 1, "a dry run keeps the plain failure");
        }

        #[test]
        fn restore_help_names_every_status_it_ends_with_and_what_exit_zero_covers() {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            let help = cmd.find_subcommand_mut("restore").unwrap().render_long_help().to_string();
            for code in [0, 1, 2, EXIT_STALE_PLAN, EXIT_REFUSED_BEFORE_WRITE] {
                assert!(help.contains(&format!("\n  {code}  ")), "status {code} missing from:\n{help}");
            }
            for status in ["applied_verified", "applied_unverified"] {
                assert!(help.contains(status), "exit 0 covers {status}:\n{help}");
            }
            let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(flat.contains("changed between this run's own plan and its apply"), "{flat}");
            assert!(flat.contains("prints its plan and then exits 1"), "{flat}");
        }
    }

}
