//! `fleet` CLI — settings management for a fleet of Nikon Z cameras.
//!
//! All real logic lives in the `nikon_fleet` library; this file is just
//! the command-line surface.

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
    FirmwareError, FirmwareMeta, FIRMWARE_META_FORMAT_VERSION,
    archive_dir, archive_exists, format_size, ingest_bundle, list_archives, load_meta,
    model_slug, save_meta, sha256_file, IngestOutcome, Provenance, RetrievalMethod,
};
use nikon_fleet::firmware_browse::{
    BrowserWindow, VersionEvidence, browse_firmware, nikon_is_release_page,
};
use nikon_fleet::firmware_source::{
    CameraRelation, DownloadAccess, FirmwareSource, FwVersion, HttpGet, ModelStatus, NikonDownloadCenter,
    Release, UreqHttp, assess, camera_relation, fetch_direct,
};
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::ops::cameras::duplicate_models;
use nikon_fleet::ops::capture::{self as ops_capture, CaptureStats};
use nikon_fleet::ops::firmware_latest::{LatestReport, ModelSurvey, survey_json};
use nikon_fleet::ops::list::{EntryState, Listing, list_dir, reference_filename};
use nikon_fleet::ops::restore;
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
    /// Base directory for snapshots/ and references/ subdirs.
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
    #[command(hide = true)]
    Worker(worker_cmd::WorkerArgs),

    /// Manage the per-model capture baselines that decide whether a capture
    /// is complete (docs/mcp-plan.md §5.5).
    #[command(subcommand)]
    Baseline(BaselineCmd),

    /// Diff two snapshot files.
    Diff(DiffArgs),

    /// Diff a live camera against its reference.
    Check(CheckArgs),

    /// List stored snapshots.
    Ls(LsArgs),

    /// Remove a stored snapshot.
    Rm(RmArgs),

    /// Restore settings from a snapshot to a live camera.
    Restore(RestoreArgs),

    /// Copy settings from a snapshot onto a live camera of a possibly
    /// *different* model, translating enum/range values into the target's
    /// own equivalents instead of assuming the same code/index means the
    /// same thing on both bodies. See src/transplant.rs for the strategy.
    Transplant(TransplantArgs),

    /// Manage the firmware archive.
    #[command(subcommand)]
    Firmware(FirmwareCmd),

    /// Read a vendor-only property (0xD0xx/0x5xxx) over raw USB PTP, bypassing
    /// the MAID SDK. See docs/nx-field-session-2026-07-09.md for background —
    /// these were reverse-engineered from an NX Field WiFi capture and later
    /// confirmed to also work over plain USB.
    VendorRead(VendorReadArgs),

    /// Write an FTP profile (0x90EE) over raw USB PTP, bypassing the MAID
    /// SDK. Reverse-engineered the same way as VendorRead — see
    /// docs/nx-field-session-2026-07-09.md for the field map and the parts
    /// of the wire format that are still opaque-but-fixed.
    VendorWriteFtp(VendorWriteFtpArgs),
}

#[derive(Args, Debug)]
struct DiscoverArgs {
    /// Output camera list as JSON (for GUI / scripting).
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct SnapshotArgs {
    /// Camera serial to target. (Used to disambiguate when multiple bodies are attached.)
    #[arg(long)]
    serial: Option<String>,
    /// Optional label, e.g. "wedding-baseline".
    #[arg(long)]
    label: Option<String>,
    /// Save the capture even if it is missing capabilities the model's
    /// baseline reads (a body in an unusual state, e.g. no card). Without
    /// this an incomplete capture is refused and nothing is written.
    #[arg(long)]
    allow_partial: bool,
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
        /// firmware update).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RefCmd {
    /// Promote a stored snapshot to be the reference for its camera.
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

#[derive(Args, Debug)]
struct RmArgs {
    snapshot_path: PathBuf,
}

#[derive(Subcommand, Debug)]
enum FirmwareCmd {
    /// Archive a firmware binary file.
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
    Fetch(FirmwareFetchArgs),
    /// Open the release's download page in an embedded browser so YOU accept
    /// Nikon's licence and click Download; the file is captured, checked
    /// against the page's version and archived with provenance. Needs a build
    /// with `--features webview`; otherwise prints the manual steps.
    Browse(FirmwareBrowseArgs),
}

#[derive(Args, Debug)]
struct FirmwareBrowseArgs {
    /// Model whose newest firmware to download, e.g. "Z 9" or "Z6_3".
    model: String,
}

/// Models checked when no --model is given: the fleet bodies that have
/// newer firmware to watch (Z 5 is supported by the source but not default).
const DEFAULT_WATCH_MODELS: &[&str] = &["Z 9", "Z6_3"];

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
    /// Optional free-form notes.
    #[arg(long)]
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
    /// on USB; this command does not use --sdk-bundle/the MAID SDK at all,
    /// so it does NOT understand the "id-N" fallback serial `fleet
    /// discover`/`snapshot` print for a camera with no readable USB serial —
    /// use the real serial, or the "usb-<bus>:<addr>" fallback this
    /// command's own error/warning output uses instead for that case.
    #[arg(long)]
    serial: Option<String>,
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
    /// on USB; this command does not use --sdk-bundle/the MAID SDK at all,
    /// so it does NOT understand the "id-N" fallback serial `fleet
    /// discover`/`snapshot` print for a camera with no readable USB serial —
    /// use the real serial, or the "usb-<bus>:<addr>" fallback this
    /// command's own error/warning output uses instead for that case.
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

/// Whether `s` identifies this device, as recorded by `cmd_snapshot`'s
/// serial field: the real USB serial, the bare numeric SDK id, or the
/// `"id-{id}"` fallback form written when no USB serial was readable.
/// Without the last form, a camera snapshotted under that fallback could
/// never be found again by --serial in any later command.
fn serial_matches(dev: &DeviceInfo, usb: &Option<UsbCameraInfo>, s: &str) -> bool {
    usb.as_ref().map(|u| u.serial.as_str()) == Some(s)
        || dev.id.to_string() == s
        || format!("id-{}", dev.id) == s
}

/// The serial to record for a captured snapshot: the real USB serial if one
/// was read (and non-empty — the SDK/USB layer can return `Some("")` for an
/// unreadable descriptor, which must NOT be treated as a valid serial), else
/// the `"id-{id}"` fallback.
fn snapshot_serial(usb_info: &Option<UsbCameraInfo>, dev_id: u32) -> String {
    usb_info
        .as_ref()
        .and_then(|u| (!u.serial.is_empty()).then(|| u.serial.clone()))
        .unwrap_or_else(|| format!("id-{dev_id}"))
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

fn cmd_ref(data_dir: &Path, sub: &RefCmd) -> Result<()> {
    let refs = references_dir(data_dir);
    match sub {
        RefCmd::Set { snapshot_path } => {
            let snap = Snapshot::load_from_file(snapshot_path)
                .with_context(|| format!("loading {}", snapshot_path.display()))?;
            ensure_dir(&refs)?;
            let target = refs.join(reference_filename(&snap.camera.model, &snap.camera.serial));
            fs::copy(snapshot_path, &target)
                .with_context(|| format!("copying to {}", target.display()))?;
            println!(
                "Set reference for {} {} → {}",
                snap.camera.model, snap.camera.serial, target.display()
            );
        }
        RefCmd::Show { model, serial } => {
            let target = refs.join(reference_filename(model, serial));
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
    let sdk = open_sdk(bundle_path, no_usb_reset)?;
    let enriched = enrich_devices(sdk.devices()?);
    if enriched.is_empty() {
        if args.json {
            // Built via serde like the populated-list branch below, rather
            // than a hand-typed literal — the two used to be two different
            // code paths producing the same logical shape with no compiler
            // tie between them if the schema ever gains a field.
            println!("{}", serde_json::json!({ "cameras": Vec::<serde_json::Value>::new() }));
        } else {
            println!("(no Nikon cameras detected)");
        }
        return Ok(());
    }
    if args.json {
        let cameras: Vec<serde_json::Value> = enriched.iter().map(|(dev, usb)| {
            let id_str   = dev.id.to_string();
            let serial   = usb.as_ref().and_then(|u| (!u.serial.is_empty()).then_some(u.serial.as_str())).unwrap_or(id_str.as_str()).to_string();
            let firmware = usb.as_ref().map(|u| u.firmware.as_str()).unwrap_or(dev.version.as_str()).to_string();
            serde_json::json!({ "model": dev.name, "serial": serial, "firmware": firmware })
        }).collect();
        println!("{}", serde_json::json!({ "cameras": cameras }));
    } else {
        println!("{} device(s):", enriched.len());
        for (dev, usb) in &enriched {
            // Same empty-but-present-serial fallback as the --json branch
            // above — previously this branch only fell back to "?" when
            // there was no USB pairing at all, so an empty (not absent)
            // serial printed as a blank field instead of falling back.
            let id_str   = dev.id.to_string();
            let serial   = usb.as_ref().and_then(|u| (!u.serial.is_empty()).then_some(u.serial.as_str())).unwrap_or(id_str.as_str());
            let firmware = usb.as_ref().map(|u| u.firmware.as_str()).unwrap_or(dev.version.as_str());
            println!("  {}  serial={}  fw={}  id={}", dev.name, serial, firmware, dev.id);
        }
    }
    Ok(())
}

/// Select the target device: by --serial if given (matched against USB
/// iSerialNumber, bare SDK id, or the "id-{id}" fallback form — see
/// `serial_matches`), else the first available one. Shared by cmd_snapshot
/// and cmd_check, which used to each carry an identical copy of this match.
fn select_device(
    enriched: Vec<(DeviceInfo, Option<UsbCameraInfo>)>,
    serial: Option<&str>,
) -> Result<(DeviceInfo, Option<UsbCameraInfo>)> {
    match serial {
        Some(s) => enriched.into_iter()
            .find(|(dev, usb)| serial_matches(dev, usb, s))
            .ok_or_else(|| anyhow::anyhow!("no device with serial {s}")),
        None => {
            let idx = enriched.iter().position(|(d, _)| d.available).unwrap_or(0);
            Ok(enriched.into_iter().nth(idx).unwrap())
        }
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
    if unknown_schema > 0 {
        eprintln!(
            "warning: {unknown_schema} capabilit{plural} not found in the loaded schema \
             ({schema_path}) — recorded by numeric code (cap_0x...) instead of a symbolic \
             name. The schema may be stale relative to this camera's firmware.",
            plural = if unknown_schema == 1 { "y is" } else { "ies are" },
            schema_path = schema_path.display(),
        );
    }
}

/// Judge a freshly captured snapshot against its model's baseline, in memory,
/// before anything is written. An incomplete capture is an error unless
/// `allow_partial`; a capture the gate cannot judge is noted on stderr and
/// allowed (a new model has to be capturable to be baselined).
fn apply_capture_gate(snap: &Snapshot, allow_partial: bool) -> Result<()> {
    use nikon_fleet::ops::baseline::{GateOutcome, NoBaselineReason, describe_failure, gate};
    match gate(snap) {
        GateOutcome::Passed => Ok(()),
        GateOutcome::Failed { missing } => {
            let text = describe_failure(snap, &missing);
            if allow_partial {
                eprintln!("warning: {text} Continuing because --allow-partial was given.");
                Ok(())
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
            Ok(())
        }
        GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline, capture }) => {
            eprintln!(
                "note: firmware {capture} differs from the {:?} baseline's {baseline} and the readable set differs \
                 too; this capture was not checked for completeness (re-seed the baseline for the new firmware).",
                snap.camera.model
            );
            Ok(())
        }
    }
}

/// Write the baseline seeded from `snapshot_path` to `out_dir/{model_slug}.json`.
/// Refuses to replace an existing file unless `force`.
fn seed_baseline(snapshot_path: &Path, out_dir: &Path, force: bool) -> Result<PathBuf> {
    use nikon_fleet::ops::baseline::Baseline;
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
        fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    } else {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("{} (already exists? pass --force to replace it)", path.display()))?;
        f.write_all(json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

fn cmd_baseline(sub: &BaselineCmd) -> Result<()> {
    match sub {
        BaselineCmd::Seed { snapshot_path, out_dir, force } => {
            let path = seed_baseline(snapshot_path, out_dir, *force)?;
            println!("Wrote {}\nBaselines are embedded at build time: rebuild to use it.", path.display());
            Ok(())
        }
    }
}

fn cmd_snapshot(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &SnapshotArgs, no_usb_reset: bool) -> Result<()> {
    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    let (target, usb_info) = select_device(enriched, args.serial.as_deref())?;
    println!("Connecting to {:?} (id={})…", target.name, target.id);

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
    println!("Reading {} capabilities…", device.capabilities.len());

    let captured_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let serial = snapshot_serial(&usb_info, target.id);
    let firmware = usb_info.as_ref().map(|u| u.firmware.clone())
        .unwrap_or_else(|| target.version.clone());
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
    println!(
        "  ok={}  err={}  skipped(no-get-bit)={}  unknown-schema={}",
        stats.read_ok, stats.read_err, stats.skipped_no_get, stats.unknown_schema
    );
    warn_if_unknown_schema(stats.unknown_schema, schema_path);

    // Judge completeness in memory, before anything is written.
    apply_capture_gate(&snap, args.allow_partial)?;

    // Save under snapshots/.
    let dir = snapshots_dir(data_dir);
    ensure_dir(&dir)?;
    let filename = snap.suggested_filename();
    let path = dir.join(&filename);
    // create-new: a capture must never replace an existing snapshot.
    snap.save_new(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());
    if snapshot_capture_failed(stats.read_ok) {
        // The file is still written (matches prior behavior), but a
        // script/CI checking exit status must be able to tell this run was
        // a total failure, not a normal capture.
        bail!("no capability could be read ({} read error(s), see above); wrote an empty snapshot to {}", stats.read_err, path.display());
    }
    Ok(())
}

/// Read one vendor-only property via raw USB PTP (`0x943B`), bypassing the
/// MAID SDK entirely — no --sdk-bundle needed. See `nikon_fleet::ptp_usb`.
fn cmd_vendor_read(args: &VendorReadArgs) -> Result<()> {
    let data = nikon_fleet::ptp_usb::read_vendor_property(args.serial.as_deref(), args.propcode)
        .with_context(|| format!("reading vendor property 0x{:04x} over USB PTP", args.propcode))?;
    println!("0x{:04x}: {} byte(s): {}", args.propcode, data.len(), nikon_fleet::ptp_usb::hex_bytes(&data));
    Ok(())
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

    nikon_fleet::ptp_usb::write_ftp_profile(args.serial.as_deref(), &profile, args.force_unconfirmed_model)
        .context("writing FTP profile over USB PTP")?;
    println!("Wrote FTP profile {:?} ({}@{}:{})", args.profile_name, args.username, args.host, args.port);
    Ok(())
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

/// Whether a restore should be treated as a hard failure: zero writes
/// succeeded and at least one real error occurred — not a routine skip
/// (absent/read-only/unsupported-type capabilities are expected and don't
/// count as errors).
fn restore_completely_failed(written: usize, errors: usize) -> bool {
    written == 0 && errors > 0
}

/// The error for a targeted restore that did not write everything it was
/// asked to: `named` is how many properties `--only` selected (`None` for a
/// full restore, which skips many properties routinely and never fails for
/// that), `unrestored` each named property that was not written (or, in a dry
/// run, would not be) with a closed reason tag. `None` when everything named
/// was (or would be) written.
fn unrestored_error(named: Option<usize>, unrestored: &[(String, &'static str)]) -> Option<String> {
    let named = named?;
    if unrestored.is_empty() {
        return None;
    }
    let list: Vec<String> = unrestored.iter().map(|(n, why)| format!("{n} ({why})")).collect();
    Some(format!(
        "targeted restore incomplete: {} of {named} named propert{} not written: {}",
        unrestored.len(),
        if unrestored.len() == 1 { "y was" } else { "ies were" },
        list.join(", ")
    ))
}

fn cmd_check(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &CheckArgs, no_usb_reset: bool) -> Result<()> {
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
    let serial = snapshot_serial(&usb_info, target.id);
    let firmware = usb_info.as_ref().map(|u| u.firmware.clone())
        .unwrap_or_else(|| target.version.clone());

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
    apply_capture_gate(&live, args.allow_partial)?;

    // Reference for this body.
    let ref_path = references_dir(data_dir).join(reference_filename(&target.name, &serial));
    if !ref_path.exists() {
        bail!(
            "no reference for {} {} at {}.\n\
             Hint: `fleet snapshot` first, then `fleet ref set <snapshot>`.",
            target.name, serial, ref_path.display()
        );
    }
    let reference = Snapshot::load_from_file(&ref_path)?;

    let d = diff_with_schema(&reference, &live, &DiffOptions::default(), &schema)?;
    print_diff_human(&reference, &live, &d);
    Ok(())
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

/// Whether `s` is safe to use as one component of a path this process will
/// create/write to (as `model_slug(model)` and `version` both are, via
/// `archive_dir`). Rejects anything that isn't a single plain path segment
/// — a `--version` (or `--model`) of `"../../../../tmp/pwned"` would
/// otherwise walk `archive_dir`'s `.join(...)` calls straight out of
/// data-dir before `fs::create_dir_all`/`fs::copy`/`fs::write` ever run.
fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\\')
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
    let entries = list_archives(data_dir, args.model.as_deref());
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
            let ref_path = refs_dir.join(reference_filename(&cam.model, &cam.serial));
            if ref_path.exists() {
                Snapshot::load_from_file(&ref_path)
                    .ok()
                    .map(|s| s.camera.firmware)
                    .unwrap_or_default()
            } else {
                String::new()
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


fn watch_models(requested: &[String]) -> Vec<String> {
    if requested.is_empty() {
        DEFAULT_WATCH_MODELS.iter().map(|m| m.to_string()).collect()
    } else {
        requested.to_vec()
    }
}

fn survey_models(
    data_dir: &Path,
    models: &[String],
    source: &dyn FirmwareSource,
    http: &dyn HttpGet,
) -> Vec<ModelSurvey> {
    let cameras = usb_camera_list();
    models.iter().map(|model| {
        let release = source.latest(model, http);
        let mut archived = Vec::new();
        let mut archive_notes = Vec::new();
        for e in list_archives(data_dir, Some(model)) {
            match FwVersion::parse_canonical(&e.version) {
                Ok(v) => archived.push(v),
                Err(err) => archive_notes.push(format!("archive entry {:?} ignored: {err}", e.version)),
            }
        }
        let status = assess(&release, &archived);
        let cams = cameras.iter().filter(|c| &c.model == model)
            .map(|c| (c.serial.clone(), c.firmware.clone())).collect();
        ModelSurvey { model: model.clone(), release, archived, archive_notes, cameras: cams, status }
    }).collect()
}

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
            eprintln!("error: {}: {e:#}", s.model);
        }
    }
    if manual_pending > 0 {
        println!("\n{manual_pending} model(s) need a manual, licence-gated download (see ACTION REQUIRED above).");
    }
    if failures > 0 {
        bail!("{failures} of {} model(s) failed (see above)", surveys.len());
    }
    Ok(())
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

fn cmd_restore(data_dir: &Path, bundle: &Path, args: &RestoreArgs, no_usb_reset: bool) -> Result<()> {
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let snap = Snapshot::load_from_file(&snap_path)
        .with_context(|| format!("loading {}", snap_path.display()))?;

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
    let (target, _usb) = enriched
        .into_iter()
        .find(|(dev, usb)| serial_matches(dev, usb, target_serial))
        .ok_or_else(|| anyhow::anyhow!(
            "no connected camera with serial {}  (snapshot is for {} {})",
            target_serial, snap.camera.model, snap.camera.serial
        ))?;

    if target.name != snap.camera.model {
        eprintln!(
            "warning: snapshot is for model {:?} but connected camera is {:?}",
            snap.camera.model, target.name
        );
    }

    println!("Connecting to {:?} (id={})…", target.name, target.id);
    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;

    let run = restore::execute(&device, &snap.properties, only.as_ref(), args.dry_run);
    for step in &run.steps {
        match &step.outcome {
            restore::StepOutcome::WouldWrite => println!("  would write  {} [{:#x}]", step.name, step.code),
            restore::StepOutcome::WriteFailed(msg) => eprintln!("  warn: {} [{:#x}]: {}", step.name, step.code, msg),
            _ => {}
        }
    }
    let restore::Counts { written, skipped_absent, skipped_read_only, skipped_type, skipped_never, not_selected, errors } =
        run.counts();
    let unrestored = run.unrestored();

    let not_selected_note = if only.is_some() { format!("  skipped(not-selected)={not_selected}") } else { String::new() };
    println!(
        "{} written={written}  skipped(absent)={skipped_absent}  \
         skipped(read-only)={skipped_read_only}  \
         skipped(unsupported-type)={skipped_type}  skipped(never-copied)={skipped_never}{not_selected_note}  errors={errors}",
        if args.dry_run { "Dry run:" } else { "Restore complete:" }
    );
    if !args.dry_run && restore_completely_failed(written, errors) {
        bail!("all {errors} capability write(s) failed (see warnings above); nothing was restored");
    }
    if let Some(msg) = unrestored_error(only.as_ref().map(|o| o.len()), &unrestored) {
        bail!("{msg}");
    }
    Ok(())
}

/// Copy a snapshot's settings onto a live camera that may be a different
/// model, translating enum/range values via `nikon_fleet::transplant`
/// instead of `restore`'s literal same-model copy. See that module's docs
/// for the translation strategy and why capability *codes* (not menu
/// position, not per-model renumbering) are the right join key here.
fn cmd_transplant(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &TransplantArgs, no_usb_reset: bool) -> Result<()> {
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let snap = Snapshot::load_from_file(&snap_path)
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
    let (target, _usb) = select_device(enriched, args.serial.as_deref())?;

    // Before connecting: refuse non-Z-series models and build the per-code
    // name check. Needs the schema (MaidLayer.config) — unlike restore,
    // transplant can't run without it.
    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let guard = transplant::CodeNameGuard::new(&schema, &snap.camera.model, &target.name)?;

    if target.name == snap.camera.model {
        println!(
            "note: source and target are both {:?} — transplant works the same either way, \
             but `fleet restore` is the simpler/faster tool for a same-model copy.",
            target.name
        );
    }

    println!("Connecting to {:?} (id={})…", target.name, target.id);
    let device = sdk.connect(target.id)?;
    ensure_capabilities_present(device.capabilities.len())?;

    let outcomes = transplant::transplant(&device, &snap.properties, &guard, &options, args.dry_run);

    let mut written_literal = 0usize;
    let mut written_enum_label = 0usize;
    let mut written_enum_raw = 0usize;
    let mut written_range = 0usize;
    let mut skipped_absent = 0usize;
    let mut skipped_read_only = 0usize;
    let mut skipped_unsupported_type = 0usize;
    let mut skipped_no_match = 0usize;
    let mut skipped_type_mismatch = 0usize;
    let mut skipped_sdk_decode_failure = 0usize;
    let mut skipped_name_mismatch = 0usize;
    let mut skipped_ambiguous = 0usize;
    let mut skipped_never = 0usize;
    let mut skipped_raw_unconfirmed = 0usize;
    let mut raw_unconfirmed: Vec<(String, String)> = Vec::new();
    let mut skipped_literal_unconfirmed = 0usize;
    let mut literal_unconfirmed: Vec<(String, String)> = Vec::new();
    let mut skipped_other = 0usize;
    let mut errors = 0usize;

    for outcome in &outcomes {
        match &outcome.result {
            Ok(strategy) => {
                if args.dry_run {
                    match &outcome.choice {
                        Some(c) => println!("  would write  {} [{:#x}]  via {strategy}: {c}", outcome.name, outcome.code),
                        None => println!("  would write  {} [{:#x}]  via {strategy}", outcome.name, outcome.code),
                    }
                }
                match strategy {
                    transplant::Strategy::Literal => written_literal += 1,
                    transplant::Strategy::EnumByLabel => written_enum_label += 1,
                    transplant::Strategy::EnumByRawValue => written_enum_raw += 1,
                    transplant::Strategy::RangeClamped => written_range += 1,
                }
            }
            Err(reason) => {
                // The match below decides which counter bucket this outcome
                // belongs in; whether it is warn-worthy comes from
                // `SkipReason::severity`, and the actual eprintln! lives once,
                // after the match.
                match reason {
                    transplant::SkipReason::AbsentOnTarget => {
                        skipped_absent += 1;
                    }
                    transplant::SkipReason::ReadOnly => {
                        skipped_read_only += 1;
                    }
                    transplant::SkipReason::UnsupportedWriteType => {
                        skipped_unsupported_type += 1;
                    }
                    transplant::SkipReason::NoMatchingOption => {
                        skipped_no_match += 1;
                    }
                    transplant::SkipReason::WriteFailed(_) => {
                        errors += 1;
                    }
                    transplant::SkipReason::EnumTypeMismatch { .. } => {
                        skipped_type_mismatch += 1;
                    }
                    transplant::SkipReason::EnumUndecodedBySdk { .. } => {
                        skipped_sdk_decode_failure += 1;
                    }
                    transplant::SkipReason::CapabilityNameMismatch { .. } => {
                        skipped_name_mismatch += 1;
                    }
                    transplant::SkipReason::AmbiguousLabel { .. } => {
                        skipped_ambiguous += 1;
                    }
                    // Policy skips, not problems: routine and numerous, so
                    // counted but not warned about one by one (the per-property
                    // lines are in the summary hint below).
                    transplant::SkipReason::NeverCopied { .. } => {
                        skipped_never += 1;
                    }
                    transplant::SkipReason::RawCodeUnconfirmed { proposed } => {
                        skipped_raw_unconfirmed += 1;
                        raw_unconfirmed.push((outcome.name.clone(), proposed.clone()));
                    }
                    transplant::SkipReason::LiteralUnconfirmed { proposed } => {
                        skipped_literal_unconfirmed += 1;
                        literal_unconfirmed.push((outcome.name.clone(), proposed.clone()));
                    }
                    transplant::SkipReason::UnsupportedValueShape
                    | transplant::SkipReason::TargetReadFailed(_)
                    | transplant::SkipReason::MalformedSource => {
                        skipped_other += 1;
                    }
                }
                // Whether to warn is the reason's own severity, not a second table here.
                let warn = matches!(reason.severity(), transplant::Severity::Warn | transplant::Severity::Error);
                if warn {
                    eprintln!("  warn: {} [{:#x}]: {reason}", outcome.name, outcome.code);
                }
            }
        }
    }

    let written = written_literal + written_enum_label + written_enum_raw + written_range;
    println!(
        "{} written={written} (literal={written_literal} enum-by-label={written_enum_label} \
         enum-by-raw-value={written_enum_raw} range-clamped={written_range})\n  \
         skipped(absent)={skipped_absent} skipped(read-only)={skipped_read_only} \
         skipped(unsupported-type)={skipped_unsupported_type} skipped(no-matching-option)={skipped_no_match} \
         skipped(type-mismatch)={skipped_type_mismatch} skipped(sdk-decode-failure)={skipped_sdk_decode_failure} \
         skipped(name-mismatch)={skipped_name_mismatch} skipped(ambiguous-label)={skipped_ambiguous} \
         skipped(never-copied)={skipped_never} skipped(raw-code-unconfirmed)={skipped_raw_unconfirmed} \
         skipped(literal-unconfirmed)={skipped_literal_unconfirmed} \
         skipped(other)={skipped_other}  errors={errors}",
        if args.dry_run { "Dry run:" } else { "Transplant complete:" }
    );
    if !raw_unconfirmed.is_empty() {
        println!(
            "\n{} propert{} matched only by raw numeric code and would change the target; not written \
             (nothing guarantees both bodies number the same option the same way). Review, then opt in \
             per property with --allow-raw-code <name>:",
            raw_unconfirmed.len(),
            if raw_unconfirmed.len() == 1 { "y was" } else { "ies were" }
        );
        for (name, proposed) in &raw_unconfirmed {
            println!("  {name}: would set {proposed}");
        }
    }
    if !literal_unconfirmed.is_empty() {
        println!(
            "\n{} numeric propert{} copied as-is would change the target; not written \
             (an integer is no more self-describing than a raw code, so nothing says both bodies \
             give it the same meaning). Review, then opt in per property with --allow-raw-code <name>:",
            literal_unconfirmed.len(),
            if literal_unconfirmed.len() == 1 { "y" } else { "ies" }
        );
        for (name, proposed) in &literal_unconfirmed {
            println!("  {name}: would set {proposed}");
        }
    }
    if !args.dry_run && restore_completely_failed(written, errors) {
        bail!("all {errors} capability write(s) failed (see warnings above); nothing was transplanted");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    let no_reset = cli.no_usb_reset;
    match &cli.cmd {
        Cmd::Discover(a) => cmd_discover(&cli.sdk_bundle, a, no_reset),
        Cmd::Snapshot(args) => cmd_snapshot(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Check(args) => cmd_check(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::Diff(args) => cmd_diff(&cli.data_dir, &cli.schema, args),
        Cmd::Ls(args) => cmd_ls(&cli.data_dir, args),
        Cmd::Rm(args) => cmd_rm(&cli.data_dir, args),
        Cmd::Ref(sub) => cmd_ref(&cli.data_dir, sub),
        Cmd::Baseline(sub) => cmd_baseline(sub),
        Cmd::Worker(args) => {
            let code = worker_cmd::run(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args);
            std::process::exit(code)
        }
        Cmd::Restore(args) => cmd_restore(&cli.data_dir, &cli.sdk_bundle, args, no_reset),
        Cmd::Transplant(args) => cmd_transplant(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args, no_reset),
        Cmd::VendorRead(args) => cmd_vendor_read(args),
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

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use serde_json::json;

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

    // ── serial_matches / snapshot_serial ───────────────────────────────────

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

    #[test]
    fn serial_matches_real_usb_serial() {
        assert!(serial_matches(&dev(7), &Some(usb("ABC123")), "ABC123"));
    }

    #[test]
    fn serial_matches_bare_sdk_id() {
        assert!(serial_matches(&dev(7), &None, "7"));
    }

    #[test]
    fn serial_matches_id_fallback_form() {
        // Regression: a snapshot recorded under the "id-{id}" fallback
        // (written when USB serial was unreadable) must be re-findable by
        // that exact string — previously only the real serial or bare id
        // were checked, so such a snapshot could never be matched again.
        assert!(serial_matches(&dev(7), &None, "id-7"));
    }

    #[test]
    fn serial_matches_no_match() {
        assert!(!serial_matches(&dev(7), &Some(usb("ABC123")), "OTHER"));
        assert!(!serial_matches(&dev(7), &None, "8"));
    }

    #[test]
    fn snapshot_serial_uses_real_serial_when_present() {
        assert_eq!(snapshot_serial(&Some(usb("ABC123")), 7), "ABC123");
    }

    #[test]
    fn snapshot_serial_falls_back_to_id_when_usb_none() {
        assert_eq!(snapshot_serial(&None, 7), "id-7");
    }

    #[test]
    fn snapshot_serial_falls_back_to_id_when_serial_present_but_empty() {
        // Regression: Some(UsbCameraInfo { serial: "", .. }) (a real,
        // observed case — read_usb_string returns "" for an unreadable
        // descriptor) must NOT be treated as a valid serial.
        assert_eq!(snapshot_serial(&Some(usb("")), 7), "id-7");
    }

    // ── snapshot_capture_failed / restore_completely_failed ─────────────────

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

    #[test]
    fn restore_completely_failed_zero_written_zero_errors_is_not_failure() {
        // e.g. every capability was skipped as absent/read-only — routine,
        // not an error.
        assert!(!restore_completely_failed(0, 0));
    }

    #[test]
    fn restore_completely_failed_zero_written_nonzero_errors_is_failure() {
        assert!(restore_completely_failed(0, 1));
    }

    #[test]
    fn restore_completely_failed_any_written_is_not_failure() {
        assert!(!restore_completely_failed(1, 99));
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
    fn capture_gate_does_not_judge_an_unknown_model_or_a_changed_firmware() {
        let mut unknown = capture_from_builtin("Z 9", 0);
        unknown.camera.model = "Z 5".into();
        unknown.properties.clear();
        assert!(apply_capture_gate(&unknown, false).is_ok(), "no baseline: allowed, with a note");
        let mut newer = capture_from_builtin("Z 9", 5);
        newer.camera.firmware = "9.99".into();
        assert!(apply_capture_gate(&newer, false).is_ok(), "other firmware and a different set: not blamed");
    }

    fn write_seed_source(dir: &Path, model: &str, firmware: &str, codes: &[u32]) -> PathBuf {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: "1".into(), firmware: firmware.into() },
            Transport::Usb,
            "2026-10-27T12:00:00Z".into(),
        );
        for c in codes {
            s.insert(format!("kNkMAIDCapability_C{c}"), *c, serde_json::json!(1));
        }
        let p = dir.join("src_snapshot.json");
        s.save_to_file(&p).unwrap();
        p
    }

    #[test]
    fn seed_baseline_writes_model_slug_json_that_loads_back() {
        let d = TempDir::new().unwrap();
        let src = write_seed_source(d.path(), "Z 9", "5.31", &[3, 1, 2]);
        let out = d.path().join("baselines");
        let path = seed_baseline(&src, &out, false).unwrap();
        assert_eq!(path, out.join("Z_9.json"));
        let b = nikon_fleet::ops::baseline::Baseline::from_json(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((b.model.as_str(), b.firmware.as_str(), b.readable.len()), ("Z 9", "5.31", 3));
        assert_eq!(b.seeded_from, "src_snapshot.json");
    }

    #[test]
    fn seed_baseline_refuses_to_replace_without_force_and_leaves_the_file_alone() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        let first = write_seed_source(d.path(), "Z 9", "5.31", &[1, 2]);
        let path = seed_baseline(&first, &out, false).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let second = write_seed_source(d.path(), "Z 9", "5.40", &[1, 2, 3]);
        let err = seed_baseline(&second, &out, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "{err:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        seed_baseline(&second, &out, true).unwrap();
        let replaced = fs::read_to_string(&path).unwrap();
        assert!(replaced.contains("5.40") && replaced != before);
    }

    #[test]
    fn seed_baseline_refuses_a_useless_or_unsafe_source() {
        let d = TempDir::new().unwrap();
        let out = d.path().join("baselines");
        let empty = write_seed_source(d.path(), "Z 9", "5.31", &[]);
        assert!(seed_baseline(&empty, &out, false).is_err(), "no readable capabilities");
        let no_fw = write_seed_source(d.path(), "Z 9", "", &[1]);
        assert!(seed_baseline(&no_fw, &out, false).is_err(), "no firmware");
        let hostile = write_seed_source(d.path(), "../evil", "1.00", &[1]);
        assert!(seed_baseline(&hostile, &out, false).is_err(), "model that is not a file name");
        assert!(!d.path().join("evil.json").exists() && !out.join("evil.json").exists());
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

    // ── is_safe_path_component ───────────────────────────────────────────

    #[test]
    fn is_safe_path_component_accepts_plain_segment() {
        assert!(is_safe_path_component("5.31"));
        assert!(is_safe_path_component("Z_9"));
    }

    #[test]
    fn is_safe_path_component_rejects_dotdot() {
        assert!(!is_safe_path_component(".."));
    }

    #[test]
    fn is_safe_path_component_rejects_traversal_sequence() {
        assert!(!is_safe_path_component("../../../../tmp/pwned"));
    }

    #[test]
    fn is_safe_path_component_rejects_embedded_separator() {
        assert!(!is_safe_path_component("a/b"));
        assert!(!is_safe_path_component("a\\b"));
    }

    #[test]
    fn is_safe_path_component_rejects_empty_and_dot() {
        assert!(!is_safe_path_component(""));
        assert!(!is_safe_path_component("."));
    }

    #[test]
    fn cmd_firmware_add_path_traversal_version_rejected() {
        let data_dir = TempDir::new().unwrap();
        let bin_path = data_dir.path().join("fw.bin");
        fs::write(&bin_path, b"v1").unwrap();
        let err = cmd_firmware_add(
            data_dir.path(),
            &firmware_add_args(bin_path, "Z 9", "../../../../tmp/pwned", false),
        ).unwrap_err();
        assert!(err.to_string().contains("not a valid single path component"));
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
        assert_eq!(meta.canonical_snapshot_path.as_deref(), Some("snapshots/foo.json"));
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
}
