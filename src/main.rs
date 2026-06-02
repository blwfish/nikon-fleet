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

use nikon_fleet::diff::{Diff, DiffOptions, diff};
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

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List USB-attached cameras.
    Discover,

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

fn open_sdk(bundle_path: &Path) -> Result<Sdk> {
    if !bundle_path.exists() {
        bail!(
            "SDK bundle not found at {}.\n\
             Run scripts/setup-sdk-runtime.sh first, or pass --sdk-bundle <path>.",
            bundle_path.display()
        );
    }
    let mut sdk = Sdk::open(bundle_path)
        .with_context(|| format!("loading SDK from {}", bundle_path.display()))?;
    sdk.initialize().context("InitializeSDK")?;
    Ok(sdk)
}

fn cmd_discover(bundle_path: &Path) -> Result<()> {
    let sdk = open_sdk(bundle_path)?;
    let devices = sdk.devices()?;
    if devices.is_empty() {
        println!("(no Nikon cameras detected)");
        return Ok(());
    }
    println!("{} device(s):", devices.len());
    for d in devices {
        println!(
            "  id={}  name={:?}  available={}  pid={}  version={:?}",
            d.id, d.name, d.available, d.connected_pid, d.version
        );
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

fn cmd_snapshot(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &SnapshotArgs) -> Result<()> {
    let mut sdk = open_sdk(bundle)?;
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

fn cmd_check(data_dir: &Path, bundle: &Path, schema_path: &Path, args: &CheckArgs) -> Result<()> {
    let mut sdk = open_sdk(bundle)?;
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

fn cmd_restore(data_dir: &Path, bundle: &Path, args: &RestoreArgs) -> Result<()> {
    let snap_path = resolve_snapshot_path(data_dir, &args.snapshot_path);
    let snap = Snapshot::load_from_file(&snap_path)
        .with_context(|| format!("loading {}", snap_path.display()))?;

    let mut sdk = open_sdk(bundle)?;
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
    match &cli.cmd {
        Cmd::Discover => cmd_discover(&cli.sdk_bundle),
        Cmd::Snapshot(args) => cmd_snapshot(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args),
        Cmd::Check(args) => cmd_check(&cli.data_dir, &cli.sdk_bundle, &cli.schema, args),
        Cmd::Diff(args) => cmd_diff(&cli.data_dir, args),
        Cmd::Ls(args) => cmd_ls(&cli.data_dir, args),
        Cmd::Rm(args) => cmd_rm(&cli.data_dir, args),
        Cmd::Ref(sub) => cmd_ref(&cli.data_dir, sub),
        Cmd::Restore(args) => cmd_restore(&cli.data_dir, &cli.sdk_bundle, args),
    }
}
