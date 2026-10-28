//! `fleet-mcp`: the optional MCP server for nikon-fleet. Speaks MCP on stdio and
//! logs to stderr. Stdout is the protocol channel: nothing else may write to it.
//!
//! ```text
//! fleet-mcp --data-dir ~/nikon-fleet-data [--enable-writes]
//! ```
//!
//! Without `--enable-writes` the `*_apply` tools do not exist.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use fleet_mcp::server::{Config, Deadlines, FleetMcp};
use fleet_mcp::supervisor::Supervisor;
use rmcp::ServiceExt;

#[derive(Parser, Debug)]
#[command(name = "fleet-mcp", about = "MCP server for nikon-fleet (stdio)")]
struct Args {
    /// Directory holding snapshots/ and references/.
    #[arg(long)]
    data_dir: PathBuf,

    /// The `nikon-fleet` binary to run as the camera worker (default: beside this executable).
    #[arg(long)]
    worker: Option<PathBuf>,

    /// Passed to the worker as --sdk-bundle (default: the worker's own).
    #[arg(long)]
    sdk_bundle: Option<PathBuf>,

    /// MaidLayer.config, passed to the worker and used for diffs (default: the repository's).
    #[arg(long)]
    schema: Option<PathBuf>,

    /// Register the tools that write to cameras (transplant_apply, restore_apply).
    #[arg(long)]
    enable_writes: bool,

    /// Give every camera call this many seconds instead of the per-tool defaults.
    #[arg(long, value_name = "SECS")]
    deadline_secs: Option<u64>,
}

fn default_worker() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", exe.display()))?;
    Ok(dir.join("nikon-fleet"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let worker = match args.worker {
        Some(w) => w,
        None => default_worker()?,
    };
    if !worker.is_file() {
        anyhow::bail!("worker binary not found at {} (use --worker)", worker.display());
    }
    let schema = args.schema.clone().unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../sdk-runtime/MaidLayer.config")));

    let mut global: Vec<OsString> = vec!["--data-dir".into(), args.data_dir.clone().into()];
    if let Some(b) = &args.sdk_bundle {
        global.extend(["--sdk-bundle".into(), b.clone().into()]);
    }
    if let Some(s) = &args.schema {
        global.extend(["--schema".into(), s.clone().into()]);
    }

    let cfg = Config {
        data_dir: args.data_dir,
        schema,
        enable_writes: args.enable_writes,
        deadlines: args.deadline_secs.map(|s| Deadlines::all(Duration::from_secs(s))).unwrap_or_default(),
    };
    eprintln!(
        "fleet-mcp: data dir {}, worker {}, camera writes {}",
        cfg.data_dir.display(),
        worker.display(),
        if cfg.enable_writes { "ENABLED" } else { "disabled" }
    );
    let server = FleetMcp::new(cfg, Supervisor::new(worker, global));
    let tasks = server.tasks.clone();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    // The client is gone. A worker that is mid-apply must be allowed to finish (or hit its
    // own deadline): killing it half way would leave the camera half written.
    tasks.close();
    if tokio::time::timeout(Duration::from_secs(400), tasks.wait()).await.is_err() {
        eprintln!("fleet-mcp: a camera call was still running after 400 s; exiting anyway");
    }
    Ok(())
}
