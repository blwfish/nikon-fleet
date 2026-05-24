//! `fleet` CLI — settings management for a fleet of Nikon Z cameras.
//!
//! All real logic lives in the `nikon_fleet` library; this file is just
//! the command-line surface.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use nikon_fleet::diff::{Diff, DiffOptions, diff};
use nikon_fleet::snapshot::Snapshot;

/// Top-level CLI parser. `clap` generates --help, completion, validation,
/// and arg parsing from these derives.
#[derive(Parser, Debug)]
#[command(name = "fleet", about = "Manage settings across a fleet of Nikon Z cameras", version)]
struct Cli {
    /// Base directory for snapshots/ and references/ subdirs.
    #[arg(long, default_value = ".", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List USB-attached cameras (requires SDK wiring; not yet implemented).
    Discover,

    /// Capture current settings from a live camera (not yet implemented).
    Snapshot(SnapshotArgs),

    /// Manage per-camera reference snapshots.
    #[command(subcommand)]
    Ref(RefCmd),

    /// Diff two snapshot files.
    Diff(DiffArgs),

    /// Diff a live camera against its reference (not yet implemented).
    Check(CheckArgs),

    /// List stored snapshots.
    Ls(LsArgs),

    /// Remove a stored snapshot.
    Rm(RmArgs),
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

fn cmd_not_yet(name: &str) -> Result<()> {
    println!("`fleet {name}` is not wired up yet — needs the Nikon SDK FFI module.");
    println!("Coming next once a camera is available for testing.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Discover => cmd_not_yet("discover"),
        Cmd::Snapshot(_) => cmd_not_yet("snapshot"),
        Cmd::Check(_) => cmd_not_yet("check"),
        Cmd::Diff(args) => cmd_diff(&cli.data_dir, args),
        Cmd::Ls(args) => cmd_ls(&cli.data_dir, args),
        Cmd::Rm(args) => cmd_rm(&cli.data_dir, args),
        Cmd::Ref(sub) => cmd_ref(&cli.data_dir, sub),
    }
}
