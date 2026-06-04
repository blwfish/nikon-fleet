//! `fleet` CLI — settings management for a fleet of Nikon Z cameras.
//!
//! All real logic lives in the `nikon_fleet` library; this file is just
//! the command-line surface.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use nikon_fleet::diff::{Diff, DiffOptions, diff, is_volatile};
use nikon_fleet::firmware::{
    FirmwareError, FirmwareMeta, FIRMWARE_META_FORMAT_VERSION,
    archive_dir, archive_exists, format_size, list_archives, load_meta,
    model_slug, save_meta, sha256_file,
};
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::sdk::{CapabilityInfo, DeviceInfo, Sdk, SdkError, UsbCameraInfo, pair_devices, usb_camera_list, OP_GET, OP_SET};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};

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

    /// Manage the firmware archive.
    #[command(subcommand)]
    Firmware(FirmwareCmd),
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
}

#[derive(Subcommand, Debug)]
enum RefCmd {
    /// Promote a stored snapshot to be the reference for its camera.
    Set { snapshot_path: PathBuf },
    /// Show the reference snapshot for a (model, serial).
    Show { model: String, serial: String },
    /// List all references.
    List,
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
}

#[derive(Args, Debug)]
struct LsArgs {
    /// Limit to snapshots for a specific camera, in "{model}_{serial}" form.
    #[arg(long)]
    camera: Option<String>,
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
struct RestoreArgs {
    /// Snapshot to restore from. Resolved relative to snapshots/ if not absolute.
    snapshot_path: PathBuf,
    /// Target camera serial. Defaults to the serial recorded in the snapshot.
    #[arg(long)]
    serial: Option<String>,
    /// Print what would be written without sending anything to the camera.
    #[arg(long)]
    dry_run: bool,
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

/// Filename used for a camera's reference snapshot.
/// Spaces in model names become underscores so the filename is shell-friendly.
fn reference_filename(model: &str, serial: &str) -> String {
    let model = model.replace(' ', "_");
    format!("{model}_{serial}.json")
}

// ─────────────────────────────────────────────────────────────────────────
// USB enrichment
// ─────────────────────────────────────────────────────────────────────────

fn enrich_devices(devices: Vec<DeviceInfo>) -> Vec<(DeviceInfo, Option<UsbCameraInfo>)> {
    pair_devices(devices, &usb_camera_list())
}

// ─────────────────────────────────────────────────────────────────────────
// Commands
// ─────────────────────────────────────────────────────────────────────────

fn cmd_diff(data_dir: &Path, args: &DiffArgs) -> Result<()> {
    // Allow paths to be either absolute or relative to snapshots/.
    let a_path = resolve_snapshot_path(data_dir, &args.snapshot_a);
    let b_path = resolve_snapshot_path(data_dir, &args.snapshot_b);
    let a = Snapshot::load_from_file(&a_path)
        .with_context(|| format!("loading {}", a_path.display()))?;
    let b = Snapshot::load_from_file(&b_path)
        .with_context(|| format!("loading {}", b_path.display()))?;
    let opts = DiffOptions { include_volatile: args.include_volatile };
    let d = diff(&a, &b, &opts)?;

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
    let dir = snapshots_dir(data_dir);
    if !dir.exists() {
        println!("(no snapshots — directory {} does not exist)", dir.display());
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    entries.sort();

    let filter = args.camera.as_deref();
    let mut shown = 0usize;
    for path in &entries {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if let Some(f) = filter {
            // Filename starts with "{model_underscored}_{serial}_..."
            if !name.starts_with(&f.replace(' ', "_")) {
                continue;
            }
        }
        // Read just enough to surface camera + label + timestamp.
        match Snapshot::load_from_file(path) {
            Ok(s) => {
                println!(
                    "{name}\n  {model} {serial}  label={label}  fw={fw}  {captured}",
                    name = name,
                    model = s.camera.model,
                    serial = s.camera.serial,
                    label = s.label.as_deref().unwrap_or("-"),
                    fw = s.camera.firmware,
                    captured = s.captured_at,
                );
            }
            Err(e) => {
                println!("{name}  (failed to load: {e})");
            }
        }
        shown += 1;
    }
    if shown == 0 {
        println!("(no snapshots match)");
    }
    Ok(())
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
        RefCmd::List => {
            if !refs.exists() {
                println!("(no references — directory {} does not exist)", refs.display());
                return Ok(());
            }
            let mut entries: Vec<_> = fs::read_dir(&refs)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            entries.sort();
            for p in entries {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                println!("{name}");
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Live-camera commands (SDK)
// ─────────────────────────────────────────────────────────────────────────

// OP_GET and OP_SET are re-exported from nikon_fleet::sdk.

fn open_sdk(bundle_path: &Path, no_usb_reset: bool) -> Result<Sdk> {
    if !bundle_path.exists() {
        bail!(
            "SDK bundle not found at {}.\n\
             Run scripts/setup-sdk-runtime.sh first, or pass --sdk-bundle <path>.",
            bundle_path.display()
        );
    }
    let mut sdk = Sdk::open(bundle_path)
        .with_context(|| format!("loading SDK from {}", bundle_path.display()))?;
    if no_usb_reset {
        sdk.initialize_no_usb_reset().context("InitializeSDK")?;
    } else {
        sdk.initialize().context("InitializeSDK")?;
    }
    Ok(sdk)
}

fn cmd_discover(bundle_path: &Path, args: &DiscoverArgs, no_usb_reset: bool) -> Result<()> {
    let sdk = open_sdk(bundle_path, no_usb_reset)?;
    let enriched = enrich_devices(sdk.devices()?);
    if enriched.is_empty() {
        if args.json {
            println!("{{\"cameras\":[]}}");
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
            let serial   = usb.as_ref().map(|u| u.serial.as_str()).unwrap_or("?");
            let firmware = usb.as_ref().map(|u| u.firmware.as_str()).unwrap_or(dev.version.as_str());
            println!("  {}  serial={}  fw={}  id={}", dev.name, serial, firmware, dev.id);
        }
    }
    Ok(())
}

/// Build a map from numeric capability code → symbolic name for one model.
/// Takes the union across all firmware versions of that model.
fn name_map_for_model(schema: &MaidLayerConfig, model: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for section in schema.sections_for_model(model) {
        for cap in &section.capabilities {
            map.entry(cap.code).or_insert_with(|| cap.name.clone());
        }
    }
    map
}

fn cmd_snapshot(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &SnapshotArgs, no_usb_reset: bool) -> Result<()> {
    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    // Pick by --serial if given (matched against USB iSerialNumber), else first available.
    let (target, usb_info) = match args.serial.as_deref() {
        Some(s) => enriched.into_iter()
            .find(|(dev, usb)| {
                usb.as_ref().map(|u| u.serial.as_str()) == Some(s) || dev.id.to_string() == s
            })
            .ok_or_else(|| anyhow::anyhow!("no device with serial {s}"))?,
        None => {
            let idx = enriched.iter().position(|(d, _)| d.available).unwrap_or(0);
            enriched.into_iter().nth(idx).unwrap()
        }
    };
    println!("Connecting to {:?} (id={})…", target.name, target.id);

    let schema = MaidLayerConfig::parse_file(schema_path)
        .with_context(|| format!("loading schema from {}", schema_path.display()))?;
    let name_lookup = name_map_for_model(&schema, &target.name);
    if name_lookup.is_empty() {
        eprintln!(
            "warning: no schema entries found for model {:?}; properties will be keyed by numeric id",
            target.name
        );
    }

    let device = sdk.connect(target.id)?;
    println!("Reading {} capabilities…", device.capabilities.len());

    let captured_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let serial = usb_info.as_ref().map(|u| u.serial.clone())
        .unwrap_or_else(|| format!("id-{}", target.id));
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

    let mut read_ok = 0usize;
    let mut read_err = 0usize;
    let mut skipped_no_get = 0usize;
    for cap in &device.capabilities {
        if cap.operations & OP_GET == 0 {
            skipped_no_get += 1;
            continue;
        }
        match device.read_capability(cap.id) {
            Ok(value) => {
                let name = name_lookup
                    .get(&cap.id)
                    .cloned()
                    .unwrap_or_else(|| format!("cap_{:#x}", cap.id));
                snap.insert(name, cap.id, value);
                read_ok += 1;
            }
            Err(_) => read_err += 1,
        }
    }
    println!(
        "  ok={read_ok}  err={read_err}  skipped(no-get-bit)={skipped_no_get}"
    );

    // Save under snapshots/.
    let dir = snapshots_dir(data_dir);
    ensure_dir(&dir)?;
    let filename = snap.suggested_filename();
    let path = dir.join(&filename);
    snap.save_to_file(&path)?;
    println!("Wrote {}", path.display());
    Ok(())
}

fn cmd_check(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &CheckArgs, no_usb_reset: bool) -> Result<()> {
    let mut sdk = open_sdk(bundle, no_usb_reset)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        bail!("no Nikon cameras detected");
    }
    let enriched = enrich_devices(devices);
    let (target, usb_info) = match args.serial.as_deref() {
        Some(s) => enriched.into_iter()
            .find(|(dev, usb)| {
                usb.as_ref().map(|u| u.serial.as_str()) == Some(s) || dev.id.to_string() == s
            })
            .ok_or_else(|| anyhow::anyhow!("no device with serial {s}"))?,
        None => {
            let idx = enriched.iter().position(|(d, _)| d.available).unwrap_or(0);
            enriched.into_iter().nth(idx).unwrap()
        }
    };

    // Live snapshot (same logic as cmd_snapshot but in-memory).
    let schema = MaidLayerConfig::parse_file(schema_path)?;
    let name_lookup = name_map_for_model(&schema, &target.name);
    let device = sdk.connect(target.id)?;
    let serial = usb_info.as_ref().map(|u| u.serial.clone())
        .unwrap_or_else(|| format!("id-{}", target.id));
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
    for cap in &device.capabilities {
        if cap.operations & OP_GET == 0 {
            continue;
        }
        if let Ok(value) = device.read_capability(cap.id) {
            let name = name_lookup
                .get(&cap.id)
                .cloned()
                .unwrap_or_else(|| format!("cap_{:#x}", cap.id));
            live.insert(name, cap.id, value);
        }
    }

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

    let d = diff(&reference, &live, &DiffOptions::default())?;
    print_diff_human(&reference, &live, &d);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Firmware commands
// ─────────────────────────────────────────────────────────────────────────

fn cmd_firmware_add(data_dir: &Path, args: &FirmwareAddArgs) -> Result<()> {
    if !args.bin_path.exists() {
        bail!("firmware file not found: {}", args.bin_path.display());
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
    let rel = args.snapshot_path
        .strip_prefix(data_dir)
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
    instructions.push_str("\nSTEP 1 — VERIFY FIRMWARE FILE\n");
    instructions.push_str(&format!("  File:    firmware.bin\n"));
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

fn cmd_restore(data_dir: &Path, bundle: &Path, args: &RestoreArgs, no_usb_reset: bool) -> Result<()> {
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let snap = Snapshot::load_from_file(&snap_path)
        .with_context(|| format!("loading {}", snap_path.display()))?;

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
        .find(|(dev, usb)| {
            usb.as_ref().map(|u| u.serial.as_str()) == Some(target_serial)
                || dev.id.to_string() == target_serial
        })
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

    // Index capabilities by code for O(1) lookup.
    let cap_map: HashMap<u32, &CapabilityInfo> =
        device.capabilities.iter().map(|c| (c.id, c)).collect();

    let mut written = 0usize;
    let mut skipped_no_set = 0usize;
    let mut skipped_type = 0usize;
    let mut errors = 0usize;

    for (name, prop) in &snap.properties {
        let Some(cap) = cap_map.get(&prop.code) else {
            skipped_no_set += 1;
            continue;
        };
        if cap.operations & OP_SET == 0 {
            skipped_no_set += 1;
            continue;
        }
        if args.dry_run {
            println!("  would write  {} [{:#x}]", name, prop.code);
            written += 1;
            continue;
        }
        match device.write_capability(prop.code, cap.kind, &prop.value) {
            Ok(()) => written += 1,
            Err(SdkError::UnsupportedWrite(_)) => skipped_type += 1,
            Err(e) => {
                eprintln!("  warn: {} [{:#x}]: {}", name, prop.code, e);
                errors += 1;
            }
        }
    }

    println!(
        "{} written={written}  skipped(read-only)={skipped_no_set}  \
         skipped(unsupported-type)={skipped_type}  errors={errors}",
        if args.dry_run { "Dry run:" } else { "Restore complete:" }
    );
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
        Cmd::Diff(args) => cmd_diff(&cli.data_dir, args),
        Cmd::Ls(args) => cmd_ls(&cli.data_dir, args),
        Cmd::Rm(args) => cmd_rm(&cli.data_dir, args),
        Cmd::Ref(sub) => cmd_ref(&cli.data_dir, sub),
        Cmd::Restore(args) => cmd_restore(&cli.data_dir, &cli.sdk_bundle, args, no_reset),
        Cmd::Firmware(sub) => match sub {
            FirmwareCmd::Add(args) => cmd_firmware_add(&cli.data_dir, args),
            FirmwareCmd::Ls(args) => cmd_firmware_ls(&cli.data_dir, args),
            FirmwareCmd::Pin(args) => cmd_firmware_pin(&cli.data_dir, args),
            FirmwareCmd::Rollback(args) => cmd_firmware_rollback(&cli.data_dir, &cli.schema, args),
            FirmwareCmd::Check(args) => cmd_firmware_check(&cli.data_dir, args),
        },
    }
}
