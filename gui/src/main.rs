use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use nikon_fleet::firmware::{
    FIRMWARE_META_FORMAT_VERSION, FirmwareMeta, firmware_root, model_slug, scan_archives, sha256_file,
};
use nikon_fleet::ops::cameras::{AmbiguousPairing, CameraList, duplicate_models, list_cameras};
use nikon_fleet::ops::list::{EntryState, list_dir};
use nikon_fleet::ops::paths::{checked_reference_filename, is_safe_path_component, write_atomic};
use nikon_fleet::ops::apply::{ApplyStatus, EXIT_REFUSED_BEFORE_WRITE, EXIT_STALE_PLAN};
use nikon_fleet::ops::labels::Labels;
use nikon_fleet::ops::plan::planned_would_change;
use nikon_fleet::ops::render::{self, Row, RowFilter};
use nikon_fleet::ops::restore::undo_command;
use nikon_fleet::ops::target::{is_id_fallback, storable_serial};
use nikon_fleet::ptp_usb::{self, FtpField, FtpProfile};
use nikon_fleet::settings::{self, Settings};
use nikon_fleet::snapshot::Snapshot;
use nikon_fleet::transplant::{PROPERTY_PREFIX, Severity};
use nikon_fleet::vendor_argv::{vendor_read_argv, vendor_write_ftp_argv};
use serde::Deserialize;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

mod check;
mod restore;

use check::{CheckRequest, CheckUi, CheckView};
use restore::{
    RestoreApplyJson, RestorePlanJson, RestoreRequest, RestoreUi, SourceView, UndoOffer, RestoreActionLike,
};

// This GUI is a thin front end over the `nikon-fleet` CLI and the `nikon_fleet`
// library. Everything that touches a camera (discover, snapshot, the vendor ops)
// runs as a CLI subprocess with a timeout, so a hung SDK call can never wedge this
// process or leave its camera lock held; everything about stored files (listing,
// references, settings) goes through the library's own code.

// ── Settings ──────────────────────────────────────────────────────────────

/// What the GUI knows about `settings.json` (`nikon_fleet::settings` owns the file's
/// format, location and rules: unknown keys survive a save, a blank `data_dir` is
/// unset, an unreadable file is an error and is never overwritten).
#[derive(Debug, Clone, PartialEq)]
struct SettingsState {
    path: PathBuf,
    /// The directory in use. `None` when the settings file could not be read: the
    /// GUI then refuses to read or write stored data rather than guess a directory
    /// the other programs do not use.
    data_dir: Option<PathBuf>,
    /// `data_dir` as written in the file (`None` = unset, the default applies).
    configured: Option<String>,
    error: Option<String>,
}

fn load_settings_state(path: &Path) -> SettingsState {
    match Settings::load_from(path) {
        Ok(s) => SettingsState {
            path: path.to_path_buf(),
            data_dir: Some(s.effective_data_dir()),
            configured: s.data_dir().map(str::to_string),
            error: None,
        },
        Err(e) => SettingsState { path: path.to_path_buf(), data_dir: None, configured: None, error: Some(e.to_string()) },
    }
}

/// An unreadable `settings.json` is an error the user must see, not a quiet default.
fn settings_error_status(state: &SettingsState) -> Option<Status> {
    state.error.as_ref().map(|e| Status::new(Level::Error, format!("settings.json is unusable: {e}")))
}

/// Save the data directory typed in Preferences. A blank value clears the setting (the
/// default applies); anything else must be absolute, as `nikon-fleet settings
/// set-data-dir` requires (a relative path would mean a different place to each
/// program). The file is re-read first and never overwritten when it cannot be parsed.
fn save_data_dir(path: &Path, typed: &str) -> Result<SettingsState, String> {
    let typed = typed.trim();
    if !typed.is_empty() && !Path::new(typed).is_absolute() {
        return Err(format!(
            "{typed:?} is not an absolute path; a relative data directory would mean a different place to each program"
        ));
    }
    settings::update_in(path, |s| s.set_data_dir(Some(typed))).map_err(|e| e.to_string())?;
    Ok(load_settings_state(path))
}

// ── Worker thread protocol ────────────────────────────────────────────────

#[derive(Debug)]
enum Cmd {
    Discover,
    Snapshot { camera: CameraRow, label: String },
    RefreshListing,
    SetReference { file: String },
    SetDataDir(Result<PathBuf, String>),
    Export { dest: PathBuf },
    Import { src: PathBuf },
    VendorRead { propcode: u16, serial: Option<String> },
    VendorWriteFtp { profile: FtpProfile, serial: Option<String> },
    /// Read-only: plan copying a stored snapshot onto a connected body.
    TransplantPlan { request: TransplantRequest },
    /// WRITES TO THE CAMERA, only the plan with this digest and summary (the ones that were shown
    /// and confirmed).
    TransplantApply { request: TransplantRequest, digest: String, summary: String },
    /// Read-only: plan putting a stored snapshot of a connected body back on it.
    RestorePlan { request: RestoreRequest },
    /// WRITES TO THE CAMERA, only the plan with this digest and summary (the ones that were shown
    /// and confirmed).
    RestoreApply { request: RestoreRequest, digest: String, summary: String },
    /// Read-only: capture a connected body and compare it with its reference.
    Check { request: CheckRequest },
}

enum Evt {
    Cameras(DiscoverResult),
    Listing(SnapListing),
    Done(OpDone),
    Vendor { op: VendorOp, result: Result<VendorOut, String> },
    TransplantPlanned { request: TransplantRequest, result: Result<(PlanJson, String), String> },
    TransplantApplied { request: TransplantRequest, outcome: ApplyOutcome },
    RestorePlanned { request: RestoreRequest, result: Result<(RestorePlanJson, String, SourceView), String> },
    RestoreApplied { request: RestoreRequest, outcome: ApplyOutcome<RestoreApplyJson> },
    Checked { request: CheckRequest, result: Result<CheckView, String> },
    Err(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VendorOp {
    Read,
    WriteFtp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VendorOut {
    text: String,
    /// What the CLI said on stderr during a successful run (the unconfirmed-model
    /// warning, a reply that did not match its command): shown next to the result.
    stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Status {
    text: String,
    level: Level,
}

impl Status {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Status { text: text.into(), level }
    }
}

/// What a finished operation tells the user.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpDone {
    status: Status,
    /// Lines that must be seen (skipped entries, rejected files, warnings).
    notices: Vec<String>,
    /// The CLI's stderr from a successful run, shown in a collapsible block.
    cli_log: String,
    /// The stored files changed, so the cached listing is stale.
    refresh_listing: bool,
}

/// One body as `nikon-fleet discover --json` reports it, plus what this GUI derives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CameraRow {
    model: String,
    /// The identity snapshots are stored under: the USB serial, else `id-N`. Never empty.
    serial: String,
    firmware: String,
    known_model: bool,
    baseline: bool,
    /// How many attached bodies share this model (>1 makes the serial pairing unverifiable).
    model_count: usize,
}

impl CameraRow {
    fn pairing_ambiguous(&self) -> bool {
        self.model_count > 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoverResult {
    rows: Vec<CameraRow>,
    notices: Vec<String>,
    cli_log: String,
}

/// A cached listing of `snapshots/`: everything `ops::list::list_dir` found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SnapListing {
    dir: String,
    dir_exists: bool,
    /// Readable snapshots, newest first (by `captured_at`, ties by file name).
    rows: Vec<SnapRow>,
    /// `*.json` files that are not valid snapshots: they belong to no camera, so they are
    /// listed apart rather than dropped.
    unreadable: Vec<UnreadableRow>,
    skipped_dir_entries: usize,
    reference_read_failures: usize,
    /// The directory exists but could not be listed.
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapRow {
    file: String,
    model: String,
    serial: String,
    label: Option<String>,
    captured_at: String,
    is_reference: bool,
    /// The file name was not valid UTF-8 (`list_dir` shows it lossily), so it cannot be
    /// named in a later operation.
    name_lossy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnreadableRow {
    file: String,
    error_kind: &'static str,
    error: String,
}

impl SnapListing {
    /// The bodies that have stored snapshots but are not among `cameras`, as (model, serial), sorted
    /// and without repeats. They are what the sidebar offers when no camera is plugged in.
    fn stored_only_bodies(&self, cameras: &[CameraRow]) -> Vec<(String, String)> {
        let mut out = self.bodies();
        out.retain(|(model, serial)| !cameras.iter().any(|c| &c.model == model && &c.serial == serial));
        out
    }

    /// Every body that has a stored snapshot, as (model, serial), sorted and without repeats.
    fn bodies(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.rows.iter().map(|r| (r.model.clone(), r.serial.clone())).collect();
        out.sort();
        out.dedup();
        out
    }

    fn rows_for<'a>(&'a self, model: &'a str, serial: &'a str) -> impl Iterator<Item = &'a SnapRow> {
        self.rows.iter().filter(move |r| r.model == model && r.serial == serial)
    }

    /// Everything about the directory that a plain list of rows would hide.
    fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.error {
            out.push(format!("The snapshots directory could not be listed: {e}"));
        }
        if !self.unreadable.is_empty() {
            out.push(format!(
                "{} file(s) in {} are not valid snapshots and are not listed under any camera",
                self.unreadable.len(),
                self.dir
            ));
        }
        if self.skipped_dir_entries > 0 {
            out.push(format!(
                "{} entr{} in {} could not be read and {} not listed",
                self.skipped_dir_entries,
                if self.skipped_dir_entries == 1 { "y" } else { "ies" },
                self.dir,
                if self.skipped_dir_entries == 1 { "was" } else { "were" },
            ));
        }
        if self.reference_read_failures > 0 {
            out.push(format!(
                "{} reference file(s) exist but could not be read, so their snapshots are shown without a reference mark",
                self.reference_read_failures
            ));
        }
        let lossy = self.rows.iter().filter(|r| r.name_lossy).count();
        if lossy > 0 {
            out.push(format!("{lossy} snapshot file name(s) are not valid UTF-8; they cannot be set as a reference here"));
        }
        out
    }
}

/// List `snapshots/` through the library's `ops::list::list_dir` (the CLI's `ls --json`
/// code): the validity rule, the byte-identity reference rule and the order are its.
fn list_all(data_dir: &Path) -> SnapListing {
    let dir = data_dir.join("snapshots");
    let shown = dir.display().to_string();
    let listing = match list_dir(&dir, None, Some(&data_dir.join("references"))) {
        Ok(l) => l,
        Err(e) => return SnapListing { dir: shown, error: Some(e.to_string()), ..SnapListing::default() },
    };
    let mut out = SnapListing {
        dir: listing.dir,
        dir_exists: listing.dir_exists,
        skipped_dir_entries: listing.skipped_dir_entries,
        reference_read_failures: listing.reference_read_failures,
        ..SnapListing::default()
    };
    for entry in listing.entries {
        match entry.state {
            EntryState::Ok { model, serial, label, captured_at, is_reference, .. } => out.rows.push(SnapRow {
                name_lossy: entry.file.contains('\u{FFFD}'),
                file: entry.file,
                model,
                serial,
                label,
                captured_at,
                is_reference: is_reference == Some(true),
            }),
            EntryState::Unreadable { error_kind, error } => {
                out.unreadable.push(UnreadableRow { file: entry.file, error_kind, error })
            }
        }
    }
    // Newest first, as the list has always been shown; the full timestamp string, ties by file name
    // (`list_dir` hands the rows over sorted by file name, so a stable sort keeps that for ties).
    out.rows.sort_by(|a, b| b.captured_at.cmp(&a.captured_at).then_with(|| a.file.cmp(&b.file)));
    out
}

// ── Snapshot viewer ───────────────────────────────────────────────────────

/// One snapshot window's state. Loading and drawing happen elsewhere; this is the part that has
/// rules: what is shown, what the filter keeps, what a failed load looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotViewer {
    /// The file's name in `snapshots/`: what identifies the window.
    file: String,
    is_reference: bool,
    loading: bool,
    /// Why the file cannot be shown. When set there are no rows: an unreadable file shows its
    /// error, not an empty table that reads as "nothing in it".
    error: Option<String>,
    header: Option<render::Header>,
    /// Every property, sorted by name; [`SnapshotViewer::visible_rows`] applies the filter.
    rows: Vec<Row>,
    filter: String,
    /// Show the `0x8254` code column. Off by default: the name says what a row is; the code is for
    /// finding it in the SDK's documents (the filter matches codes either way).
    show_codes: bool,
    /// Why some names are missing (no `sdk-runtime/`, an unreadable labels file), when they are.
    labels_note: Option<String>,
}

impl SnapshotViewer {
    fn loading(file: &str, is_reference: bool) -> Self {
        SnapshotViewer {
            file: file.to_string(),
            is_reference,
            loading: true,
            error: None,
            header: None,
            rows: Vec::new(),
            filter: String::new(),
            show_codes: false,
            labels_note: None,
        }
    }

    /// The load's answer: the rendered snapshot, or why there is none.
    fn finish(&mut self, result: Result<render::RenderedSnapshot, String>) {
        self.loading = false;
        match result {
            Ok(r) => {
                self.error = None;
                self.header = Some(r.header);
                self.rows = r.rows;
                self.labels_note = r.labels_note;
            }
            Err(e) => {
                self.error = Some(e);
                self.header = None;
                self.rows.clear();
                self.labels_note = None;
            }
        }
    }

    /// The rows the filter keeps, in name order: the library's one filter rule.
    fn visible_rows(&self) -> Vec<&Row> {
        let filter = RowFilter::new(&self.filter);
        self.rows.iter().filter(|r| filter.matches(r)).collect()
    }

    /// "N of M properties", once there is something loaded to count.
    fn count_line(&self) -> Option<String> {
        self.header.as_ref()?;
        Some(format!("{} of {} properties", self.visible_rows().len(), self.rows.len()))
    }

    /// Model, serial, firmware, label and capture time; the file name until the file is read.
    fn title(&self) -> String {
        let mark = if self.is_reference { "  ◄ ref" } else { "" };
        match &self.header {
            Some(h) => format!(
                "{} {}  fw {}  {}  {}{mark}",
                h.model,
                h.serial,
                h.firmware,
                h.label.as_deref().unwrap_or("(no label)"),
                h.captured_at.get(..19).unwrap_or(&h.captured_at),
            ),
            None => format!("{}{mark}", self.file),
        }
    }
}

/// The snapshot windows that are open, at most one per file.
#[derive(Debug, Default)]
struct SnapshotViewers {
    open: Vec<SnapshotViewer>,
}

impl SnapshotViewers {
    /// Open the window for `file`. True when its file needs reading: a new window, or one that
    /// failed before (opening it again is the retry). A window that already shows the file is left
    /// as it is, filter text included.
    fn open(&mut self, file: &str, is_reference: bool) -> bool {
        match self.open.iter_mut().find(|v| v.file == file) {
            Some(v) if v.error.is_none() => false,
            Some(v) => {
                *v = SnapshotViewer::loading(file, is_reference);
                true
            }
            None => {
                self.open.push(SnapshotViewer::loading(file, is_reference));
                true
            }
        }
    }

    /// Hand a load's answer to its window. False when the window was closed meanwhile.
    fn finish(&mut self, file: &str, result: Result<render::RenderedSnapshot, String>) -> bool {
        match self.open.iter_mut().find(|v| v.file == file) {
            Some(v) => {
                v.finish(result);
                true
            }
            None => false,
        }
    }

    /// Close the window for `file`; false if none was open.
    fn close(&mut self, file: &str) -> bool {
        let before = self.open.len();
        self.open.retain(|v| v.file != file);
        self.open.len() != before
    }

    /// Close the windows whose close was asked for this frame, given each open window's file and
    /// whether its close button was pressed. Returns the files that were closed, so a request for
    /// a window that is no longer open (or a window not asking) changes nothing.
    fn close_requested(&mut self, requests: impl IntoIterator<Item = (String, bool)>) -> Vec<String> {
        let mut closed = Vec::new();
        for (file, requested) in requests {
            if requested && self.close(&file) {
                closed.push(file);
            }
        }
        closed
    }

    /// Keep each window's reference mark current with the listing (a reference set while the
    /// window is open).
    fn sync_reference(&mut self, listing: &SnapListing) {
        for v in &mut self.open {
            if let Some(row) = listing.rows.iter().find(|r| r.file == v.file) {
                v.is_reference = row.is_reference;
            }
        }
    }
}

/// A finished load, from the thread that read the file.
struct ViewLoad {
    file: String,
    result: Result<render::RenderedSnapshot, String>,
}

/// Find, read, validate and render `snapshots/<file>` by the library's rules, so the window shows
/// what `nikon-fleet show` prints. Reads a file: call it off the UI thread.
fn load_for_view(data_dir: &Path, sdk_runtime: &Path, file: &str) -> Result<render::RenderedSnapshot, String> {
    let path = render::find_named(&[&data_dir.join("snapshots")], file).map_err(|e| e.to_string())?;
    let snapshot = render::load(&path).map_err(|e| e.to_string())?;
    let firmware = Some(snapshot.camera.firmware.as_str()).filter(|f| !f.is_empty());
    let labels = Labels::load_for(sdk_runtime, &snapshot.camera.model, firmware);
    Ok(render::render_snapshot_with(&snapshot, None, &labels))
}

/// Where the SDK's names are: `$NIKON_FLEET_SDK_RUNTIME` if set, else the project's `sdk-runtime/`
/// (where `scripts/setup-sdk-runtime.sh` puts it, and where the CLI's `--schema` default points).
fn resolve_sdk_runtime(env_override: Option<OsString>) -> PathBuf {
    match env_override {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../sdk-runtime")),
    }
}

fn spawn_view_load(data_dir: PathBuf, file: String, tx: mpsc::Sender<ViewLoad>, ctx: egui::Context) -> std::io::Result<()> {
    std::thread::Builder::new().name("snapshot-view".into()).spawn(move || {
        let sdk_runtime = resolve_sdk_runtime(std::env::var_os("NIKON_FLEET_SDK_RUNTIME"));
        let result = load_for_view(&data_dir, &sdk_runtime, &file);
        // The receiver is gone only when the window closed; there is no one to tell.
        let _ = tx.send(ViewLoad { file, result });
        ctx.request_repaint();
    })?;
    Ok(())
}

// ── App ───────────────────────────────────────────────────────────────────

struct FleetApp {
    cmd_tx: mpsc::Sender<Cmd>,
    evt_rx: mpsc::Receiver<Evt>,
    cameras: Vec<CameraRow>,
    selected: Option<usize>,
    /// A body that is not connected but has stored snapshots, picked from the sidebar's "Stored" list
    /// (model, serial). Only ever set while `selected` is `None`.
    selected_stored: Option<(String, String)>,
    listing: Option<SnapListing>,
    viewers: SnapshotViewers,
    view_tx: mpsc::Sender<ViewLoad>,
    view_rx: mpsc::Receiver<ViewLoad>,
    label: String,
    status: Status,
    notices: Vec<String>,
    cli_log: String,
    /// Commands sent and not yet answered; every command is answered by exactly one event.
    pending: usize,
    /// The worker's channel closed (it panicked or returned); said once, then every command fails at `send`.
    worker_gone: bool,
    busy_since: Option<Instant>,
    // Preferences
    settings: SettingsState,
    prefs_open: bool,
    prefs_data_dir: String,
    // Vendor ops (raw USB PTP — see docs/nx-field-session-2026-07-09.md)
    vendor_ops_open: bool,
    vendor_propcode: String,
    vendor_read_result: String,
    ftp_form: FtpForm,
    pending_write: Option<PendingWrite>,
    vendor_write_result: String,
    transplant: TransplantUi,
    restore: RestoreUi,
    check: CheckUi,
}

impl FleetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = load_settings_state(&settings::settings_path());
        let prefs_data_dir = settings.configured.clone().unwrap_or_default();
        let status = settings_error_status(&settings)
            .unwrap_or_else(|| Status::new(Level::Info, "Click Discover to find cameras."));

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let (view_tx, view_rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        let initial = settings.data_dir_or_err();
        std::thread::spawn(move || worker(cmd_rx, evt_tx, ctx, initial));

        let mut app = Self {
            cmd_tx,
            evt_rx,
            cameras: Vec::new(),
            selected: None,
            selected_stored: None,
            listing: None,
            viewers: SnapshotViewers::default(),
            view_tx,
            view_rx,
            label: String::new(),
            status,
            notices: Vec::new(),
            cli_log: String::new(),
            pending: 0,
            worker_gone: false,
            busy_since: None,
            settings,
            prefs_open: false,
            prefs_data_dir,
            vendor_ops_open: false,
            vendor_propcode: "0xD053".to_string(),
            vendor_read_result: String::new(),
            ftp_form: FtpForm::default(),
            pending_write: None,
            vendor_write_result: String::new(),
            transplant: TransplantUi::default(),
            restore: RestoreUi::default(),
            check: CheckUi::default(),
        };
        app.refresh_listing();
        app
    }

    fn busy(&self) -> bool {
        self.pending > 0
    }

    fn selected_camera(&self) -> Option<&CameraRow> {
        self.selected.and_then(|i| self.cameras.get(i))
    }

    /// Serial of the currently selected camera, if any. Vendor ops target
    /// this the same way Snapshot does, but don't require it — the CLI/
    /// library auto-selects when exactly one Nikon camera is on USB.
    fn selected_serial(&self) -> Option<String> {
        self.selected_camera().map(|c| c.serial.clone())
    }

    /// Hand `cmd` to the worker; false (with the status saying so) if the worker is gone.
    fn send(&mut self, cmd: Cmd) -> bool {
        if self.pending == 0 {
            self.busy_since = Some(Instant::now());
        }
        self.pending += 1;
        if self.cmd_tx.send(cmd).is_err() {
            self.pending -= 1;
            if self.pending == 0 {
                self.busy_since = None;
            }
            self.status = Status::new(Level::Error, "The worker thread has stopped; restart the app.");
            return false;
        }
        true
    }

    /// Re-list the stored snapshots (after anything that changes them). Selecting another
    /// camera needs no listing: the cached one holds every camera's snapshots.
    fn refresh_listing(&mut self) {
        if self.settings.data_dir.is_some() {
            self.send(Cmd::RefreshListing);
        }
    }

    /// Tell the worker which directory the (re)loaded settings name, and re-list it.
    fn use_settings_data_dir(&mut self) {
        self.transplant.data_dir_changed();
        self.restore.data_dir_changed();
        self.check.data_dir_changed();
        if self.cmd_tx.send(Cmd::SetDataDir(self.settings.data_dir_or_err())).is_err() {
            self.status = Status::new(Level::Error, "The worker thread has stopped; restart the app.");
            return;
        }
        self.listing = None;
        self.refresh_listing();
    }

    /// A confirmation to write to a camera is only good for the exact target and form it
    /// was asked for.
    fn drop_stale_confirmation(&mut self) {
        let stale = self
            .pending_write
            .as_ref()
            .is_some_and(|p| p.is_stale(&self.ftp_form, self.selected_camera()));
        if stale {
            self.pending_write = None;
            self.vendor_write_result =
                "Confirmation withdrawn: the form or the selected camera changed. Nothing was written.".into();
        }
    }

    /// Open the window for a stored snapshot and read its file off the UI thread. Reading is not
    /// a worker command: it must not wait behind a camera operation, and it changes nothing.
    fn open_snapshot(&mut self, ctx: &egui::Context, file: &str, is_reference: bool, name_lossy: bool) {
        if name_lossy {
            self.status = Status::new(Level::Warn, "That file's name is not valid UTF-8, so it cannot be opened here.");
            return;
        }
        let Some(data_dir) = self.settings.data_dir.clone() else {
            self.status = Status::new(Level::Error, "The data directory is unknown, so no snapshot can be opened.");
            return;
        };
        if !self.viewers.open(file, is_reference) {
            return;
        }
        if let Err(e) = spawn_view_load(data_dir, file.to_string(), self.view_tx.clone(), ctx.clone()) {
            self.viewers.finish(file, Err(format!("could not start reading the file: {e}")));
        }
    }

    fn poll_views(&mut self) {
        while let Ok(load) = self.view_rx.try_recv() {
            self.viewers.finish(&load.file, load.result);
        }
    }

    /// The worker's channel closed: it panicked or returned. Nothing it was doing will be answered.
    fn worker_stopped(&mut self) {
        if std::mem::replace(&mut self.worker_gone, true) {
            return;
        }
        self.pending = 0;
        self.busy_since = None;
        self.status = Status::new(Level::Error, "The worker thread has stopped; restart the app.");
        self.transplant.worker_lost();
        self.restore.worker_lost();
        self.check.worker_lost();
    }

    fn poll(&mut self) {
        loop {
            let evt = match self.evt_rx.try_recv() {
                Ok(evt) => evt,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker_stopped();
                    break;
                }
            };
            self.pending = self.pending.saturating_sub(1);
            if self.pending == 0 {
                self.busy_since = None;
            }
            match evt {
                Evt::Cameras(found) => {
                    let n = found.rows.len();
                    self.status = if found.notices.is_empty() {
                        Status::new(Level::Info, format!("Found {n} camera(s)."))
                    } else {
                        Status::new(Level::Warn, format!("Found {n} camera(s); see the notes below."))
                    };
                    self.cameras = found.rows;
                    self.selected = (!self.cameras.is_empty()).then_some(0);
                    if self.selected.is_some() {
                        self.selected_stored = None;
                    }
                    self.notices = found.notices;
                    self.cli_log = found.cli_log;
                }
                Evt::Listing(l) => self.listing = Some(l),
                Evt::Done(done) => {
                    self.status = done.status;
                    self.notices = done.notices;
                    self.cli_log = done.cli_log;
                    if done.refresh_listing {
                        self.refresh_listing();
                    }
                }
                Evt::Vendor { op, result } => {
                    let (text, level) = match result {
                        Ok(out) => (with_stderr(&out.text, &out.stderr), Level::Info),
                        Err(msg) => (format!("Error: {msg}"), Level::Error),
                    };
                    self.status = Status::new(level, text.lines().next().unwrap_or("").to_string());
                    match op {
                        VendorOp::Read => self.vendor_read_result = text,
                        VendorOp::WriteFtp => self.vendor_write_result = text,
                    }
                }
                Evt::TransplantPlanned { request, result } => {
                    self.status = match &result {
                        Ok((plan, _)) => Status::new(Level::Info, format!("Plan: {}", plan.summary)),
                        Err(e) => Status::new(Level::Error, e.lines().next().unwrap_or("").to_string()),
                    };
                    self.transplant.plan_arrived(request, result);
                }
                Evt::TransplantApplied { request, outcome } => {
                    self.status = applied_status(&outcome);
                    // A restore point may have been written, and the camera may have changed.
                    let wrote = !matches!(outcome, ApplyOutcome::NotApplied { .. });
                    self.transplant.applied(request, outcome);
                    if wrote {
                        self.refresh_listing();
                    }
                }
                Evt::RestorePlanned { request, result } => {
                    self.status = match &result {
                        Ok((plan, _, _)) => Status::new(Level::Info, format!("Restore plan: {}", plan.summary)),
                        Err(e) => Status::new(Level::Error, e.lines().next().unwrap_or("").to_string()),
                    };
                    self.restore.plan_arrived(request, result);
                }
                Evt::RestoreApplied { request, outcome } => {
                    self.status = applied_status_of("Restore", &outcome);
                    // A restore point may have been written, and the camera may have changed.
                    let wrote = !matches!(outcome, ApplyOutcome::NotApplied { .. });
                    self.restore.applied(request, outcome);
                    if wrote {
                        self.refresh_listing();
                    }
                }
                Evt::Checked { request, result } => {
                    self.status = match &result {
                        Ok(view) => {
                            let (level, text) = check::check_headline(view);
                            Status::new(level, format!("Check {} {}: {text}", request.target.model, request.target.serial))
                        }
                        Err(e) => Status::new(Level::Error, e.lines().next().unwrap_or("").to_string()),
                    };
                    self.check.arrived(request, result);
                }
                Evt::Err(msg) => self.status = Status::new(Level::Error, format!("Error: {msg}")),
            }
        }
    }
}

impl SettingsState {
    fn data_dir_or_err(&self) -> Result<PathBuf, String> {
        self.data_dir.clone().ok_or_else(|| {
            format!(
                "settings.json is unusable, so the data directory is unknown: {}",
                self.error.as_deref().unwrap_or("(no detail)")
            )
        })
    }
}

fn with_stderr(text: &str, stderr: &str) -> String {
    if stderr.trim().is_empty() {
        text.to_string()
    } else {
        format!("{text}\n[stderr]\n{}", stderr.trim_end())
    }
}

fn level_color(level: Level) -> Option<egui::Color32> {
    match level {
        Level::Info => None,
        Level::Warn => Some(egui::Color32::from_rgb(200, 120, 0)),
        Level::Error => Some(egui::Color32::from_rgb(200, 60, 60)),
    }
}

impl eframe::App for FleetApp {
    // eframe 0.34 requires ui() for the simplified single-panel path; our
    // update() override handles layout directly so this is never called.
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.poll_views();
        self.drop_stale_confirmation();
        self.transplant.sync(&self.cameras);
        self.restore.sync(&self.cameras);
        self.check.sync(&self.cameras);
        self.ui_toolbar(ctx);
        self.ui_notices(ctx);
        self.ui_preferences(ctx);
        self.ui_vendor_ops(ctx);
        self.ui_transplant(ctx);
        self.ui_restore(ctx);
        self.ui_check(ctx);
        self.ui_sidebar(ctx);
        self.ui_main(ctx);
        self.ui_snapshot_viewers(ctx);
    }
}

impl FleetApp {
    fn ui_toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.busy(), |ui| {
                    if ui.button("⟳ Discover").clicked() {
                        self.send(Cmd::Discover);
                    }
                });
                if ui.button("⚙ Preferences").clicked() {
                    self.prefs_open = !self.prefs_open;
                }
                if ui.button("🔧 Vendor Ops").clicked() {
                    self.vendor_ops_open = !self.vendor_ops_open;
                }
                if ui.button("⇄ Transplant").clicked() {
                    self.transplant.open = !self.transplant.open;
                }
                if ui.button("↺ Restore").clicked() {
                    self.restore.open = !self.restore.open;
                }
                if ui.button("✓ Check").clicked() {
                    self.check.open = !self.check.open;
                }
                ui.separator();
                if let Some(since) = self.busy_since {
                    // No animated spinner: it repaints every frame. A once-a-second
                    // repaint keeps the elapsed time moving.
                    ui.label(format!("⏳ Working… {}s", since.elapsed().as_secs()));
                    ctx.request_repaint_after(Duration::from_secs(1));
                } else {
                    let text = egui::RichText::new(&self.status.text);
                    ui.label(match level_color(self.status.level) {
                        Some(c) => text.color(c),
                        None => text,
                    });
                }
            });
        });
    }

    /// Warnings and the CLI's own messages from the last operation.
    fn ui_notices(&mut self, ctx: &egui::Context) {
        if self.notices.is_empty() && self.cli_log.trim().is_empty() {
            return;
        }
        egui::TopBottomPanel::bottom("notices").show(ctx, |ui| {
            for n in &self.notices {
                ui.colored_label(egui::Color32::from_rgb(200, 120, 0), n);
            }
            if !self.cli_log.trim().is_empty() {
                let lines = self.cli_log.lines().count();
                egui::CollapsingHeader::new(format!("CLI messages ({lines} line(s))")).show(ui, |ui| {
                    // Drag the corner handle to make the log taller or shorter; the size is kept.
                    egui::Resize::default()
                        .id_salt("cli_log_resize")
                        .resizable([false, true])
                        .default_size(egui::vec2(ui.available_width(), 120.0))
                        .min_size(egui::vec2(0.0, 40.0))
                        .max_size(egui::vec2(f32::INFINITY, 700.0))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                ui.add(egui::Label::new(egui::RichText::new(self.cli_log.trim_end()).monospace()));
                            });
                        });
                });
            }
        });
    }

    fn ui_preferences(&mut self, ctx: &egui::Context) {
        let mut prefs_open = self.prefs_open;
        let mut save_prefs = false;
        let mut reload_prefs = false;
        let mut cancel_prefs = false;
        let mut export_trigger = false;
        let mut import_trigger = false;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::Window::new("Preferences")
            .open(&mut prefs_open)
            .resizable(false)
            .collapsible(false)
            .min_width(480.0)
            .show(ctx, |ui| {
                if let Some(e) = &self.settings.error {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), format!("settings.json is unusable: {e}"));
                    ui.label("Nothing is read or written until it is fixed (it is never overwritten). Edit or delete it, then Reload.");
                    if ui.button("Reload settings").clicked() {
                        reload_prefs = true;
                    }
                    ui.add_space(6.0);
                }
                ui.label("Data directory — snapshots and references are stored here:");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.prefs_data_dir)
                            .hint_text("(default)")
                            .desired_width(380.0),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.prefs_data_dir = path.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(format!("Default: {}", settings::default_data_dir().display()))
                        .small()
                        .weak(),
                );
                if let Some(dir) = &self.settings.data_dir {
                    ui.label(egui::RichText::new(format!("In use: {}", dir.display())).small().weak());
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                ui.label("Move data to another machine:");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy && data_ready, |ui| {
                        if ui
                            .button("↑  Export all…")
                            .on_hover_text("Pack snapshots, references and the firmware archive into a zip archive")
                            .clicked()
                        {
                            export_trigger = true;
                        }
                        if ui
                            .button("↓  Import archive…")
                            .on_hover_text(
                                "Add the archive's files to the current data directory. Files that already exist are kept, never replaced.",
                            )
                            .clicked()
                        {
                            import_trigger = true;
                        }
                    });
                });

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save_prefs = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_prefs = true;
                    }
                });
            });

        if save_prefs {
            match save_data_dir(&self.settings.path, &self.prefs_data_dir) {
                Ok(state) => {
                    self.settings = state;
                    self.prefs_data_dir = self.settings.configured.clone().unwrap_or_default();
                    self.use_settings_data_dir();
                    self.status = Status::new(
                        Level::Info,
                        format!(
                            "Data dir: {}",
                            self.settings.data_dir.as_deref().map(|d| d.display().to_string()).unwrap_or_default()
                        ),
                    );
                    prefs_open = false;
                }
                Err(e) => self.status = Status::new(Level::Error, format!("Settings not saved: {e}")),
            }
        }
        if reload_prefs {
            self.settings = load_settings_state(&self.settings.path);
            self.prefs_data_dir = self.settings.configured.clone().unwrap_or_default();
            self.use_settings_data_dir();
            self.status = settings_error_status(&self.settings).unwrap_or_else(|| Status::new(Level::Info, "Settings reloaded."));
        }
        if cancel_prefs {
            self.prefs_data_dir = self.settings.configured.clone().unwrap_or_default();
            prefs_open = false;
        }
        self.prefs_open = prefs_open;

        // File dialogs for export/import — open native dialogs, then dispatch
        // to the worker so the UI shows progress during the zip operation.
        if export_trigger {
            if let Some(dest) = rfd::FileDialog::new()
                .set_file_name("fleet-export.zip")
                .add_filter("Zip archive", &["zip"])
                .save_file()
            {
                self.send(Cmd::Export { dest });
            }
        }
        if import_trigger {
            if let Some(src) = rfd::FileDialog::new().add_filter("Zip archive", &["zip"]).pick_file() {
                self.send(Cmd::Import { src });
            }
        }
    }

    fn ui_vendor_ops(&mut self, ctx: &egui::Context) {
        // Raw USB PTP vendor operations reverse-engineered from an NX Field
        // WiFi capture (docs/nx-field-session-2026-07-09.md) — bypass the
        // MAID SDK entirely. Unlike the rest of this GUI these don't need a
        // prior Discover: they talk straight to the USB device and only need
        // a serial when more than one Nikon camera is attached. They run as
        // `nikon-fleet` CLI subprocesses, not in this process — see
        // `do_vendor_read`.
        let mut vendor_ops_open = self.vendor_ops_open;
        let mut read_trigger = false;
        let mut preview_trigger = false;
        let mut write_request = false;
        let mut confirm_write = false;
        let mut cancel_write = false;
        let busy = self.busy();
        let target = match self.selected_camera() {
            Some(c) => format!("Target: {} {}", c.model, c.serial),
            None => "Target: (auto-detected — only one Nikon camera on USB)".to_string(),
        };

        egui::Window::new("Vendor Ops (raw USB PTP)")
            .open(&mut vendor_ops_open)
            .resizable(false)
            .collapsible(false)
            .min_width(420.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(target).small().weak());
                ui.add_space(6.0);

                ui.group(|ui| {
                    ui.label(egui::RichText::new("Read Vendor Property (0x943B)").strong());
                    ui.horizontal(|ui| {
                        ui.label("Property code:");
                        ui.add(egui::TextEdit::singleline(&mut self.vendor_propcode).desired_width(100.0));
                        ui.add_enabled_ui(!busy, |ui| {
                            if ui.button("Read").clicked() {
                                read_trigger = true;
                            }
                        });
                    });
                    ui.add(
                        egui::TextEdit::multiline(&mut self.vendor_read_result)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .interactive(false)
                            .font(egui::TextStyle::Monospace),
                    );
                });

                ui.add_space(8.0);

                ui.group(|ui| {
                    ui.label(egui::RichText::new("Write FTP Profile (0x90EE)").strong());
                    let form = &mut self.ftp_form;
                    egui::Grid::new("vendor_ftp_grid").num_columns(2).spacing([6.0, 4.0]).show(ui, |ui| {
                        ui.label("Profile name:");
                        ui.text_edit_singleline(&mut form.profile_name);
                        ui.end_row();
                        ui.label("SSID (2.4GHz):");
                        ui.text_edit_singleline(&mut form.ssid_24ghz);
                        ui.end_row();
                        ui.label("SSID (5GHz):");
                        ui.text_edit_singleline(&mut form.ssid_5ghz);
                        ui.end_row();
                        ui.label("FTP host:");
                        ui.text_edit_singleline(&mut form.host);
                        ui.end_row();
                        ui.label("Port:");
                        ui.text_edit_singleline(&mut form.port);
                        ui.end_row();
                        ui.label("Username:");
                        ui.text_edit_singleline(&mut form.username);
                        ui.end_row();
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut form.password).password(true));
                        ui.end_row();
                    });

                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Preview (dry run)").clicked() {
                                preview_trigger = true;
                            }
                            if self.pending_write.is_none() && ui.button("Write to Camera…").clicked() {
                                write_request = true;
                            }
                        });
                    });

                    if let Some(pending) = &self.pending_write {
                        ui.add_space(4.0);
                        ui.colored_label(egui::Color32::from_rgb(200, 120, 0), pending.confirm_text());
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button("Confirm Write").clicked() {
                                    confirm_write = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    cancel_write = true;
                                }
                            });
                        });
                    }

                    ui.add(
                        egui::TextEdit::multiline(&mut self.vendor_write_result)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .interactive(false)
                            .font(egui::TextStyle::Monospace),
                    );
                });
            });
        self.vendor_ops_open = vendor_ops_open;

        if read_trigger {
            match ptp_usb::parse_propcode(&self.vendor_propcode) {
                Ok(propcode) => {
                    let serial = self.selected_serial();
                    self.send(Cmd::VendorRead { propcode, serial });
                }
                Err(e) => self.vendor_read_result = format!("Invalid property code: {e}"),
            }
        }
        if preview_trigger {
            self.pending_write = None;
            self.vendor_write_result = match prepare_ftp(&self.ftp_form) {
                Ok((_, preview)) => preview,
                Err(e) => e,
            };
        }
        if write_request {
            match prepare_ftp(&self.ftp_form) {
                Ok((profile, _)) => {
                    self.vendor_write_result.clear();
                    self.pending_write = Some(PendingWrite::new(profile, &self.ftp_form, self.selected_camera()));
                }
                Err(e) => self.vendor_write_result = e,
            }
        }
        if cancel_write {
            self.pending_write = None;
            self.vendor_write_result = "Cancelled. Nothing was written.".into();
        }
        if confirm_write {
            // The payload and the target are the ones the confirmation text named,
            // not whatever the form and the selection say now.
            if let Some(p) = self.pending_write.take() {
                self.vendor_write_result = "Writing…".into();
                self.send(Cmd::VendorWriteFtp { profile: p.profile, serial: p.target.map(|t| t.serial) });
            }
        }
    }

    fn ui_sidebar(&mut self, ctx: &egui::Context) {
        let mut cam_select: Option<usize> = None;
        let mut stored_select: Option<(String, String)> = None;
        let stored = self.listing.as_ref().map(|l| l.stored_only_bodies(&self.cameras)).unwrap_or_default();
        egui::SidePanel::left("cameras").min_width(180.0).show(ctx, |ui| {
            ui.heading("Cameras");
            ui.separator();
            if self.cameras.is_empty() {
                ui.label(egui::RichText::new("No cameras").italics().weak());
            }
            for (i, cam) in self.cameras.iter().enumerate() {
                let mut text = format!("{}\n{}", cam.model, cam.serial);
                if cam.pairing_ambiguous() {
                    text.push_str("\n⚠ same model as another body");
                }
                if ui.selectable_label(self.selected == Some(i), text).clicked() {
                    cam_select = Some(i);
                }
            }
            if !stored.is_empty() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Stored, not connected").weak());
                ui.separator();
                for body in &stored {
                    let picked = self.selected.is_none() && self.selected_stored.as_ref() == Some(body);
                    if ui.selectable_label(picked, format!("{}\n{}", body.0, body.1)).clicked() {
                        stored_select = Some(body.clone());
                    }
                }
            }
        });
        if let Some(i) = cam_select {
            self.label = label_after_selection(self.selected, i, std::mem::take(&mut self.label));
            self.selected = Some(i);
            self.selected_stored = None;
        }
        if let Some(body) = stored_select {
            // A label typed for a connected body must not stay on a screen where nothing can be captured.
            self.label.clear();
            self.selected = None;
            self.selected_stored = Some(body);
        }
    }

    fn ui_main(&mut self, ctx: &egui::Context) {
        let mut snap_trigger = false;
        let mut check_trigger = false;
        let mut ref_action: Option<String> = None;
        let mut view_action: Option<(String, bool, bool)> = None;
        let mut transplant_from: Option<((String, String), String)> = None;
        let mut restore_from: Option<((String, String), String)> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::CentralPanel::default().show(ctx, |ui| {
            let connected = self.selected.and_then(|i| self.cameras.get(i));
            let stored = if connected.is_none() { self.selected_stored.clone() } else { None };
            let Some((model, serial)) =
                connected.map(|c| (c.model.clone(), c.serial.clone())).or(stored)
            else {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Select a camera, or a stored body, to see snapshots").italics().weak());
                });
                return;
            };

            if let Some(cam) = connected {
                ui.horizontal(|ui| {
                    ui.heading(&cam.model);
                    ui.label(egui::RichText::new(format!("{}  fw {}", cam.serial, cam.firmware)).weak());
                });
                if !cam.known_model {
                    ui.colored_label(egui::Color32::from_rgb(200, 120, 0), "Not one of the surveyed models.");
                } else if !cam.baseline {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 0),
                        "No capture baseline for this model: a snapshot is saved without a completeness check.",
                    );
                }
                if cam.pairing_ambiguous() {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 60, 60),
                        AmbiguousPairing { model: cam.model.clone(), count: cam.model_count }.to_string(),
                    );
                }
                ui.separator();

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy && data_ready && !cam.pairing_ambiguous(), |ui| {
                        ui.label("Label:");
                        ui.text_edit_singleline(&mut self.label);
                        if ui.button("Take Snapshot").clicked() {
                            snap_trigger = true;
                        }
                    });
                    if ui
                        .button("Check against reference…")
                        .on_hover_text("Read this body and list how it differs from its reference snapshot (writes nothing)")
                        .clicked()
                    {
                        check_trigger = true;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.heading(&model);
                    ui.label(egui::RichText::new(&serial).weak());
                });
                ui.label(egui::RichText::new("Not connected: its stored snapshots can be viewed and one set as the reference.").weak());
            }
            ui.separator();

            let Some(listing) = &self.listing else {
                ui.label(egui::RichText::new("Snapshots are not loaded.").italics().weak());
                return;
            };
            for p in listing.problems() {
                ui.colored_label(egui::Color32::from_rgb(200, 120, 0), p);
            }
            if !listing.unreadable.is_empty() {
                egui::CollapsingHeader::new(format!("{} unreadable file(s)", listing.unreadable.len())).show(ui, |ui| {
                    for u in &listing.unreadable {
                        ui.label(format!("{}  ({}: {})", u.file, u.error_kind, u.error));
                    }
                });
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for snap in listing.rows_for(&model, &serial) {
                    ui.horizontal(|ui| {
                        let ts = snap.captured_at.get(..19).unwrap_or(&snap.captured_at);
                        let lbl = snap.label.as_deref().unwrap_or("(no label)");
                        let text = ui
                            .add(egui::Label::new(format!("{ts}  {lbl}")).sense(egui::Sense::click()))
                            .on_hover_text("Double-click to see what this snapshot holds");
                        if text.double_clicked() {
                            view_action = Some((snap.file.clone(), snap.is_reference, snap.name_lossy));
                        }
                        if snap.is_reference {
                            ui.label(egui::RichText::new("◀ ref").color(egui::Color32::LIGHT_GREEN));
                        } else {
                            ui.add_enabled_ui(!busy && !snap.name_lossy, |ui| {
                                if ui.small_button("Set ref").clicked() {
                                    ref_action = Some(snap.file.clone());
                                }
                            });
                        }
                        ui.add_enabled_ui(!snap.name_lossy, |ui| {
                            if ui.small_button("View").clicked() {
                                view_action = Some((snap.file.clone(), snap.is_reference, snap.name_lossy));
                            }
                        });
                        ui.add_enabled_ui(!busy && !snap.name_lossy, |ui| {
                            if ui
                                .small_button("Restore…")
                                .on_hover_text("Put this snapshot's settings back on its own body (you will see a plan first)")
                                .clicked()
                            {
                                restore_from = Some(((snap.model.clone(), snap.serial.clone()), snap.file.clone()));
                            }
                        });
                        ui.add_enabled_ui(!busy && !snap.name_lossy, |ui| {
                            if ui
                                .small_button("Transplant…")
                                .on_hover_text("Copy this snapshot's settings onto a connected body (you will see a plan first)")
                                .clicked()
                            {
                                transplant_from = Some(((snap.model.clone(), snap.serial.clone()), snap.file.clone()));
                            }
                        });
                    });
                }
            });
        });

        if snap_trigger {
            if let Some(camera) = self.selected_camera().cloned() {
                let label = self.label.trim().to_string();
                self.send(Cmd::Snapshot { camera, label });
            }
        }
        if check_trigger {
            self.start_check_for_selected();
        }
        if let Some(file) = ref_action {
            self.send(Cmd::SetReference { file });
        }
        if let Some((file, is_reference, name_lossy)) = view_action {
            self.open_snapshot(ctx, &file, is_reference, name_lossy);
        }
        if let Some((body, file)) = transplant_from {
            self.start_transplant_from(body, file);
        }
        if let Some((body, file)) = restore_from {
            self.start_restore_from(body, file);
        }
    }

    /// One window per open snapshot: a filter box, a count line and the rows in a scrollable
    /// monospace grid. Problems are orange, placeholders and cut values dim. Each is its own native
    /// OS window (movable outside the main one, onto another monitor); where the integration cannot
    /// make native windows it is a resizable egui window inside the main one.
    fn ui_snapshot_viewers(&mut self, ctx: &egui::Context) {
        if let Some(listing) = &self.listing {
            self.viewers.sync_reference(listing);
        }
        let embedded = ctx.embed_viewports();
        let mut requests = Vec::new();
        for viewer in &mut self.viewers.open {
            let requested = if embedded {
                let mut open = true;
                egui::Window::new(viewer.title())
                    .id(egui::Id::new(("snapshot_viewer", &viewer.file)))
                    .open(&mut open)
                    .resizable(true)
                    .default_size([720.0, 480.0])
                    .show(ctx, |ui| draw_snapshot_viewer(ui, viewer));
                !open
            } else {
                let builder = egui::ViewportBuilder::default()
                    .with_title(viewer.title())
                    .with_inner_size([760.0, 600.0])
                    .with_min_inner_size([420.0, 240.0]);
                ctx.show_viewport_immediate(viewer_viewport_id(&viewer.file), builder, |ui, class| {
                    if class == egui::ViewportClass::EmbeddedWindow {
                        // Not reachable under eframe (`embed_viewports` was false above); draw the
                        // contents rather than nothing.
                        draw_snapshot_viewer(ui, viewer);
                        false
                    } else {
                        egui::CentralPanel::default().show_inside(ui, |ui| draw_snapshot_viewer(ui, viewer));
                        ui.ctx().input(|i| i.viewport().close_requested())
                    }
                })
            };
            requests.push((viewer.file.clone(), requested));
        }
        self.viewers.close_requested(requests);
    }
}

/// The native window's identity: stable for a file, so a viewer keeps its window (and its place on
/// screen) from frame to frame, and different for each file.
fn viewer_viewport_id(file: &str) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("snapshot_viewer", file))
}

fn draw_snapshot_viewer(ui: &mut egui::Ui, viewer: &mut SnapshotViewer) {
    if viewer.loading {
        ui.label(egui::RichText::new("Reading the file…").italics().weak());
        return;
    }
    if let Some(e) = &viewer.error {
        ui.colored_label(egui::Color32::from_rgb(200, 60, 60), format!("{} cannot be shown: {e}", viewer.file));
        return;
    }
    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.add(egui::TextEdit::singleline(&mut viewer.filter).hint_text("name, value or code"));
        if ui.small_button("Clear").clicked() {
            viewer.filter.clear();
        }
        ui.checkbox(&mut viewer.show_codes, "Show codes");
    });
    if let Some(line) = viewer.count_line() {
        ui.label(egui::RichText::new(line).weak());
    }
    if let Some(note) = &viewer.labels_note {
        ui.label(egui::RichText::new(format!("Names: {note}")).small().color(egui::Color32::from_rgb(200, 120, 0)));
    }
    ui.separator();
    let warn = egui::Color32::from_rgb(200, 120, 0);
    let show_codes = viewer.show_codes;
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new(("snapshot_rows", &viewer.file)).striped(true).show(ui, |ui| {
            for row in viewer.visible_rows() {
                ui.label(egui::RichText::new(row.short_name()).monospace());
                ui.label(egui::RichText::new(row.extra_display_name().unwrap_or("")));
                if show_codes {
                    ui.label(egui::RichText::new(row.code_hex()).monospace().weak());
                }
                let text = egui::RichText::new(&row.value.display).monospace();
                let text = if row.value.is_problem() {
                    text.color(warn)
                } else if row.value.notes.is_empty() {
                    text
                } else {
                    text.weak()
                };
                let shown = ui.label(text);
                if !row.value.notes.is_empty() {
                    let tags: Vec<&str> = row.value.notes.iter().map(|n| n.tag()).collect();
                    shown.on_hover_text(tags.join(", "));
                }
                ui.end_row();
            }
        });
    });
}

// ── Worker thread ─────────────────────────────────────────────────────────

fn worker(rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Evt>, ctx: egui::Context, initial_data_dir: Result<PathBuf, String>) {
    let mut data_dir = initial_data_dir;
    for cmd in rx {
        let evt = match cmd {
            Cmd::SetDataDir(dir) => {
                data_dir = dir;
                continue;
            }
            Cmd::Discover => do_discover(&list_cameras),
            Cmd::Snapshot { camera, label } => with_data_dir(&data_dir, |d| do_snapshot(d, &camera, &label, &list_cameras)),
            Cmd::RefreshListing => with_data_dir(&data_dir, |d| Evt::Listing(list_all(d))),
            Cmd::SetReference { file } => with_data_dir(&data_dir, |d| match set_reference(d, &file) {
                Ok(report) => Evt::Done(describe_reference(&report)),
                Err(e) => Evt::Err(e),
            }),
            Cmd::Export { dest } => with_data_dir(&data_dir, |d| match do_export(d, &dest) {
                Ok(report) => Evt::Done(describe_export(&report)),
                Err(e) => Evt::Err(e),
            }),
            Cmd::Import { src } => with_data_dir(&data_dir, |d| match do_import(d, &src) {
                Ok(report) => Evt::Done(describe_import(&report)),
                Err(e) => Evt::Err(e),
            }),
            Cmd::VendorRead { propcode, serial } => {
                Evt::Vendor { op: VendorOp::Read, result: do_vendor_read(propcode, serial.as_deref()) }
            }
            Cmd::VendorWriteFtp { profile, serial } => {
                Evt::Vendor { op: VendorOp::WriteFtp, result: do_vendor_write_ftp(&profile, serial.as_deref()) }
            }
            Cmd::TransplantPlan { request } => match data_dir.as_ref() {
                Ok(d) => {
                    let result = do_transplant_plan(d, &request);
                    Evt::TransplantPlanned { request, result }
                }
                Err(e) => Evt::TransplantPlanned { request, result: Err(e.clone()) },
            },
            Cmd::RestorePlan { request } => match data_dir.as_ref() {
                Ok(d) => {
                    let result = restore::do_restore_plan(d, &request);
                    Evt::RestorePlanned { request, result }
                }
                Err(e) => Evt::RestorePlanned { request, result: Err(e.clone()) },
            },
            Cmd::Check { request } => match data_dir.as_ref() {
                Ok(d) => {
                    let result = check::do_check(d, &request);
                    Evt::Checked { request, result }
                }
                Err(e) => Evt::Checked { request, result: Err(e.clone()) },
            },
            Cmd::RestoreApply { request, digest, summary } => match data_dir.as_ref() {
                Ok(d) => {
                    let outcome = restore::do_restore_apply(d, &request, &digest, &summary);
                    Evt::RestoreApplied { request, outcome }
                }
                Err(message) => Evt::RestoreApplied {
                    request,
                    outcome: ApplyOutcome::NotApplied {
                        why: NotAppliedWhy::Refused,
                        message: message.clone(),
                        cli_log: String::new(),
                    },
                },
            },
            Cmd::TransplantApply { request, digest, summary } => match data_dir.as_ref() {
                Ok(d) => {
                    let outcome = do_transplant_apply(d, &request, &digest, &summary);
                    Evt::TransplantApplied { request, outcome }
                }
                Err(message) => Evt::TransplantApplied {
                    request,
                    outcome: ApplyOutcome::NotApplied {
                        why: NotAppliedWhy::Refused,
                        message: message.clone(),
                        cli_log: String::new(),
                    },
                },
            },
        };
        // The receiver is gone only when the window closed; there is no one to tell.
        let _ = tx.send(evt);
        ctx.request_repaint();
    }
}

fn with_data_dir(data_dir: &Result<PathBuf, String>, f: impl FnOnce(&Path) -> Evt) -> Evt {
    match data_dir {
        Ok(d) => f(d),
        Err(e) => Evt::Err(e.clone()),
    }
}

// ── Running the CLI ───────────────────────────────────────────────────────

/// Env var that overrides where the `nikon-fleet` CLI is found.
const CLI_ENV: &str = "NIKON_FLEET_CLI";
const CLI_NAME: &str = "nikon-fleet";

/// How long a command that talks to a camera may run before it is killed. A camera
/// session takes 4-20 s; the SDK's close hangs on about 1 run in 14 (F24), and a
/// worker with no limit would wait for that forever.
const CAMERA_CLI_TIMEOUT: Duration = Duration::from_secs(90);

/// Where to find the CLI binary: `$NIKON_FLEET_CLI` if set, else
/// `nikon-fleet` next to this executable (a `cargo build --workspace` puts
/// both in the same target directory; on Windows the file is `nikon-fleet.exe`, so `exe_suffix` is
/// `std::env::consts::EXE_SUFFIX`). An override that points nowhere is an
/// error — it is not silently ignored in favour of the sibling, which would
/// hide a typo'd path.
fn resolve_cli(
    env_override: Option<OsString>,
    current_exe: Option<&Path>,
    exe_suffix: &str,
    is_file: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    if let Some(v) = env_override {
        let p = PathBuf::from(v);
        return if is_file(&p) {
            Ok(p)
        } else {
            Err(format!("{CLI_ENV} is set to {} but no file exists there", p.display()))
        };
    }
    let exe = current_exe.ok_or_else(|| {
        format!("could not determine fleet-gui's own location to find the {CLI_NAME} CLI; set {CLI_ENV}")
    })?;
    let sibling = exe.with_file_name(format!("{CLI_NAME}{exe_suffix}"));
    if is_file(&sibling) {
        Ok(sibling)
    } else {
        Err(format!(
            "the {CLI_NAME} CLI was not found at {} — camera operations run it as a subprocess. \
             Build it with `cargo build --workspace` (building only fleet-gui does not build it), \
             or set {CLI_ENV} to its path",
            sibling.display()
        ))
    }
}

fn locate_cli() -> Result<PathBuf, String> {
    resolve_cli(std::env::var_os(CLI_ENV), std::env::current_exe().ok().as_deref(), std::env::consts::EXE_SUFFIX, Path::is_file)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliOutput {
    stdout: String,
    /// Not an error: a successful run's warnings (an unconfirmed model, a skipped body).
    stderr: String,
}

/// Why a CLI run did not succeed. Closed: callers branch on `kind`, never on the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliFailure {
    /// The program could not be started.
    Spawn,
    /// It ran and exited non-zero (or by signal).
    Exit,
    /// Its input could not be delivered, even though it exited 0.
    StdinWrite,
    /// It was killed after the time limit.
    TimedOut,
    /// Waiting for it failed; it was killed.
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliError {
    kind: CliFailure,
    /// For a human: the child's own stderr when it said anything, else what happened.
    message: String,
    /// What the child had printed before it ended (a hung run may have printed a result).
    stdout: String,
    stderr: String,
    /// The child's exit status when it ended by exiting (`None`: never ran, killed, or by signal).
    code: Option<i32>,
}

/// `nikon-fleet vendor-write-ftp` exits with this when the profile data was sent and the write
/// then failed (`PtpUsbError::FtpWriteIndeterminate`): the camera may have changed. Documented
/// under "Exit status:" in its `--help`; callers test the code, never the stderr text.
const EXIT_FTP_WRITE_INDETERMINATE: i32 = 4;

impl CliError {
    /// A camera write that ended this way may or may not have reached the camera.
    fn outcome_unknown(&self) -> bool {
        matches!(self.kind, CliFailure::TimedOut | CliFailure::Wait)
    }

    /// For a real `vendor-write-ftp`: the camera may have changed. Beyond the cases above, only
    /// the CLI's own definitive refusals (exit 1: an error before or without sending the data;
    /// exit 2: a usage error) mean it did not. A child that died by signal (no exit code), panicked
    /// (101) or exited with any other status may have done so after the data went out.
    fn write_outcome_unknown(&self) -> bool {
        self.outcome_unknown()
            || self.kind == CliFailure::StdinWrite
            || (self.kind == CliFailure::Exit && !matches!(self.code, Some(1) | Some(2)))
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Collect a pipe to its end on its own thread, so a full pipe can never block the child.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read error ends the capture with what arrived; the exit status carries the verdict.
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// How long to wait for a pipe's contents once the child has ended (a grandchild holding
/// the pipe open must not hang us).
const PIPE_GRACE: Duration = Duration::from_secs(5);

fn collect(rx: mpsc::Receiver<Vec<u8>>) -> String {
    String::from_utf8_lossy(&rx.recv_timeout(PIPE_GRACE).unwrap_or_default()).into_owned()
}

/// Run `program args…`, optionally piping `stdin_data` (then closing stdin), and return
/// its trimmed stdout and stderr. Success is the exit status alone — the text is only
/// ever shown to the user, never parsed for success. Failure is typed ([`CliFailure`]):
/// a non-zero exit (including death by signal) carries the child's stderr, or its exit
/// status if stderr was empty; a stdin write that failed fails the call even when the
/// child exited 0; a child still running after `timeout` is killed.
fn run_cli(program: &Path, args: &[String], stdin_data: Option<&str>, timeout: Duration) -> Result<CliOutput, CliError> {
    let fail = |kind, message: String, stdout: String, stderr: String| CliError { kind, message, stdout, stderr, code: None };
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd
        .spawn()
        .map_err(|e| fail(CliFailure::Spawn, format!("could not run {}: {e}", program.display()), String::new(), String::new()))?;

    // Written on its own thread: a child that never reads its input must not stop the
    // timeout from firing. The write's result is checked below.
    let stdin_thread = stdin_data.map(|data| {
        let mut stdin = child.stdin.take().expect("stdin was configured as piped");
        let data = data.as_bytes().to_vec();
        // `stdin` drops when the thread ends, closing the pipe so the child sees EOF.
        std::thread::spawn(move || stdin.write_all(&data))
    });
    let out_rx = drain(child.stdout.take().expect("stdout was configured as piped"));
    let err_rx = drain(child.stderr.take().expect("stderr was configured as piped"));

    let deadline = Instant::now() + timeout;
    let waited: Result<std::process::ExitStatus, (CliFailure, String)> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                // Ending the run either way; a failed kill leaves nothing more we can do.
                let _ = child.kill();
                let _ = child.wait();
                break Err((CliFailure::TimedOut, String::new()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err((CliFailure::Wait, e.to_string()));
            }
        }
    };
    let stdout = collect(out_rx);
    let stderr = collect(err_rx);
    // A panicked writer thread is as much a failed write as an error.
    let stdin_err = stdin_thread.map(|t| match t.join() {
        Ok(r) => r.err().map(|e| e.to_string()),
        Err(_) => Some("the input writer thread panicked".to_string()),
    });
    let stdin_err = stdin_err.flatten();

    let status = match waited {
        Ok(s) => s,
        Err((kind, detail)) => {
            let message = match kind {
                CliFailure::TimedOut => format!(
                    "{} did not finish within {} s and was stopped{}",
                    program.display(),
                    timeout.as_secs(),
                    if stderr.trim().is_empty() { String::new() } else { format!(": {}", stderr.trim()) }
                ),
                _ => format!("waiting for {} failed: {detail}; it was stopped", program.display()),
            };
            return Err(fail(kind, message, stdout.trim().to_string(), stderr.trim_end().to_string()));
        }
    };
    if status.success() {
        if let Some(e) = stdin_err {
            return Err(fail(
                CliFailure::StdinWrite,
                format!("{} exited 0 but could not be given its input ({e}); nothing it reports can be trusted", program.display()),
                stdout.trim().to_string(),
                stderr.trim_end().to_string(),
            ));
        }
        return Ok(CliOutput { stdout: stdout.trim().to_string(), stderr: stderr.trim_end().to_string() });
    }
    let detail = stderr.trim();
    let message = if !detail.is_empty() {
        detail.to_string()
    } else if let Some(e) = stdin_err {
        format!("{} exited with {status} (and could not be given its input: {e})", program.display())
    } else {
        format!("{} exited with {status}", program.display())
    };
    let mut err = fail(CliFailure::Exit, message, stdout.trim().to_string(), stderr.trim_end().to_string());
    err.code = status.code();
    Err(err)
}

// ── Camera operations (CLI subprocesses) ──────────────────────────────────

/// What `nikon-fleet discover --json` prints for one body (`ops::cameras::DiscoveredCamera`).
/// A key the library drops or renames fails the parse loudly instead of showing blanks.
#[derive(Debug, Deserialize)]
struct DiscoveredJson {
    model: String,
    serial: String,
    firmware: String,
    sdk_id: u32,
    known_model: bool,
    baseline: bool,
}

#[derive(Debug, Deserialize)]
struct DiscoverJson {
    cameras: Vec<DiscoveredJson>,
}

fn parse_discover(stdout: &str) -> Result<Vec<CameraRow>, String> {
    let parsed: DiscoverJson = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("`discover --json` printed something that is not the expected JSON ({e})"))?;
    // The library's grouping rule (`ops::cameras::duplicate_models`), the one the MCP refuses on.
    let counts = duplicate_models(parsed.cameras.iter().map(|c| c.model.as_str()));
    Ok(parsed
        .cameras
        .into_iter()
        .map(|c| {
            let model_count = counts.get(&c.model).copied().unwrap_or(1);
            CameraRow {
                // The CLI already reports the storable identity; an empty one must still
                // never reach a file name, so the library's own fallback (`id-N`) fills in.
                serial: if c.serial.is_empty() { storable_serial(&None, c.sdk_id) } else { c.serial },
                model: c.model,
                firmware: c.firmware,
                known_model: c.known_model,
                baseline: c.baseline,
                model_count,
            }
        })
        .collect())
}

/// What a plain look at the USB bus (no SDK session) adds to the CLI's list: bodies that
/// were dropped as unreadable, and same-model bodies whose serial pairing cannot be verified.
fn discover_notices(rows: &[CameraRow], scan: &CameraList) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(e) = &scan.enumeration_error {
        out.push(format!("The USB bus could not be enumerated ({e}); the camera list may be incomplete."));
    }
    for u in &scan.unreadable {
        out.push(format!(
            "A Nikon USB device (bus {}, address {}) is attached but could not be read ({:?}: {}); it is not in the list.",
            u.bus, u.address, u.stage, u.error
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows.iter().filter(|r| r.pairing_ambiguous()) {
        if !seen.contains(&row.model.as_str()) {
            seen.push(&row.model);
            out.push(AmbiguousPairing { model: row.model.clone(), count: row.model_count }.to_string());
        }
    }
    out
}

fn do_discover(scan: &dyn Fn() -> CameraList) -> Evt {
    match locate_cli().and_then(|cli| discover_via(&cli, scan, CAMERA_CLI_TIMEOUT)) {
        Ok(found) => Evt::Cameras(found),
        Err(e) => Evt::Err(e),
    }
}

fn discover_via(cli: &Path, scan: &dyn Fn() -> CameraList, timeout: Duration) -> Result<DiscoverResult, String> {
    let argv = vec!["discover".to_string(), "--json".to_string()];
    let mut notices = Vec::new();
    let (stdout, cli_log) = match run_cli(cli, &argv, None, timeout) {
        Ok(out) => (out.stdout, out.stderr),
        // The SDK's close can hang after the result is out (F24). A complete JSON document
        // is complete whether or not the process then exited, so it is taken, with a note.
        Err(e) if e.kind == CliFailure::TimedOut && parse_discover(&e.stdout).is_ok() => {
            notices.push(format!(
                "`discover` printed its result but did not exit within {} s (the SDK's close can hang); it was stopped.",
                timeout.as_secs()
            ));
            (e.stdout, e.stderr)
        }
        Err(e) => return Err(e.message),
    };
    let rows = parse_discover(&stdout)?;
    notices.extend(discover_notices(&rows, &scan()));
    Ok(DiscoverResult { rows, notices, cli_log })
}

/// Why a snapshot is refused before the camera is touched. The CLI only warns about
/// these cases; a snapshot stored under the wrong body's serial is worse than none.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotRefusal {
    /// More than one attached body has this model: serial and firmware are paired to SDK
    /// devices by enumeration order, which cannot be verified.
    AmbiguousPairing(AmbiguousPairing),
    /// Nikon devices are attached that could not be read, so they are missing from the
    /// list the pairing is made from.
    BodiesUnreadable { count: usize },
    /// The USB bus could not be enumerated at all.
    UsbEnumerationFailed(String),
}

impl SnapshotRefusal {
    /// Why the serial pairing cannot be trusted, for whichever operation was about to name a body
    /// by serial (a snapshot, a transplant): the reason alone, the caller says what was refused.
    fn reason(&self) -> String {
        match self {
            SnapshotRefusal::AmbiguousPairing(a) => a.to_string(),
            SnapshotRefusal::BodiesUnreadable { count } => format!(
                "{count} Nikon USB device(s) are attached but could not be read, so the serial pairing cannot be \
                 verified. Check the cables and that no other program holds the cameras, then Discover again."
            ),
            SnapshotRefusal::UsbEnumerationFailed(e) => {
                format!("the USB bus could not be enumerated ({e}), so the attached bodies are unknown.")
            }
        }
    }
}

impl std::fmt::Display for SnapshotRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Snapshot refused, nothing was read: {}", self.reason())
    }
}

/// The pre-flight for a snapshot: `scan` is a fresh USB-only look at the bus.
fn snapshot_refusal(camera: &CameraRow, scan: &CameraList) -> Option<SnapshotRefusal> {
    if let Some(e) = &scan.enumeration_error {
        return Some(SnapshotRefusal::UsbEnumerationFailed(e.clone()));
    }
    if !scan.unreadable.is_empty() {
        return Some(SnapshotRefusal::BodiesUnreadable { count: scan.unreadable.len() });
    }
    let on_bus = duplicate_models(scan.cameras.iter().map(|c| c.model.as_str()));
    let count = on_bus.get(&camera.model).copied().unwrap_or(0).max(camera.model_count);
    (count > 1).then(|| SnapshotRefusal::AmbiguousPairing(AmbiguousPairing { model: camera.model.clone(), count }))
}

/// The snapshot label after a click on camera `chosen`: kept when the click is on the camera that is
/// already selected, cleared when it is a different one (a label typed for one body must not end
/// up on another body's snapshot).
fn label_after_selection(previous: Option<usize>, chosen: usize, label: String) -> String {
    if previous == Some(chosen) {
        label
    } else {
        String::new()
    }
}

fn snapshot_argv(data_dir: &Path, serial: &str, label: &str) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    // `--flag=value` throughout: a serial or label starting with `-` stays attached to its flag.
    let mut argv = vec![format!("--data-dir={dir}"), "snapshot".into(), "--json".into(), format!("--serial={serial}")];
    if !label.is_empty() {
        argv.push(format!("--label={label}"));
    }
    Ok(argv)
}

fn do_snapshot(data_dir: &Path, camera: &CameraRow, label: &str, scan: &dyn Fn() -> CameraList) -> Evt {
    match locate_cli().and_then(|cli| snapshot_via(&cli, data_dir, camera, label, scan, CAMERA_CLI_TIMEOUT)) {
        Ok(done) => Evt::Done(done),
        Err(e) => Evt::Err(e),
    }
}

/// Capture through `nikon-fleet snapshot --json`: the one capture path (the shared capture
/// op, the baseline gate, the create-new write) with this GUI as a consumer of its verdict.
fn snapshot_via(
    cli: &Path,
    data_dir: &Path,
    camera: &CameraRow,
    label: &str,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> Result<OpDone, String> {
    if let Some(refusal) = snapshot_refusal(camera, &scan()) {
        return Err(refusal.to_string());
    }
    let argv = snapshot_argv(data_dir, &camera.serial, label)?;
    match run_cli(cli, &argv, None, timeout) {
        Ok(out) => describe_capture(&out.stdout, out.stderr),
        // The capture may have written its file before it failed or was stopped: say what
        // happened and refresh the list either way.
        Err(e) => {
            let unknown = if e.outcome_unknown() { " A snapshot file may or may not have been written." } else { "" };
            Ok(OpDone {
                status: Status::new(Level::Error, format!("Snapshot failed: {}{unknown}", e.message)),
                notices: Vec::new(),
                cli_log: e.stderr,
                refresh_listing: true,
            })
        }
    }
}

/// `nikon-fleet snapshot --json`'s value (`ops::capture_snapshot::CaptureOutcome`).
#[derive(Debug, Deserialize)]
struct CaptureJson {
    file: String,
    camera: CaptureCameraJson,
    stats: CaptureStatsJson,
    gate: GateJson,
    property_count: usize,
    expected_unreadable: Option<usize>,
    unexpected_unreadable: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CaptureCameraJson {
    model: String,
}

#[derive(Debug, Deserialize)]
struct CaptureStatsJson {
    read_ok: usize,
    read_err: usize,
    skipped_no_get: usize,
    unknown_schema: usize,
}

/// `ops::baseline::GateOutcome` as serialized: `outcome`, plus `reason` and its fields
/// for `no_baseline`, or `missing` for `failed`. Closed vocabularies: a new one fails the parse.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GateJson {
    outcome: GateKind,
    reason: Option<NoBaselineKind>,
    baseline: Option<String>,
    capture: Option<String>,
    missing: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GateKind {
    Passed,
    Failed,
    NoBaseline,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NoBaselineKind {
    UnknownModel,
    FirmwareChanged,
}

/// Turn a successful `snapshot --json` run into what the user is told: the file, the gate's
/// verdict (a capture that was not checked for completeness is a warning, never plain
/// "Saved"), and the capture's own statistics.
fn describe_capture(stdout: &str, stderr: String) -> Result<OpDone, String> {
    let c: CaptureJson = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "the snapshot command succeeded but its report could not be read ({e}); the snapshot was probably saved: \
             check the list"
        )
    })?;
    let stats = format!(
        "{} read, {} failed, {} without a get bit, {} not in the schema",
        c.stats.read_ok, c.stats.read_err, c.stats.skipped_no_get, c.stats.unknown_schema
    );
    let mut notices = Vec::new();
    if c.stats.unknown_schema > 0 {
        notices.push(format!(
            "{} capabilit{} not found in the schema and recorded by numeric code: the schema may be stale for this firmware.",
            c.stats.unknown_schema,
            if c.stats.unknown_schema == 1 { "y was" } else { "ies were" }
        ));
    }
    // The shared op refuses an empty capture before writing; if one is reported anyway it is
    // a failure, whatever the exit status said.
    if c.stats.read_ok == 0 {
        return Err(format!(
            "the camera read no capability at all ({} read error(s)); {} holds nothing useful. Retry, or unplug and replug the camera.",
            c.stats.read_err, c.file
        ));
    }
    let (level, verdict) = match (c.gate.outcome, c.gate.reason) {
        (GateKind::Passed, _) => {
            let unreadable = match (c.expected_unreadable, c.unexpected_unreadable) {
                (Some(e), Some(u)) if u > 0 => format!("{u} unexpected and {e} expected unreadable properties"),
                (Some(e), _) => format!("{e} expected unreadable properties"),
                _ => format!("{} unreadable properties", c.stats.read_err),
            };
            (Level::Info, format!("completeness check passed ({unreadable})"))
        }
        (GateKind::Failed, _) => (
            Level::Warn,
            format!(
                "INCOMPLETE: {} baseline capabilit(ies) could not be read",
                c.gate.missing.as_ref().map_or(0, Vec::len)
            ),
        ),
        (GateKind::NoBaseline, Some(NoBaselineKind::UnknownModel)) => {
            (Level::Warn, format!("NOT checked for completeness: no baseline exists for {}", c.camera.model))
        }
        (GateKind::NoBaseline, Some(NoBaselineKind::FirmwareChanged)) => (
            Level::Warn,
            format!(
                "NOT checked for completeness: firmware {} differs from the baseline's {}",
                c.gate.capture.as_deref().unwrap_or("?"),
                c.gate.baseline.as_deref().unwrap_or("?")
            ),
        ),
        (GateKind::NoBaseline, None) => (Level::Warn, "NOT checked for completeness (no reason given)".to_string()),
    };
    Ok(OpDone {
        status: Status::new(level, format!("Saved {}: {} properties; {verdict}. [{stats}]", c.file, c.property_count)),
        notices,
        cli_log: stderr,
        refresh_listing: true,
    })
}

fn do_vendor_read(propcode: u16, serial: Option<&str>) -> Result<VendorOut, String> {
    vendor_read_via(&locate_cli()?, propcode, serial, CAMERA_CLI_TIMEOUT)
}

/// Read a vendor property via `0x943B`, bypassing the MAID SDK entirely —
/// no Discover/connect needed first, unlike every other worker function
/// here. Runs `nikon-fleet vendor-read` as a **subprocess**, not
/// `ptp_usb::read_vendor_property` in-process.
///
/// Why (finding nikon-fleet-20260909-7b2e#12): in-process, this runs on the
/// GUI worker thread and, via `ptp_usb::claim_with_retry`, SIGKILLs
/// `ptpcamerad`/`icdd` and repeatedly re-asserts the USB configuration. The
/// codebase already had to fix a crash of the same shape (commit 48704d5:
/// USB device events reaching the AppKit run loop mid-frame invalidated the
/// Metal surface and segfaulted the GUI), and whether this path triggers it
/// was never verified. A subprocess takes the question away: the churn
/// happens outside this process, and if the child dies it comes back as an
/// error string. (`gui/fleet_gui.py` has always worked this way.)
fn vendor_read_via(cli: &Path, propcode: u16, serial: Option<&str>, timeout: Duration) -> Result<VendorOut, String> {
    let out = run_cli(cli, &vendor_read_argv(propcode, serial), None, timeout).map_err(|e| e.message)?;
    Ok(VendorOut { text: out.stdout, stderr: out.stderr })
}

/// Write an FTP profile via `0x90EE`. Same no-Discover-needed property as
/// `vendor_read_via`, same reason for the subprocess, with the password piped on stdin
/// (never in argv, where `ps` would show it).
///
/// No GUI control for `--force-unconfirmed-model` yet — never passed, so the GUI can only
/// write to models ptp_usb's confirmed-safe list covers.
fn do_vendor_write_ftp(profile: &FtpProfile, serial: Option<&str>) -> Result<VendorOut, String> {
    vendor_write_via(&locate_cli()?, profile, serial, CAMERA_CLI_TIMEOUT)
}

fn vendor_write_via(cli: &Path, profile: &FtpProfile, serial: Option<&str>, timeout: Duration) -> Result<VendorOut, String> {
    let out = run_cli(cli, &vendor_write_ftp_argv(profile, serial), Some(&profile.password), timeout)
        .map_err(|e| ftp_write_failure(&e))?;
    let text = if out.stdout.is_empty() {
        format!("The CLI reported success writing FTP profile {:?} but printed no confirmation.", profile.profile_name)
    } else {
        out.stdout
    };
    Ok(VendorOut { text, stderr: out.stderr })
}

/// A failed FTP write. When the run was cut off after the data may have gone out, or the
/// child took a truncated password, the camera may have changed: not a plain failure.
fn ftp_write_failure(e: &CliError) -> String {
    if e.kind == CliFailure::Exit && e.code == Some(EXIT_FTP_WRITE_INDETERMINATE) {
        format!(
            "The camera may have changed: check the FTP profile in the camera's menu (this tool cannot read it back). {}",
            e.message
        )
    } else if e.write_outcome_unknown() {
        format!(
            "{} The camera MAY have taken the profile (possibly an incomplete one): check its FTP settings before writing again.",
            e.message
        )
    } else {
        e.message.clone()
    }
}

// ── The FTP profile form ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct FtpForm {
    profile_name: String,
    ssid_24ghz: String,
    ssid_5ghz: String,
    host: String,
    port: String,
    username: String,
    password: String,
}

impl Default for FtpForm {
    fn default() -> Self {
        FtpForm {
            profile_name: String::new(),
            ssid_24ghz: String::new(),
            ssid_5ghz: String::new(),
            host: String::new(),
            port: "21".to_string(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl FtpForm {
    fn raw(&self, field: FtpField) -> &str {
        match field {
            FtpField::ProfileName => &self.profile_name,
            FtpField::Ssid24Ghz => &self.ssid_24ghz,
            FtpField::Ssid5Ghz => &self.ssid_5ghz,
            FtpField::Host => &self.host,
            FtpField::Username => &self.username,
            FtpField::Password => &self.password,
        }
    }

    /// The value as sent: surrounding whitespace is dropped exactly for the fields
    /// `ptp_usb::FTP_PROFILE_FIELDS` says are checked trimmed (the password never is — a
    /// leading or trailing space could be real there). One table decides both the
    /// required-field check and what lands on the camera.
    fn value(&self, field: FtpField) -> String {
        let raw = self.raw(field);
        if field.spec().trimmed_for_required_check { raw.trim().to_string() } else { raw.to_string() }
    }

    fn build(&self) -> Result<FtpProfile, String> {
        let missing = ptp_usb::ftp_profile_missing_fields(
            self.raw(FtpField::ProfileName),
            self.raw(FtpField::Ssid24Ghz),
            self.raw(FtpField::Ssid5Ghz),
            self.raw(FtpField::Host),
            self.raw(FtpField::Username),
            self.raw(FtpField::Password),
        );
        if !missing.is_empty() {
            return Err(format!("Missing required field(s): {}", missing.join(", ")));
        }
        let port = self.port.trim().parse::<u16>().map_err(|e| format!("Invalid port: {e}"))?;
        Ok(FtpProfile {
            profile_name: self.value(FtpField::ProfileName),
            ssid_24ghz: self.value(FtpField::Ssid24Ghz),
            ssid_5ghz: self.value(FtpField::Ssid5Ghz),
            host: self.value(FtpField::Host),
            port,
            username: self.value(FtpField::Username),
            password: self.value(FtpField::Password),
        })
    }
}

/// The one path from the form to a profile that can be previewed or written: the missing-field
/// rule, the trim rule, and the library's own encoder (`ptp_usb::encode_ftp_profile`, whose
/// field limits and character rules the real write applies too). The preview encodes a copy
/// with the password redacted — the hex dump would otherwise show it byte for byte — and that
/// copy encodes exactly when the real profile does, so a profile the camera write would reject
/// is rejected here, before anything is confirmed.
fn prepare_ftp(form: &FtpForm) -> Result<(FtpProfile, String), String> {
    let profile = form.build()?;
    let blob = ptp_usb::encode_ftp_profile(&profile.with_password_redacted()).map_err(|e| e.to_string())?;
    let preview = format!(
        "Would write {} byte(s) (password redacted in this preview): {}",
        blob.len(),
        ptp_usb::hex_bytes(&blob)
    );
    Ok((profile, preview))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteTarget {
    model: String,
    serial: String,
}

fn write_target(camera: Option<&CameraRow>) -> Option<WriteTarget> {
    camera.map(|c| WriteTarget { model: c.model.clone(), serial: c.serial.clone() })
}

/// A write the user has been asked to confirm. It holds the exact profile and target the
/// confirmation text names; "Confirm Write" sends these, not whatever the form and the
/// camera selection say by then, and any change to either withdraws the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingWrite {
    profile: FtpProfile,
    form: FtpForm,
    target: Option<WriteTarget>,
}

impl PendingWrite {
    fn new(profile: FtpProfile, form: &FtpForm, camera: Option<&CameraRow>) -> Self {
        PendingWrite { profile, form: form.clone(), target: write_target(camera) }
    }

    fn is_stale(&self, form: &FtpForm, camera: Option<&CameraRow>) -> bool {
        self.form != *form || self.target != write_target(camera)
    }

    fn confirm_text(&self) -> String {
        let p = &self.profile;
        let target = match &self.target {
            Some(t) => format!("{} (serial {})", t.model, t.serial),
            None => "the only Nikon camera on USB (none is selected here; the CLI refuses if several are attached)".into(),
        };
        format!(
            "Overwrite the FTP profile on {target} with {:?} ({}@{}:{})? This cannot be read back or undone from here.",
            p.profile_name, p.username, p.host, p.port
        )
    }
}


// ── Transplant: plan, confirm, apply ──────────────────────────────────────
//
// Copy a stored snapshot onto a connected body, which may be another model, translating enum and
// range values into the target's own equivalents. The translation is `nikon-fleet transplant`'s
// (`src/transplant.rs`); this is a consumer of it, run as a subprocess like every camera
// operation here, and it follows the MCP server's discipline (docs/mcp-plan.md, sections 5.4-5.5):
//
//   1. PLAN     `transplant --dry-run --json`: reads the body, writes nothing, returns a digest and
//               a one-line summary.
//   2. CONFIRM  the person is shown the plan and the summary, asks to apply, and confirms a text
//               that names the exact body. A property that is matched only by raw numeric code or
//               copied as a bare number (`raw_code_unconfirmed`, `literal_unconfirmed`) is never
//               written unless it is opted into by name, with a confirmation of its own, which
//               re-plans: the opt-in set is part of the digest.
//   3. APPLY    `transplant --json --expect-digest --expect-summary`: the CLI recomputes the plan
//               on the body as it is now and refuses (exit `EXIT_STALE_PLAN`, nothing written)
//               unless it is the plan that was confirmed. Then restore point, writes, read-back.
//
// Only `applied_verified` means the change took. Every other status is shown as what it is.

/// A plan reads every enum and range on the body (4-20 s a read); the MCP server allows the same.
const TRANSPLANT_PLAN_TIMEOUT: Duration = Duration::from_secs(150);
/// An apply adds a restore point and a read-back, each a full capture, to the writes.
const TRANSPLANT_APPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// The `reason.tag`s of a skip that is written only when its property is opted into by name
/// (`SkipReason::RawCodeUnconfirmed`, `SkipReason::LiteralUnconfirmed`). A test pins them to the
/// library's own tags.
const OPT_IN_TAGS: [&str; 2] = ["raw_code_unconfirmed", "literal_unconfirmed"];

/// What a transplant is asked for. These exact values reach the CLI: a plan is good for this
/// request only, and a change to any part of it withdraws the plan and any confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TransplantRequest {
    /// The stored body the source snapshot is of (model, serial), as the person chose it.
    source_body: (String, String),
    /// The source, a file name in `snapshots/`.
    source_file: String,
    /// The body written to, as discovered (a changed firmware or pairing is a different request).
    target: CameraRow,
    /// Full property names the person opted into by raw code, each confirmed on its own.
    allow_raw_code: BTreeSet<String>,
}

// The CLI's JSON, as `ops::plan::PlanReport` and `ops::apply::ApplyReport` serialise it. The
// closed vocabularies (`ApplyStatus`, `Severity`) are the library's own types; a field the library
// drops or renames fails the parse loudly instead of showing a blank.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct BodyJson {
    model: String,
    serial: String,
    firmware: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PlanJson {
    digest: String,
    summary: String,
    same_model: bool,
    source: BodyJson,
    source_label: Option<String>,
    target: BodyJson,
    allow_raw_code: Vec<String>,
    counts: PlanCountsJson,
    outcomes: Vec<OutcomeJson>,
    cannot_read: Vec<NotTransferableJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PlanCountsJson {
    copied: usize,
    would_change: usize,
    by_tag: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct NotTransferableJson {
    what: String,
    why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutcomeStatus {
    Copied,
    Skipped,
}

/// A key that is there, null or not: `Some(value)`, where serde's `default` gives `None` for a key
/// that is not.
fn key_present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct OutcomeJson {
    name: String,
    code: u32,
    status: OutcomeStatus,
    strategy: Option<String>,
    reason: Option<ReasonJson>,
    /// For a copied property with a planned write: whether it changes the target. `None` (the key
    /// absent) is no planned write at all; `Some(None)` (null) is a live value that could not be
    /// read, which counts as a possible change. The library's `ops::plan::planned_would_change`
    /// reads it, so this and a plan's `would_change` count cannot differ.
    #[serde(default, rename = "changes", deserialize_with = "key_present")]
    planned_changes: Option<Option<bool>>,
    choice: Option<ChoiceJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ReasonJson {
    tag: String,
    severity: Severity,
    detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ChoiceJson {
    target_index: u64,
    choice: String,
    current_index: Option<u64>,
    current: Option<String>,
    unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ApplyJson {
    status: ApplyStatus,
    no_op: bool,
    plan_digest: String,
    restore_point: Option<RestorePointJson>,
    restore_point_gate_waived: bool,
    written: Vec<String>,
    write_errors: Vec<WriteErrorJson>,
    rejected_by_kind: Vec<String>,
    outcomes: Vec<OutcomeJson>,
    readback: Option<ReadbackJson>,
    undo_set: Vec<String>,
    undo_not_restorable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RestorePointJson {
    file: String,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct WriteErrorJson {
    name: String,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ReadbackJson {
    /// The read-back held to the baseline gate; `None` when nothing at all could be read.
    gate: Option<GateJson>,
    not_taken: Vec<String>,
    unverifiable: Vec<String>,
    side_effects: Option<SideEffectsJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct SideEffectsJson {
    changed: Vec<String>,
    vanished: Vec<String>,
    appeared: Vec<String>,
}

/// The name a person knows a property by.
fn short_name(full: &str) -> &str {
    full.strip_prefix(PROPERTY_PREFIX).unwrap_or(full)
}

fn names_list(names: &[String]) -> String {
    names.iter().map(|n| short_name(n)).collect::<Vec<_>>().join(", ")
}

/// A skip's detail object as `key: value` pairs, for a person. Display only: nothing branches on it.
fn detail_text(detail: &Option<serde_json::Value>) -> String {
    let Some(serde_json::Value::Object(map)) = detail else {
        return String::new();
    };
    map.iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{}: {s}", k.replace('_', " ")),
            other => format!("{}: {other}", k.replace('_', " ")),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl OutcomeJson {
    fn tag(&self) -> Option<&str> {
        self.reason.as_ref().map(|r| r.tag.as_str())
    }

    /// A skip that is written only if its property is opted into by name.
    fn needs_opt_in(&self) -> bool {
        self.status == OutcomeStatus::Skipped && self.tag().is_some_and(|t| OPT_IN_TAGS.contains(&t))
    }

    /// A copied property that would, or might, change the target (the library's `would_change`).
    fn would_change(&self) -> bool {
        self.status == OutcomeStatus::Copied && planned_would_change(self.planned_changes)
    }

    /// The value a skip proposed, as the library worded it (`code 5 (#2), was code 3 (#1)`).
    fn proposed(&self) -> Option<&str> {
        self.reason.as_ref()?.detail.as_ref()?.get("proposed")?.as_str()
    }

    /// One line for a copied or skipped property.
    fn line(&self) -> String {
        let name = short_name(&self.name);
        match (&self.status, &self.reason) {
            (OutcomeStatus::Copied, _) => {
                let strategy = self.strategy.as_deref().unwrap_or("?").replace('_', " ");
                let change = match (&self.choice, self.planned_changes) {
                    (Some(c), _) => {
                        let mut s = format!("{:?} (#{})", c.choice, c.target_index);
                        if c.unchanged {
                            s.push_str(", unchanged");
                        } else if let (Some(cur), Some(i)) = (&c.current, c.current_index) {
                            s.push_str(&format!(", was {cur:?} (#{i})"));
                        } else {
                            s.push_str(", current value unknown");
                        }
                        format!(": {s}")
                    }
                    (None, Some(None)) => ": current value unknown, so it may change".to_string(),
                    (None, _) => String::new(),
                };
                format!("{name}  [{strategy}]{change}")
            }
            (OutcomeStatus::Skipped, Some(r)) => {
                let detail = detail_text(&r.detail);
                let what = r.tag.replace('_', " ");
                if detail.is_empty() { format!("{name}: {what}") } else { format!("{name}: {what} ({detail})") }
            }
            (OutcomeStatus::Skipped, None) => format!("{name}: skipped"),
        }
    }
}

/// The plan's outcomes sorted into what the person must look at.
#[derive(Debug, Default, PartialEq, Eq)]
struct PlanSections<'a> {
    /// Copied, and changes the target or might.
    will_change: Vec<&'a OutcomeJson>,
    /// Copied, and the target already holds it.
    unchanged: Vec<&'a OutcomeJson>,
    /// Skipped until opted into by name.
    needs_decision: Vec<&'a OutcomeJson>,
    /// Skipped for a reason that wants attention (warn or error severity).
    attention: Vec<&'a OutcomeJson>,
    /// Skipped on purpose or as a matter of course, by tag.
    routine: BTreeMap<&'a str, Vec<&'a OutcomeJson>>,
}

fn plan_sections(outcomes: &[OutcomeJson]) -> PlanSections<'_> {
    let mut s = PlanSections::default();
    for o in outcomes {
        match (&o.status, &o.reason) {
            (OutcomeStatus::Copied, _) if o.would_change() => s.will_change.push(o),
            (OutcomeStatus::Copied, _) => s.unchanged.push(o),
            _ if o.needs_opt_in() => s.needs_decision.push(o),
            (OutcomeStatus::Skipped, Some(r)) if matches!(r.severity, Severity::Warn | Severity::Error) => s.attention.push(o),
            (OutcomeStatus::Skipped, Some(r)) => s.routine.entry(r.tag.as_str()).or_default().push(o),
            (OutcomeStatus::Skipped, None) => s.attention.push(o),
        }
    }
    s
}

/// A plan the CLI returned for `request`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedTransplant {
    request: TransplantRequest,
    plan: PlanJson,
    /// What each opted-in property is set to, as the plan that offered it worded the proposal (the
    /// re-made plan no longer carries the proposal). Shown in the apply confirmation.
    optin_values: BTreeMap<String, String>,
    cli_log: String,
}

/// Why a transplant is refused before the camera is touched, beyond what the CLI itself checks:
/// a write must be aimed at one body with certainty.
fn transplant_target_refusal(camera: &CameraRow) -> Option<String> {
    if is_id_fallback(&camera.serial) {
        return Some(format!(
            "{} reports no USB serial (it is known only as {}), so a write cannot be aimed at it with certainty.",
            camera.model, camera.serial
        ));
    }
    camera
        .pairing_ambiguous()
        .then(|| AmbiguousPairing { model: camera.model.clone(), count: camera.model_count }.to_string())
}

/// The full pre-flight: the target's own check, then a fresh USB-only look at the bus (the same
/// rule a snapshot uses to trust a serial pairing).
fn transplant_refusal(camera: &CameraRow, scan: &CameraList) -> Option<String> {
    let reason = transplant_target_refusal(camera).or_else(|| snapshot_refusal(camera, scan).map(|r| r.reason()))?;
    Some(format!("Transplant refused, nothing was read or written: {reason}"))
}

/// The CLI arguments for a plan (`confirmed` is `None`) or an apply (the digest and summary the
/// person confirmed). `--flag=value` throughout, so a value starting with `-` stays attached to
/// its flag, and `--` before the source so a file name cannot be taken for a flag.
fn transplant_argv(data_dir: &Path, request: &TransplantRequest, confirmed: Option<(&str, &str)>) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    let source = snapshot_path_for_cli(data_dir, &request.source_file)?;
    let mut argv = vec![
        format!("--data-dir={dir}"),
        "transplant".into(),
        "--json".into(),
        format!("--serial={}", request.target.serial),
    ];
    match confirmed {
        None => argv.push("--dry-run".into()),
        Some((digest, summary)) => {
            argv.push(format!("--expect-digest={digest}"));
            argv.push(format!("--expect-summary={summary}"));
        }
    }
    for name in &request.allow_raw_code {
        argv.push(format!("--allow-raw-code={name}"));
    }
    argv.push("--".into());
    argv.push(source);
    Ok(argv)
}

/// The snapshot as an absolute path. The CLI looks for a path that is not absolute in its own
/// working directory first, so a bare file name could be read from there instead of from the
/// `snapshots/` directory the person chose it from.
fn snapshot_path_for_cli(data_dir: &Path, file: &str) -> Result<String, String> {
    if !is_safe_path_component(file) {
        return Err(format!("{file:?} is not a plain snapshot file name"));
    }
    let joined = data_dir.join("snapshots").join(file);
    let absolute = std::path::absolute(&joined).map_err(|e| format!("the path of {file} could not be made absolute: {e}"))?;
    absolute.to_str().map(str::to_string).ok_or_else(|| format!("{} is not valid UTF-8", absolute.display()))
}

fn do_transplant_plan(data_dir: &Path, request: &TransplantRequest) -> Result<(PlanJson, String), String> {
    transplant_plan_via(&locate_cli()?, data_dir, request, &list_cameras, TRANSPLANT_PLAN_TIMEOUT)
}

/// Ask the CLI for a plan. Reads the body, writes nothing.
fn transplant_plan_via(
    cli: &Path,
    data_dir: &Path,
    request: &TransplantRequest,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> Result<(PlanJson, String), String> {
    if let Some(refusal) = transplant_refusal(&request.target, &scan()) {
        return Err(refusal);
    }
    let argv = transplant_argv(data_dir, request, None)?;
    let out = run_cli(cli, &argv, None, timeout).map_err(|e| format!("Planning failed: {}", e.message))?;
    let plan: PlanJson = serde_json::from_str(out.stdout.trim()).map_err(|e| {
        format!("the plan command succeeded but its report could not be read ({e}); nothing was written")
    })?;
    check_plan_is_for(&plan, request)?;
    Ok((plan, out.stderr))
}

/// A plan is shown and confirmed only if it is for the snapshot, the body (at the firmware it runs
/// now) and the opt-ins that were asked for.
fn check_plan_is_for(plan: &PlanJson, request: &TransplantRequest) -> Result<(), String> {
    if plan.target.serial != request.target.serial || plan.target.model != request.target.model {
        return Err(format!(
            "the plan is for {} {}, not for the {} {} that was asked for; nothing was written",
            plan.target.model, plan.target.serial, request.target.model, request.target.serial
        ));
    }
    if plan.target.firmware != request.target.firmware {
        return Err(format!(
            "the plan was made for firmware {} on {} {}, which is {} now; nothing was written",
            plan.target.firmware, plan.target.model, plan.target.serial, request.target.firmware
        ));
    }
    if (&plan.source.model, &plan.source.serial) != (&request.source_body.0, &request.source_body.1) {
        return Err(format!(
            "the plan is of a snapshot of {} {}, not of the {} {} that was chosen; nothing was written",
            plan.source.model, plan.source.serial, request.source_body.0, request.source_body.1
        ));
    }
    let planned: BTreeSet<&String> = plan.allow_raw_code.iter().collect();
    let asked: BTreeSet<&String> = request.allow_raw_code.iter().collect();
    if planned != asked {
        return Err("the plan does not carry the opt-ins that were asked for; nothing was written".to_string());
    }
    Ok(())
}

/// What an apply came to. The CLI's report decides; without one, the way the run ended says only
/// whether the camera was certainly not written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApplyOutcome<R = ApplyJson> {
    /// The CLI reported (the report's `status` is the verdict).
    Report { report: Box<R>, note: Option<String>, cli_log: String },
    /// Refused before any write; the camera is as it was.
    NotApplied { why: NotAppliedWhy, message: String, cli_log: String },
    /// No report and a run that may have written: the camera may have changed.
    Indeterminate { message: String, cli_log: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotAppliedWhy {
    /// The camera or snapshot no longer gives the plan that was confirmed.
    PlanStale,
    /// Any other refusal: the restore point could not be taken, the camera is busy, the body is gone.
    Refused,
}

fn do_transplant_apply(data_dir: &Path, request: &TransplantRequest, digest: &str, summary: &str) -> ApplyOutcome {
    match locate_cli() {
        Ok(cli) => {
            transplant_apply_via(&cli, data_dir, request, digest, summary, &list_cameras, TRANSPLANT_APPLY_TIMEOUT)
        }
        Err(message) => ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() },
    }
}

/// Apply exactly the plan that was confirmed: the CLI is given its digest and summary and refuses
/// if the body or the snapshot no longer give it.
fn transplant_apply_via(
    cli: &Path,
    data_dir: &Path,
    request: &TransplantRequest,
    digest: &str,
    summary: &str,
    scan: &dyn Fn() -> CameraList,
    timeout: Duration,
) -> ApplyOutcome {
    let refused = |message: String| ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, cli_log: String::new() };
    if let Some(refusal) = transplant_refusal(&request.target, &scan()) {
        return refused(refusal);
    }
    let argv = match transplant_argv(data_dir, request, Some((digest, summary))) {
        Ok(a) => a,
        Err(e) => return refused(e),
    };
    classify_apply(run_cli(cli, &argv, None, timeout), digest)
}

/// Read the way an apply ended. A report on stdout is the answer whatever the exit status was (a
/// failed or unverified apply exits non-zero after printing it, and a hung teardown can follow a
/// report). Without a report that reads: exit `EXIT_STALE_PLAN` is a refusal of this plan and exit
/// `EXIT_REFUSED_BEFORE_WRITE` is the CLI saying it refused before it could write; a CLI that could
/// not be started wrote nothing either. Anything else may have written, exit 1 included: the CLI
/// also ends 1 for an apply that wrote but did not fully take, and a report this program cannot
/// read (a newer CLI, a report cut off) looks the same as no report.
fn classify_apply(result: Result<CliOutput, CliError>, confirmed_digest: &str) -> ApplyOutcome {
    classify_apply_as::<ApplyJson>(result, confirmed_digest)
}

/// What an apply's JSON report says that the way it ended depends on, whichever command printed it.
trait AppliedReport: serde::de::DeserializeOwned {
    fn plan_digest(&self) -> &str;
    fn status(&self) -> ApplyStatus;
    fn restore_point(&self) -> Option<&RestorePointJson>;
    /// The properties an undo of this apply puts back.
    fn undo_set(&self) -> &[String];
}

impl AppliedReport for ApplyJson {
    fn plan_digest(&self) -> &str {
        &self.plan_digest
    }
    fn status(&self) -> ApplyStatus {
        self.status
    }
    fn restore_point(&self) -> Option<&RestorePointJson> {
        self.restore_point.as_ref()
    }
    fn undo_set(&self) -> &[String] {
        &self.undo_set
    }
}

fn classify_apply_as<R: AppliedReport>(result: Result<CliOutput, CliError>, confirmed_digest: &str) -> ApplyOutcome<R> {
    let report_of = |stdout: &str| serde_json::from_str::<R>(stdout.trim()).ok();
    let (stdout, stderr, failure) = match result {
        Ok(out) => (out.stdout, out.stderr, None),
        Err(e) => (e.stdout.clone(), e.stderr.clone(), Some(e)),
    };
    if let Some(report) = report_of(&stdout) {
        if report.plan_digest() != confirmed_digest {
            return ApplyOutcome::Indeterminate {
                message: "The command reported a different plan than the one that was confirmed, so what it wrote \
                          is not what you approved."
                    .to_string(),
                cli_log: stderr,
            };
        }
        let note = failure.filter(|e| e.kind != CliFailure::Exit).map(|e| {
            format!("The command was stopped after it reported ({}). The report is what it said before that.", e.message)
        });
        return ApplyOutcome::Report { report: Box::new(report), note, cli_log: stderr };
    }
    let Some(e) = failure else {
        return ApplyOutcome::Indeterminate {
            message: "The command exited 0 but its report could not be read, so what it wrote is unknown.".to_string(),
            cli_log: stderr,
        };
    };
    let unknown = |message: String| ApplyOutcome::Indeterminate { message, cli_log: stderr.clone() };
    match (e.kind, e.code) {
        (CliFailure::Exit, Some(code)) if code == EXIT_STALE_PLAN => {
            ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: e.message, cli_log: stderr }
        }
        (CliFailure::Exit, Some(code)) if code == EXIT_REFUSED_BEFORE_WRITE => {
            ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message: e.message, cli_log: stderr }
        }
        (CliFailure::Spawn, _) => {
            ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message: e.message, cli_log: stderr }
        }
        _ => unknown(e.message),
    }
}

// What the person is shown, as text with a level. Pure, so each is tested without a window.

/// The honest one-line verdict for an apply status. No wildcard: a new status must be worded here.
fn status_headline(status: ApplyStatus) -> (Level, &'static str) {
    match status {
        ApplyStatus::AppliedVerified => (Level::Info, "Applied and verified: every setting written reads back as planned."),
        ApplyStatus::AppliedUnverified => (
            Level::Warn,
            "Applied, NOT fully verified: the writes were accepted, but the read-back was incomplete or a setting does not read back as planned. Do not rely on it until it is checked.",
        ),
        ApplyStatus::Failed => (Level::Error, "Failed: at least one write did not go through, or nothing could be written."),
        ApplyStatus::Indeterminate => (Level::Error, "Unknown: the camera may have been changed."),
        ApplyStatus::NotApplied => (Level::Warn, "Not applied: the camera was not changed."),
    }
}

/// What a failed apply leaves unsaid: the writes are made one after another, so one that failed
/// does not mean the others did not land. Said only where there is a restore point to point to.
fn failed_aftermath<R: AppliedReport>(r: &R) -> Option<&'static str> {
    (r.status() == ApplyStatus::Failed && r.restore_point().is_some()).then_some(
        "Some writes may have landed even though this failed. Compare the camera with the restore point below; the command under it puts back what this changed.",
    )
}

/// The text of the confirmation for an apply: it names the body and carries the plan's summary,
/// so what is approved is what is sent.
fn apply_confirm_text(p: &PlannedTransplant) -> String {
    let t = &p.request.target;
    let plan = &p.plan;
    let mut s = format!(
        "Write to {} (serial {}, firmware {}) the settings of {} ({} {}{})?\n{}",
        t.model,
        t.serial,
        t.firmware,
        p.request.source_file,
        plan.source.model,
        plan.source.serial,
        plan.source_label.as_deref().map(|l| format!(", {l:?}")).unwrap_or_default(),
        plan.summary,
    );
    if !plan.allow_raw_code.is_empty() {
        let opted: Vec<String> = plan
            .allow_raw_code
            .iter()
            .map(|n| {
                let value = p.optin_values.get(n).map_or("a value that was not stated", String::as_str);
                format!("{} (sets {value})", short_name(n))
            })
            .collect();
        s.push_str(&format!(
            "\nOpted in by raw code or bare number (not known to mean the same on both bodies): {}.",
            opted.join(", ")
        ));
    }
    s.push_str(
        "\nA restore point of the camera is saved first and every write is read back; the result says whether it took.",
    );
    s
}

/// The extra confirmation for one property that is matched only by raw code or copied as a bare
/// number. It names the property, both bodies and the value.
fn opt_in_text(p: &PlannedTransplant, property: &str) -> String {
    let o = p.plan.outcomes.iter().find(|o| o.name == property);
    let proposed = o.and_then(OutcomeJson::proposed).unwrap_or("(not stated)");
    let how = match o.and_then(OutcomeJson::tag) {
        Some("raw_code_unconfirmed") => "matched only by its numeric code",
        _ => "copied as a bare number",
    };
    format!(
        "Write {} anyway? It is {how}: nothing guarantees that {} {} gives that number the same meaning as {} {}. \
         It would set {proposed}. This adds it to the plan, which is then made again, and you confirm the apply separately.",
        short_name(property),
        p.plan.target.model,
        p.plan.target.serial,
        p.plan.source.model,
        p.plan.source.serial,
    )
}

/// A readable account of an apply report: what was written, what was not, and what the read-back
/// found. Lines marked `!` are problems.
fn apply_report_lines(r: &ApplyJson) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    let mut line = |level, text: String| out.push((level, text));
    if let Some(aftermath) = failed_aftermath(r) {
        line(Level::Error, aftermath.to_string());
    }
    if r.no_op {
        line(
            if r.status == ApplyStatus::Failed { Level::Error } else { Level::Info },
            if r.status == ApplyStatus::Failed {
                "Nothing was copied, and not because there was nothing to change; no restore point was taken.".to_string()
            } else {
                "Nothing to write: the camera already holds everything the plan copies. No restore point was taken.".to_string()
            },
        );
    }
    if r.restore_point_gate_waived {
        line(Level::Warn, "The restore point did not pass the model's completeness check and was kept anyway.".into());
    }
    if !r.written.is_empty() {
        line(Level::Info, format!("Written ({}): {}", r.written.len(), names_list(&r.written)));
    }
    for e in &r.write_errors {
        line(Level::Error, format!("Write failed: {}: {}", short_name(&e.name), e.message));
    }
    if !r.rejected_by_kind.is_empty() {
        line(
            Level::Warn,
            format!("Not taken by the body (it rejects that kind of write): {}", names_list(&r.rejected_by_kind)),
        );
    }
    match &r.readback {
        None if !r.no_op => line(Level::Warn, "No read-back was made, so nothing written is confirmed.".into()),
        None => {}
        Some(rb) => {
            match rb.gate.as_ref().map(|g| g.outcome) {
                Some(GateKind::Passed) => line(Level::Info, "Read-back: complete.".into()),
                Some(GateKind::Failed) => line(Level::Warn, "Read-back: INCOMPLETE (capabilities the model should read were missing).".into()),
                Some(GateKind::NoBaseline) => line(Level::Warn, "Read-back: could not be judged complete (no baseline for this model and firmware).".into()),
                None => line(Level::Warn, "Read-back: nothing could be read.".into()),
            }
            if !rb.not_taken.is_empty() {
                line(Level::Error, format!("Did NOT read back as written ({}): {}", rb.not_taken.len(), names_list(&rb.not_taken)));
            }
            if !rb.unverifiable.is_empty() {
                line(Level::Warn, format!("Could not be read back ({}): {}", rb.unverifiable.len(), names_list(&rb.unverifiable)));
            }
            match &rb.side_effects {
                Some(se) if !se.is_empty_effects() => {
                    let mut names: Vec<String> = se.changed.iter().map(|n| format!("{} (changed)", short_name(n))).collect();
                    names.extend(se.vanished.iter().map(|n| format!("{} (vanished)", short_name(n))));
                    names.extend(se.appeared.iter().map(|n| format!("{} (appeared)", short_name(n))));
                    line(Level::Warn, format!("Changed without being written: {}", names.join(", ")));
                }
                Some(_) => {}
                None => line(Level::Warn, "Side effects: unknown (the read-back was not complete enough to compare).".into()),
            }
        }
    }
    if !r.undo_not_restorable.is_empty() {
        line(
            Level::Warn,
            format!("An undo cannot put these back: {}", names_list(&r.undo_not_restorable)),
        );
    }
    out
}

impl SideEffectsJson {
    fn is_empty_effects(&self) -> bool {
        self.changed.is_empty() && self.vanished.is_empty() && self.appeared.is_empty()
    }
}

/// Where a transplant is: one plan at a time, and a plan or a confirmation is good only for the
/// request it was made for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TransplantStage {
    #[default]
    Idle,
    /// A plan was asked for this request and has not come back.
    Planning(TransplantRequest),
    Planned(PlannedTransplant),
    /// Asked to opt one property in; waiting for the extra confirmation.
    OptIn { plan: PlannedTransplant, property: String },
    /// Asked to apply; waiting for the confirmation.
    ConfirmApply(PlannedTransplant),
    /// Sent; the answer is the only thing that ends this.
    Applying(TransplantRequest),
    Done(Box<AppliedTransplant>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedTransplant {
    request: TransplantRequest,
    outcome: ApplyOutcome,
}

/// The transplant window's state.
#[derive(Debug, Default)]
struct TransplantUi {
    open: bool,
    source_body: Option<(String, String)>,
    source_file: Option<String>,
    /// The chosen target, by identity; the live row is found among the discovered cameras.
    target_key: Option<(String, String)>,
    optins: BTreeSet<String>,
    /// What each opt-in was proposed to set, from the plan that offered it.
    optin_values: BTreeMap<String, String>,
    /// The source file and the whole target row (firmware and pairing included) the opt-ins were
    /// given for: any other has none.
    optins_for: Option<(String, CameraRow)>,
    stage: TransplantStage,
    /// Why a plan or confirmation went away, or how a plan failed.
    notice: String,
}

impl TransplantUi {
    fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }

    /// The request the form asks for now, if it asks for one.
    fn request(&self, cameras: &[CameraRow]) -> Option<TransplantRequest> {
        Some(TransplantRequest {
            source_body: self.source_body.clone()?,
            source_file: self.source_file.clone()?,
            target: self.target_camera(cameras)?.clone(),
            allow_raw_code: self.optins.clone(),
        })
    }

    /// Run every frame, before anything is drawn: opt-ins belong to the source and target they were
    /// confirmed for, and a plan or confirmation does not outlive a change to its request.
    fn sync(&mut self, cameras: &[CameraRow]) {
        let pair = self.source_file.clone().zip(self.target_camera(cameras).cloned());
        if self.optins_for != pair {
            self.clear_optins();
            self.optins_for = pair;
        }
        let held = match &self.stage {
            TransplantStage::Planning(r) => Some(r),
            TransplantStage::Planned(p) | TransplantStage::ConfirmApply(p) | TransplantStage::OptIn { plan: p, .. } => {
                Some(&p.request)
            }
            _ => None,
        };
        if let Some(held) = held {
            if self.request(cameras).as_ref() != Some(held) {
                let was_confirming = matches!(self.stage, TransplantStage::ConfirmApply(_) | TransplantStage::OptIn { .. });
                self.go_idle();
                self.notice = format!(
                    "{} withdrawn: the snapshot, the camera or the opt-ins changed. Nothing was written.",
                    if was_confirming { "Confirmation" } else { "Plan" }
                );
            }
        }
    }

    fn clear_optins(&mut self) {
        self.optins.clear();
        self.optin_values.clear();
    }

    /// Nothing is held any more. The opt-ins go with the plan they were made for: they are shown
    /// only on a plan, so none may wait unseen to ride into the next one.
    fn go_idle(&mut self) {
        self.stage = TransplantStage::Idle;
        self.clear_optins();
    }

    /// "Discard plan" / "Close this result".
    fn discard(&mut self) {
        if matches!(self.stage, TransplantStage::Planned(_) | TransplantStage::Done(_)) {
            self.go_idle();
        }
    }

    /// The data directory changed under a plan or a confirmation: the snapshots and the restore
    /// points it names are no longer where it said, so it is withdrawn. An apply on the wire is
    /// not: its answer must still find it.
    fn data_dir_changed(&mut self) {
        if matches!(
            self.stage,
            TransplantStage::Planning(_) | TransplantStage::Planned(_) | TransplantStage::OptIn { .. } | TransplantStage::ConfirmApply(_)
        ) {
            self.go_idle();
            self.notice = "Plan withdrawn: the data directory changed. Nothing was written.".to_string();
        }
    }

    /// The worker thread is gone: nothing more will be answered. A plan being made is lost; an
    /// apply on the wire may have written, whatever stopped.
    fn worker_lost(&mut self) {
        match &self.stage {
            TransplantStage::Planning(_) => {
                self.go_idle();
                self.notice = "The worker thread stopped while the camera was being read. Nothing was written.".to_string();
            }
            TransplantStage::Applying(request) => {
                let outcome = ApplyOutcome::Indeterminate {
                    message: "The worker thread stopped while the apply was running, so what it wrote is unknown.".to_string(),
                    cli_log: String::new(),
                };
                let request = request.clone();
                self.clear_optins();
                self.stage = TransplantStage::Done(Box::new(AppliedTransplant { request, outcome }));
            }
            _ => {}
        }
    }

    /// A command could not be handed to the worker, which is gone: the camera was not asked.
    fn not_sent(&mut self) {
        match &self.stage {
            TransplantStage::Planning(_) => {
                self.go_idle();
                self.notice = "The worker thread has stopped, so the camera was not read. Nothing was written.".to_string();
            }
            TransplantStage::Applying(request) => {
                let outcome = ApplyOutcome::NotApplied {
                    why: NotAppliedWhy::Refused,
                    message: "The worker thread has stopped, so the apply was never sent.".to_string(),
                    cli_log: String::new(),
                };
                let request = request.clone();
                self.clear_optins();
                self.stage = TransplantStage::Done(Box::new(AppliedTransplant { request, outcome }));
            }
            _ => {}
        }
    }

    /// Start a plan for the form as it is. The command to send, or why not.
    fn begin_plan(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        let request = self.request(cameras).ok_or("Choose a source snapshot and a connected target first.")?;
        if let Some(why) = transplant_target_refusal(&request.target) {
            return Err(why);
        }
        self.notice.clear();
        self.stage = TransplantStage::Planning(request.clone());
        Ok(Cmd::TransplantPlan { request })
    }

    /// A plan came back. Dropped unless it is the one asked for and still wanted.
    fn plan_arrived(&mut self, request: TransplantRequest, result: Result<(PlanJson, String), String>) {
        if self.stage != TransplantStage::Planning(request.clone()) {
            return;
        }
        match result {
            Ok((plan, cli_log)) => {
                let optin_values = self
                    .optin_values
                    .iter()
                    .filter(|(name, _)| request.allow_raw_code.contains(*name))
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect();
                self.stage = TransplantStage::Planned(PlannedTransplant { request, plan, optin_values, cli_log });
            }
            Err(e) => {
                self.go_idle();
                self.notice = e;
            }
        }
    }

    /// Ask for the confirmation to apply. Only a plan that would write something can be applied.
    fn ask_apply(&mut self) {
        if let TransplantStage::Planned(p) = &self.stage {
            if p.plan.counts.would_change > 0 {
                self.stage = TransplantStage::ConfirmApply(p.clone());
            }
        }
    }

    /// Back from a confirmation to the plan; nothing was asked of the camera.
    fn cancel_confirmation(&mut self) {
        let plan = match &self.stage {
            TransplantStage::ConfirmApply(p) | TransplantStage::OptIn { plan: p, .. } => p.clone(),
            _ => return,
        };
        self.stage = TransplantStage::Planned(plan);
    }

    /// Ask for the extra confirmation to opt `property` in. Only a property the plan skipped for
    /// want of an opt-in can be asked about.
    fn ask_opt_in(&mut self, property: &str) {
        if let TransplantStage::Planned(p) = &self.stage {
            if p.plan.outcomes.iter().any(|o| o.name == property && o.needs_opt_in()) {
                self.stage = TransplantStage::OptIn { plan: p.clone(), property: property.to_string() };
            }
        }
    }

    /// The opt-in was confirmed: add it and plan again, since the opt-ins are part of the digest.
    fn confirm_opt_in(&mut self, cameras: &[CameraRow]) -> Option<Cmd> {
        let TransplantStage::OptIn { plan, property } = &self.stage else {
            return None;
        };
        let property = property.clone();
        let proposed = plan.plan.outcomes.iter().find(|o| o.name == property).and_then(OutcomeJson::proposed).map(str::to_string);
        self.optins.insert(property.clone());
        if let Some(value) = proposed {
            self.optin_values.insert(property.clone(), value);
        }
        match self.begin_plan(cameras) {
            Ok(cmd) => Some(cmd),
            Err(e) => {
                // No plan was made with it, so no opt-in is taken either.
                self.go_idle();
                self.notice = e;
                None
            }
        }
    }

    /// Take one opt-in back: the plan is made again without it and with the others.
    fn remove_opt_in(&mut self, property: &str, cameras: &[CameraRow]) -> Option<Cmd> {
        if !matches!(self.stage, TransplantStage::Planned(_)) || !self.optins.remove(property) {
            return None;
        }
        self.optin_values.remove(property);
        match self.begin_plan(cameras) {
            Ok(cmd) => Some(cmd),
            Err(e) => {
                self.go_idle();
                self.notice = e;
                None
            }
        }
    }

    /// The apply was confirmed: what is sent is the plan that was shown, not the form as it is now.
    fn confirm_apply(&mut self) -> Option<Cmd> {
        let TransplantStage::ConfirmApply(p) = &self.stage else {
            return None;
        };
        let cmd = Cmd::TransplantApply {
            request: p.request.clone(),
            digest: p.plan.digest.clone(),
            summary: p.plan.summary.clone(),
        };
        self.stage = TransplantStage::Applying(p.request.clone());
        Some(cmd)
    }

    /// The answer to an apply. The opt-ins were for that apply and do not carry to the next plan.
    fn applied(&mut self, request: TransplantRequest, outcome: ApplyOutcome) {
        if self.stage != TransplantStage::Applying(request.clone()) {
            return;
        }
        self.clear_optins();
        self.stage = TransplantStage::Done(Box::new(AppliedTransplant { request, outcome }));
    }
}

/// The status-bar line for a transplant outcome.
fn applied_status(outcome: &ApplyOutcome) -> Status {
    applied_status_of("Transplant", outcome)
}

/// The status-bar line for the outcome of `what` (a transplant, a restore).
fn applied_status_of<R: AppliedReport>(what: &str, outcome: &ApplyOutcome<R>) -> Status {
    match outcome {
        ApplyOutcome::Report { report, .. } => {
            let (level, text) = status_headline(report.status());
            let aftermath = failed_aftermath(&**report).map(|_| " Some writes may have landed: a restore point was saved, see the result.");
            Status::new(level, format!("{what} {}: {text}{}", report.status().tag(), aftermath.unwrap_or("")))
        }
        ApplyOutcome::NotApplied { why, message, .. } => Status::new(
            Level::Warn,
            match why {
                NotAppliedWhy::PlanStale => format!("{what} not_applied: the camera no longer matches the plan you approved; nothing was written. Plan again."),
                NotAppliedWhy::Refused => format!("{what} not_applied: {message}"),
            },
        ),
        ApplyOutcome::Indeterminate { message, .. } => {
            Status::new(Level::Error, format!("{what} indeterminate: the camera may have been changed. {message} Take a snapshot and compare before trying again."))
        }
    }
}

// ── The transplant window ─────────────────────────────────────────────────

/// What a click in the transplant window asks for. Collected while drawing and applied after, as
/// the vendor window does, so drawing never holds the app mutably.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TransplantAction {
    Plan,
    AskApply,
    ConfirmApply,
    CancelConfirmation,
    AskOptIn(String),
    ConfirmOptIn,
    RemoveOptIn(String),
    Discard,
    View(String),
    Copy(String),
    Undo(UndoOffer),
}

const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(200, 120, 0);
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(200, 60, 60);

fn colored(ui: &mut egui::Ui, level: Level, text: impl Into<String>) {
    let text = text.into();
    match level_color(level) {
        Some(c) => ui.colored_label(c, text),
        None => ui.label(text),
    };
}

fn camera_label(c: &CameraRow) -> String {
    format!("{}  {}  fw {}", c.model, c.serial, c.firmware)
}

fn snapshot_label(row: &SnapRow) -> String {
    let ts = row.captured_at.get(..19).unwrap_or(&row.captured_at);
    let label = row.label.as_deref().unwrap_or("(no label)");
    format!("{ts}  {label}{}", if row.is_reference { "  ◀ ref" } else { "" })
}

fn draw_outcomes(ui: &mut egui::Ui, id: &str, title: String, open: bool, level: Level, rows: &[&OutcomeJson]) {
    if rows.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(format!("{title} ({})", rows.len())).id_salt(id).default_open(open).show(ui, |ui| {
        for o in rows {
            colored(ui, level, o.line());
        }
    });
}

fn draw_plan(ui: &mut egui::Ui, p: &PlannedTransplant, can_opt_in: bool, busy: bool, action: &mut Option<TransplantAction>) {
    let plan = &p.plan;
    ui.horizontal_wrapped(|ui| {
        ui.heading(format!("{} {} → {} {}", plan.source.model, plan.source.serial, plan.target.model, plan.target.serial));
    });
    ui.label(
        egui::RichText::new(format!(
            "{}{}  →  {} (fw {})",
            p.request.source_file,
            plan.source_label.as_deref().map(|l| format!("  ({l})")).unwrap_or_default(),
            plan.target.model,
            plan.target.firmware
        ))
        .weak(),
    );
    if plan.same_model {
        ui.label(
            egui::RichText::new("Same model on both sides: a restore of this snapshot is the simpler way to do this.").weak(),
        );
    }
    ui.label(egui::RichText::new(&plan.summary).strong());
    ui.label(egui::RichText::new(format!("plan {}", plan.digest.get(..12).unwrap_or(&plan.digest))).small().weak());
    let skipped: Vec<String> = plan
        .counts
        .by_tag
        .iter()
        .filter(|(tag, _)| tag.as_str() != "copied")
        .map(|(tag, n)| format!("{n} {}", tag.replace('_', " ")))
        .collect();
    ui.label(format!(
        "{} would be written ({} would change). Skipped: {}.",
        plan.counts.copied,
        plan.counts.would_change,
        if skipped.is_empty() { "nothing".to_string() } else { skipped.join(", ") }
    ));
    if !plan.allow_raw_code.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(WARN_COLOR, "Opted in by raw code or bare number:");
            for name in &plan.allow_raw_code {
                ui.label(short_name(name));
                ui.add_enabled_ui(can_opt_in && !busy, |ui| {
                    if ui.small_button("✕").on_hover_text("Take this opt-in back; the plan is withdrawn").clicked() {
                        *action = Some(TransplantAction::RemoveOptIn(name.clone()));
                    }
                });
            }
        });
    }
    ui.separator();

    let sections = plan_sections(&plan.outcomes);
    draw_outcomes(ui, "tp_change", "Would change".into(), true, Level::Info, &sections.will_change);
    if !sections.needs_decision.is_empty() {
        egui::CollapsingHeader::new(format!("Skipped until you opt in ({})", sections.needs_decision.len()))
            .id_salt("tp_decide")
            .default_open(true)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "These are matched only by a number whose meaning on the target nobody has checked. Each is left out unless you opt it in by name.",
                    )
                    .weak(),
                );
                for o in &sections.needs_decision {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(WARN_COLOR, o.line());
                        ui.add_enabled_ui(can_opt_in && !busy, |ui| {
                            if ui.small_button(format!("Opt in to {}…", short_name(&o.name))).clicked() {
                                *action = Some(TransplantAction::AskOptIn(o.name.clone()));
                            }
                        });
                    });
                }
            });
    }
    draw_outcomes(ui, "tp_attention", "Skipped, needs attention".into(), true, Level::Warn, &sections.attention);
    draw_outcomes(ui, "tp_same", "Already the same".into(), false, Level::Info, &sections.unchanged);
    if !sections.routine.is_empty() {
        let n: usize = sections.routine.values().map(Vec::len).sum();
        egui::CollapsingHeader::new(format!("Skipped as a matter of course ({n})")).id_salt("tp_routine").show(ui, |ui| {
            for (tag, rows) in &sections.routine {
                draw_outcomes(ui, &format!("tp_routine_{tag}"), tag.replace('_', " "), false, Level::Info, rows);
            }
        });
    }
    if !plan.cannot_read.is_empty() {
        egui::CollapsingHeader::new(format!("Cannot be copied at all ({})", plan.cannot_read.len())).id_salt("tp_cannot").show(ui, |ui| {
            for c in &plan.cannot_read {
                ui.label(format!("{}: {}", c.what, c.why));
            }
        });
    }
    if !p.cli_log.trim().is_empty() {
        egui::CollapsingHeader::new("CLI messages").id_salt("tp_log").show(ui, |ui| {
            ui.label(egui::RichText::new(p.cli_log.trim_end()).monospace());
        });
    }
}

fn draw_applied(ui: &mut egui::Ui, a: &AppliedTransplant, busy: bool, action: &mut Option<TransplantAction>) {
    let t = &a.request.target;
    ui.label(egui::RichText::new(format!("{}  →  {} {}", a.request.source_file, t.model, t.serial)).weak());
    match &a.outcome {
        ApplyOutcome::Report { report, note, cli_log } => {
            let (level, headline) = status_headline(report.status);
            ui.heading(report.status.tag());
            colored(ui, level, headline);
            if let Some(note) = note {
                colored(ui, Level::Warn, note.clone());
            }
            for (level, line) in apply_report_lines(report) {
                colored(ui, level, line);
            }
            let skipped = report.outcomes.iter().filter(|o| o.status == OutcomeStatus::Skipped).count();
            if skipped > 0 {
                ui.label(egui::RichText::new(format!("{skipped} propert(ies) were skipped, as the plan showed.")).weak());
            }
            if let Some(rp) = &report.restore_point {
                let offer = restore::undo_offer(&a.request.target, &a.outcome);
                if let Some(c) = restore::draw_restore_point(ui, rp, offer.as_ref(), &report.undo_set, busy) {
                    *action = Some(match c {
                        RestoreActionLike::View(f) => TransplantAction::View(f),
                        RestoreActionLike::Copy(c) => TransplantAction::Copy(c),
                        RestoreActionLike::Undo(o) => TransplantAction::Undo(o),
                    });
                }
            }
            if !cli_log.trim().is_empty() {
                egui::CollapsingHeader::new("CLI messages").id_salt("tp_applied_log").show(ui, |ui| {
                    ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
                });
            }
        }
        ApplyOutcome::NotApplied { why, message, cli_log } => {
            ui.heading("not_applied");
            colored(ui, Level::Warn, "The camera was not changed.");
            if *why == NotAppliedWhy::PlanStale {
                colored(
                    ui,
                    Level::Warn,
                    "The camera or the snapshot no longer gives the plan you approved. Plan again to see what would be written now.",
                );
            }
            ui.label(message);
            if !cli_log.trim().is_empty() {
                ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
            }
        }
        ApplyOutcome::Indeterminate { message, cli_log } => {
            ui.heading("indeterminate");
            colored(ui, Level::Error, "The camera may have been changed.");
            ui.label(message);
            ui.label("Take a snapshot of the camera and compare it with the one you started from before doing anything else.");
            if !cli_log.trim().is_empty() {
                ui.label(egui::RichText::new(cli_log.trim_end()).monospace());
            }
        }
    }
}

impl FleetApp {
    fn ui_transplant(&mut self, ctx: &egui::Context) {
        if !self.transplant.open {
            return;
        }
        let mut open = true;
        let mut action: Option<TransplantAction> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::Window::new("Transplant settings onto a camera")
            .open(&mut open)
            .default_size([700.0, 620.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let t = &mut self.transplant;
                let cameras = &self.cameras;
                let listing = self.listing.as_ref();
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(720.0).show(ui, |ui| {
                    ui.label(egui::RichText::new("1. Source: a stored snapshot").strong());
                    let bodies = listing.map(SnapListing::bodies).unwrap_or_default();
                    if bodies.is_empty() {
                        ui.label(egui::RichText::new("No snapshots are stored.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            let body_text = t.source_body.as_ref().map_or("choose a body…".to_string(), |(m, s)| format!("{m}  {s}"));
                            egui::ComboBox::from_id_salt("tp_source_body").selected_text(body_text).width(260.0).show_ui(ui, |ui| {
                                for b in &bodies {
                                    let picked = t.source_body.as_ref() == Some(b);
                                    if ui.selectable_label(picked, format!("{}  {}", b.0, b.1)).clicked() && !picked {
                                        t.source_body = Some(b.clone());
                                        t.source_file = None;
                                    }
                                }
                            });
                            let rows: Vec<&SnapRow> = match (listing, &t.source_body) {
                                (Some(l), Some((m, s))) => l.rows_for(m, s).filter(|r| !r.name_lossy).collect(),
                                _ => Vec::new(),
                            };
                            let file_text = t
                                .source_file
                                .as_ref()
                                .and_then(|f| rows.iter().find(|r| &r.file == f))
                                .map_or("choose a snapshot…".to_string(), |r| snapshot_label(r));
                            egui::ComboBox::from_id_salt("tp_source_file").selected_text(file_text).width(300.0).show_ui(ui, |ui| {
                                for r in &rows {
                                    let picked = t.source_file.as_ref() == Some(&r.file);
                                    if ui.selectable_label(picked, snapshot_label(r)).clicked() {
                                        t.source_file = Some(r.file.clone());
                                    }
                                }
                            });
                        });
                    });

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("2. Target: a connected body").strong());
                    if cameras.is_empty() {
                        ui.label(egui::RichText::new("No camera is connected: use Discover first.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = t.target_camera(cameras).map_or("choose a body…".to_string(), camera_label);
                        egui::ComboBox::from_id_salt("tp_target").selected_text(text).width(420.0).show_ui(ui, |ui| {
                            for c in cameras {
                                let picked = t.target_key.as_ref() == Some(&(c.model.clone(), c.serial.clone()));
                                if ui.selectable_label(picked, camera_label(c)).clicked() {
                                    t.target_key = Some((c.model.clone(), c.serial.clone()));
                                }
                            }
                        });
                    });
                    if t.target_key.is_some() && t.target_camera(cameras).is_none() {
                        ui.colored_label(WARN_COLOR, "That body is no longer connected.");
                    }
                    let refusal = t.target_camera(cameras).and_then(transplant_target_refusal);
                    if let Some(why) = &refusal {
                        ui.colored_label(ERROR_COLOR, why);
                    }

                    ui.add_space(6.0);
                    let planning_possible = !busy
                        && data_ready
                        && refusal.is_none()
                        && t.request(cameras).is_some()
                        && !matches!(t.stage, TransplantStage::Applying(_));
                    ui.add_enabled_ui(planning_possible, |ui| {
                        if ui.button("Plan (reads the camera, writes nothing)").clicked() {
                            action = Some(TransplantAction::Plan);
                        }
                    });
                    if !t.notice.is_empty() {
                        ui.colored_label(WARN_COLOR, &t.notice);
                    }
                    ui.separator();

                    match &t.stage {
                        TransplantStage::Idle => {
                            ui.label(
                                egui::RichText::new("Choose a source and a target, then plan. Nothing is sent to the camera until you have read the plan and confirmed it.")
                                    .weak(),
                            );
                        }
                        TransplantStage::Planning(_) => {
                            ui.label("Planning… the camera is being read.");
                        }
                        TransplantStage::Planned(p) => {
                            draw_plan(ui, p, true, busy, &mut action);
                            ui.separator();
                            let writes = p.plan.counts.would_change > 0;
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(!busy && writes, |ui| {
                                    if ui.button("Apply to camera…").clicked() {
                                        action = Some(TransplantAction::AskApply);
                                    }
                                });
                                if ui.button("Discard plan").clicked() {
                                    action = Some(TransplantAction::Discard);
                                }
                            });
                            if !writes {
                                ui.label(egui::RichText::new("This plan would change nothing, so there is nothing to apply.").weak());
                            }
                        }
                        TransplantStage::OptIn { plan, property } => {
                            draw_plan(ui, plan, false, busy, &mut action);
                            ui.separator();
                            ui.colored_label(WARN_COLOR, opt_in_text(plan, property));
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.horizontal(|ui| {
                                    if ui.button(format!("Opt in to {}", short_name(property))).clicked() {
                                        action = Some(TransplantAction::ConfirmOptIn);
                                    }
                                    if ui.button("Cancel").clicked() {
                                        action = Some(TransplantAction::CancelConfirmation);
                                    }
                                });
                            });
                        }
                        TransplantStage::ConfirmApply(p) => {
                            draw_plan(ui, p, false, busy, &mut action);
                            ui.separator();
                            ui.colored_label(WARN_COLOR, apply_confirm_text(p));
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.horizontal(|ui| {
                                    if ui.button("Confirm: write to camera").clicked() {
                                        action = Some(TransplantAction::ConfirmApply);
                                    }
                                    if ui.button("Cancel").clicked() {
                                        action = Some(TransplantAction::CancelConfirmation);
                                    }
                                });
                            });
                        }
                        TransplantStage::Applying(r) => {
                            ui.colored_label(
                                WARN_COLOR,
                                format!("Writing to {} {}… keep it connected and do not close this program.", r.target.model, r.target.serial),
                            );
                        }
                        TransplantStage::Done(a) => {
                            draw_applied(ui, a, busy, &mut action);
                            ui.separator();
                            if ui.button("Close this result").clicked() {
                                action = Some(TransplantAction::Discard);
                            }
                        }
                    }
                });
            });
        self.transplant.open = open;
        if let Some(a) = action {
            self.transplant_act(ctx, a);
        }
    }

    fn transplant_act(&mut self, ctx: &egui::Context, action: TransplantAction) {
        match action {
            TransplantAction::Plan => match self.transplant.begin_plan(&self.cameras) {
                Ok(cmd) => self.send_transplant(cmd),
                Err(e) => self.transplant.notice = e,
            },
            TransplantAction::AskApply => self.transplant.ask_apply(),
            TransplantAction::ConfirmApply => {
                if let Some(cmd) = self.transplant.confirm_apply() {
                    self.send_transplant(cmd);
                }
            }
            TransplantAction::CancelConfirmation => self.transplant.cancel_confirmation(),
            TransplantAction::AskOptIn(property) => self.transplant.ask_opt_in(&property),
            TransplantAction::ConfirmOptIn => {
                if let Some(cmd) = self.transplant.confirm_opt_in(&self.cameras) {
                    self.send_transplant(cmd);
                }
            }
            TransplantAction::RemoveOptIn(property) => {
                if let Some(cmd) = self.transplant.remove_opt_in(&property, &self.cameras) {
                    self.send_transplant(cmd);
                }
            }
            TransplantAction::Discard => self.transplant.discard(),
            TransplantAction::View(file) => self.open_snapshot(ctx, &file, false, false),
            TransplantAction::Copy(text) => ctx.copy_text(text),
            TransplantAction::Undo(offer) => self.start_undo(offer),
        }
    }

    /// Hand a transplant command to the worker. If the worker is gone the command was not delivered,
    /// and the window must not wait for an answer that cannot come.
    fn send_transplant(&mut self, cmd: Cmd) {
        if !self.send(cmd) {
            self.transplant.not_sent();
        }
    }

    /// Open the window with `file` of `body` as the source. The target stays what was chosen; with
    /// none chosen yet it is the camera selected in the sidebar, if any, still to be read in the
    /// window and in the confirmation before anything is written.
    fn start_transplant_from(&mut self, body: (String, String), file: String) {
        self.transplant.open = true;
        self.transplant.source_body = Some(body);
        self.transplant.source_file = Some(file);
        if self.transplant.target_key.is_none() {
            self.transplant.target_key = self.selected_camera().map(|c| (c.model.clone(), c.serial.clone()));
        }
    }
}

// ── References ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreviousReference {
    /// No reference existed.
    None,
    /// The snapshot was already byte-for-byte the reference; nothing changed.
    Identical,
    /// A different reference existed and was replaced.
    Replaced { captured_at: Option<String>, label: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReferenceReport {
    model: String,
    serial: String,
    reference_file: String,
    previous: PreviousReference,
}

/// Promote `snapshots/{file}` to be the reference for its camera: a byte copy under
/// `references/{model_slug}_{serial}.json`, the same promotion as `nikon-fleet ref set`. The name
/// is built from the snapshot's own model and serial through
/// `ops::paths::checked_reference_filename`, so a `/` or `..` inside a snapshot (data, not code)
/// cannot write outside `references/`; the bytes parsed are the bytes written; the write is
/// atomic, so a failure keeps the old reference. An existing reference is replaced, and the
/// report says so.
fn set_reference(data_dir: &Path, file: &str) -> Result<ReferenceReport, String> {
    if !is_safe_path_component(file) {
        return Err(format!("{file:?} is not a plain snapshot file name"));
    }
    let src = data_dir.join("snapshots").join(file);
    let bytes = fs::read(&src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    let text = std::str::from_utf8(&bytes).map_err(|e| format!("{} is not valid UTF-8: {e}", src.display()))?;
    let snap = Snapshot::from_json(text).map_err(|e| format!("loading {}: {e}", src.display()))?;
    let name = checked_reference_filename(&snap.camera.model, &snap.camera.serial).map_err(|e| {
        format!(
            "{file} cannot be a reference: {e}; the reference file name is built from its camera model and serial"
        )
    })?;
    let refs = data_dir.join("references");
    fs::create_dir_all(&refs).map_err(|e| format!("creating {}: {e}", refs.display()))?;
    let target = refs.join(&name);
    let previous = match fs::read(&target) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // Replacing a file that cannot even be read would hide what is lost.
        Err(e) => return Err(format!("the existing reference {} could not be read ({e}); it was left as it is", target.display())),
    };
    let previous_report = match &previous {
        None => PreviousReference::None,
        Some(b) if *b == bytes => PreviousReference::Identical,
        Some(b) => {
            let old = std::str::from_utf8(b).ok().and_then(|t| Snapshot::from_json(t).ok());
            PreviousReference::Replaced {
                captured_at: old.as_ref().map(|s| s.captured_at.clone()),
                label: old.and_then(|s| s.label),
            }
        }
    };
    if previous_report != PreviousReference::Identical {
        write_atomic(&target, &bytes).map_err(|e| format!("writing {}: {e}", target.display()))?;
    }
    Ok(ReferenceReport {
        model: snap.camera.model,
        serial: snap.camera.serial,
        reference_file: name,
        previous: previous_report,
    })
}

fn describe_reference(r: &ReferenceReport) -> OpDone {
    let head = format!("Reference for {} {} → {}", r.model, r.serial, r.reference_file);
    let text = match &r.previous {
        PreviousReference::None => format!("{head}."),
        PreviousReference::Identical => format!("{head}: this snapshot was already the reference; nothing changed."),
        PreviousReference::Replaced { captured_at, label } => format!(
            "{head}: replaced the previous reference (captured {}, label {}).",
            captured_at.as_deref().unwrap_or("unknown"),
            label.as_deref().unwrap_or("-")
        ),
    };
    OpDone { status: Status::new(Level::Info, text), notices: Vec::new(), cli_log: String::new(), refresh_listing: true }
}

// ── Pack / unpack ─────────────────────────────────────────────────────────

/// Archive layout, one table for export and import (the Python GUI's `accept_zip_entry` follows
/// the same rule; the cases are in `gui/tests-fixtures/zip_entry_names.json`):
///
/// ```text
///   snapshots/<file>.json
///   references/<file>.json
///   firmware/<slug>/<version>/firmware.bin
///   firmware/<slug>/<version>/source-page.html
///   firmware/<slug>/<version>/metadata.json
/// ```
///
/// Every `/`-separated component of the raw name must be a plain path component
/// (`ops::paths::is_safe_path_component`: non-empty, not `.` or `..`, no backslash or NUL). The
/// name is split by hand because `Path::components()` collapses `.`, `//` and a trailing `/`,
/// which would accept `snapshots//a.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Snapshot,
    Reference,
    FirmwareBin,
    FirmwareSource,
    FirmwareMeta,
}

impl EntryKind {
    /// Import order: a firmware entry's `metadata.json` (its existence marker) goes last.
    fn rank(self) -> u8 {
        match self {
            EntryKind::Snapshot => 0,
            EntryKind::Reference => 1,
            EntryKind::FirmwareBin => 2,
            EntryKind::FirmwareSource => 3,
            EntryKind::FirmwareMeta => 4,
        }
    }

    fn is_firmware(self) -> bool {
        matches!(self, EntryKind::FirmwareBin | EntryKind::FirmwareSource | EntryKind::FirmwareMeta)
    }
}

fn classify_zip_entry(name: &str) -> Option<EntryKind> {
    let parts: Vec<&str> = name.split('/').collect();
    if !parts.iter().all(|p| is_safe_path_component(p)) {
        return None;
    }
    match parts.as_slice() {
        ["snapshots", file] if file.ends_with(".json") => Some(EntryKind::Snapshot),
        ["references", file] if file.ends_with(".json") => Some(EntryKind::Reference),
        ["firmware", _, _, "firmware.bin"] => Some(EntryKind::FirmwareBin),
        ["firmware", _, _, "source-page.html"] => Some(EntryKind::FirmwareSource),
        ["firmware", _, _, "metadata.json"] => Some(EntryKind::FirmwareMeta),
        _ => None,
    }
}

/// An archive entry the export or import did not carry, and why. Counted, never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Skipped {
    what: String,
    why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportReport {
    dest: PathBuf,
    files: usize,
    bytes: u64,
    /// Everything in the data directory that is not in the archive.
    skipped: Vec<Skipped>,
}

struct ExportItem {
    path: PathBuf,
    name: String,
    /// Already dense (firmware): stored, not deflated.
    stored: bool,
}

struct ExportPlan {
    items: Vec<ExportItem>,
    skipped: Vec<Skipped>,
}

fn skip(what: impl Into<String>, why: impl Into<String>) -> Skipped {
    Skipped { what: what.into(), why: why.into() }
}

/// What goes into a backup: every `*.json` file in `snapshots/` and `references/` (verbatim,
/// valid or not: a backup preserves bytes, and import is where validity is judged), and each
/// firmware archive entry `nikon-fleet firmware ls` lists (`firmware.bin`, `source-page.html`,
/// `metadata.json`). Anything else under the data directory is listed in `skipped` with its
/// reason, so a backup never silently leaves user data out: unreadable entries, other file types,
/// subdirectories, archive entries the CLI cannot list, other files inside an archive entry, and
/// the legacy flat `firmware/*.bin` layout the CLI no longer reads.
fn plan_export(data_dir: &Path) -> ExportPlan {
    let mut plan = ExportPlan { items: Vec::new(), skipped: Vec::new() };

    for (folder, want) in [("snapshots", EntryKind::Snapshot), ("references", EntryKind::Reference)] {
        let dir = data_dir.join(folder);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                plan.skipped.push(skip(format!("{folder}/"), format!("the directory could not be read ({e}); nothing in it is in the archive")));
                continue;
            }
        };
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    plan.skipped.push(skip(format!("an entry of {folder}/"), format!("could not be read ({e})")));
                    continue;
                }
            };
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                plan.skipped.push(skip(
                    format!("{folder}/{}", file_name.to_string_lossy()),
                    "the file name is not valid UTF-8",
                ));
                continue;
            };
            let path = entry.path();
            let archive_name = format!("{folder}/{name}");
            if !path.is_file() {
                plan.skipped.push(skip(archive_name, "not a regular file"));
            } else if classify_zip_entry(&archive_name) != Some(want) {
                plan.skipped.push(skip(archive_name, "not a .json file with a plain name"));
            } else {
                found.push((archive_name, path));
            }
        }
        found.sort();
        plan.items.extend(found.into_iter().map(|(name, path)| ExportItem { path, name, stored: false }));
    }

    let scan = scan_archives(data_dir, None);
    let root = firmware_root(data_dir);
    for entry in &scan.entries {
        let Ok(rel) = entry.directory.strip_prefix(&root) else {
            plan.skipped.push(skip(entry.directory.display().to_string(), "outside the firmware directory"));
            continue;
        };
        let unit = rel.components().map(|c| c.as_os_str().to_str().map(str::to_string)).collect::<Option<Vec<_>>>();
        let Some(unit) = unit.filter(|u| u.len() == 2) else {
            plan.skipped.push(skip(entry.directory.display().to_string(), "the directory name is not a plain UTF-8 model/version pair"));
            continue;
        };
        let unit = unit.join("/");
        if !entry.bin_present {
            plan.skipped.push(skip(
                format!("firmware/{unit}/firmware.bin"),
                "missing from the archive entry already; only its metadata is exported",
            ));
        }
        let known = ["firmware.bin", "source-page.html", "metadata.json"];
        for file in known {
            let path = entry.directory.join(file);
            if path.is_file() {
                let name = format!("firmware/{unit}/{file}");
                if classify_zip_entry(&name).is_some() {
                    plan.items.push(ExportItem { path, name, stored: file == "firmware.bin" });
                } else {
                    plan.skipped.push(skip(name, "the name is not importable"));
                }
            }
        }
        match fs::read_dir(&entry.directory) {
            Ok(files) => {
                for f in files {
                    match f {
                        Ok(f) => {
                            let file_name = f.file_name().to_string_lossy().into_owned();
                            if !known.contains(&file_name.as_str()) {
                                plan.skipped.push(skip(format!("firmware/{unit}/{file_name}"), "not part of the archive layout"));
                            }
                        }
                        Err(e) => plan.skipped.push(skip(format!("an entry of firmware/{unit}/"), format!("could not be read ({e})"))),
                    }
                }
            }
            Err(e) => plan.skipped.push(skip(format!("firmware/{unit}/"), format!("could not be listed for extra files ({e})"))),
        }
    }
    for s in &scan.skipped {
        plan.skipped.push(skip(s.directory.display().to_string(), format!("not a valid archive entry: {}", s.error)));
    }
    for _ in 0..scan.skipped_dir_entries {
        plan.skipped.push(skip("a directory entry under firmware/", "could not be read"));
    }
    if let Ok(top) = fs::read_dir(&root) {
        for f in top.flatten() {
            if !f.path().is_dir() {
                plan.skipped.push(skip(
                    format!("firmware/{}", f.file_name().to_string_lossy()),
                    "a flat firmware file (the legacy layout the archive no longer reads); not exported, and import would not accept it. \
                     Move it into the archive with `nikon-fleet firmware add`",
                ));
            }
        }
    }
    plan
}

/// Write the plan into `file` as a zip, streaming each file (a firmware binary is never held in
/// memory). A file that cannot be opened is counted as skipped; a failure while reading or
/// writing one aborts the export, since its entry would be half written.
fn write_zip(plan: &ExportPlan, file: fs::File) -> Result<(fs::File, usize, u64, Vec<Skipped>), String> {
    let mut zip = ZipWriter::new(file);
    let deflated = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    // firmware.bin is already dense (tens/hundreds of MiB): Deflate burns CPU for no size benefit.
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut skipped = Vec::new();
    let (mut files, mut bytes) = (0usize, 0u64);
    for item in &plan.items {
        let mut src = match fs::File::open(&item.path) {
            Ok(f) => f,
            Err(e) => {
                skipped.push(skip(item.name.clone(), format!("could not be opened ({e})")));
                continue;
            }
        };
        zip.start_file(&item.name, if item.stored { stored } else { deflated })
            .map_err(|e| format!("starting {}: {e}", item.name))?;
        bytes += std::io::copy(&mut src, &mut zip).map_err(|e| format!("copying {}: {e}", item.path.display()))?;
        files += 1;
    }
    let file = zip.finish().map_err(|e| format!("finishing the archive: {e}"))?;
    Ok((file, files, bytes, skipped))
}

/// Build the archive in a temporary file beside `dest` and move it into place only when it is
/// whole and flushed, so a failed export never replaces a previous backup (or leaves a partial
/// file). `fill` writes the content.
fn export_via_temp(
    dest: &Path,
    fill: impl FnOnce(fs::File) -> Result<fs::File, String>,
) -> Result<(), String> {
    let name = dest.file_name().ok_or_else(|| format!("{} is not a file path", dest.display()))?;
    let tmp = dest.with_file_name(format!(".{}.{}.partial", name.to_string_lossy(), std::process::id()));
    let file = fs::File::create(&tmp).map_err(|e| format!("creating {}: {e}", tmp.display()))?;
    let finished = fill(file)
        .and_then(|f| f.sync_all().map(|()| ()).map_err(|e| format!("flushing {}: {e}", tmp.display())))
        .and_then(|()| fs::rename(&tmp, dest).map_err(|e| format!("moving the archive to {}: {e}", dest.display())));
    if finished.is_err() {
        // The export's own error is the one to report; a leftover temp file is the lesser harm.
        let _ = fs::remove_file(&tmp);
    }
    finished
}

fn do_export(data_dir: &Path, dest: &Path) -> Result<ExportReport, String> {
    let plan = plan_export(data_dir);
    let mut totals = None;
    export_via_temp(dest, |file| {
        let (file, files, bytes, skipped) = write_zip(&plan, file)?;
        totals = Some((files, bytes, skipped));
        Ok(file)
    })?;
    let (files, bytes, open_failures) = totals.expect("the archive was written");
    let mut skipped = plan.skipped;
    skipped.extend(open_failures);
    Ok(ExportReport { dest: dest.to_path_buf(), files, bytes, skipped })
}

/// How many skipped or rejected names are listed in full; the count is always exact.
const NOTICES_SHOWN: usize = 20;

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn list_notices(lines: impl ExactSizeIterator<Item = String>) -> Vec<String> {
    let total = lines.len();
    let mut out: Vec<String> = lines.take(NOTICES_SHOWN).collect();
    if total > NOTICES_SHOWN {
        out.push(format!("… and {} more", total - NOTICES_SHOWN));
    }
    out
}

fn describe_export(r: &ExportReport) -> OpDone {
    let head = format!("Exported {} file(s) ({}) → {}", r.files, mib(r.bytes), r.dest.display());
    let nothing = if r.files == 0 { " — the data directory holds nothing to export" } else { "" };
    let left_out = if r.skipped.is_empty() {
        String::new()
    } else {
        format!("; {} item(s) are NOT in the archive (see the notes below)", r.skipped.len())
    };
    let level = if r.files == 0 || !r.skipped.is_empty() { Level::Warn } else { Level::Info };
    OpDone {
        status: Status::new(level, format!("{head}{nothing}{left_out}.")),
        notices: list_notices(r.skipped.iter().map(|s| format!("Not exported: {}: {}", s.what, s.why))),
        cli_log: String::new(),
        refresh_listing: false,
    }
}

/// Size limits for an import, so an archive cannot fill the disk. A snapshot is about 100 KB;
/// the JSON limit leaves room for a very large one. A firmware binary is tens to hundreds of MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImportLimits {
    /// A `.json` or `.html` entry, uncompressed.
    json_entry: u64,
    /// A `firmware.bin` entry.
    firmware_entry: u64,
    /// Everything one import writes.
    total: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        ImportLimits { json_entry: 32 * 1024 * 1024, firmware_entry: 1024 * 1024 * 1024, total: 4 * 1024 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    /// Not one of the archive layout's names (see [`EntryKind`]).
    BadName,
    /// Not a regular file (a symlink entry).
    NotAFile,
    /// The entry could not be read out of the archive.
    Unreadable,
    /// Larger than the per-entry limit.
    TooLarge,
    /// Would take the import past the total limit.
    TotalLimit,
    /// A snapshot or reference that `Snapshot::from_json` does not accept (the rule listing uses).
    InvalidSnapshot,
    /// A `metadata.json` that is not valid firmware metadata.
    InvalidMetadata,
    /// A firmware entry without a valid `metadata.json` in the archive (it would be an
    /// incomplete archive entry).
    NoMetadata,
    /// An empty firmware binary.
    EmptyFirmware,
    /// The imported binary's size or SHA-256 differs from what its `metadata.json` records.
    ChecksumMismatch,
    /// A reference whose file name is not the one its own camera model and serial give.
    ReferenceNameMismatch,
    /// Another file of its firmware entry was rejected, so the entry is not imported (a
    /// half entry could not be repaired by importing again: its metadata would mark it done).
    EntryIncomplete,
}

impl RejectReason {
    fn text(self) -> &'static str {
        match self {
            RejectReason::BadName => "not a name this program imports",
            RejectReason::NotAFile => "not a regular file",
            RejectReason::Unreadable => "could not be read from the archive",
            RejectReason::TooLarge => "over the per-entry size limit",
            RejectReason::TotalLimit => "would pass the total size limit",
            RejectReason::InvalidSnapshot => "not a valid snapshot",
            RejectReason::InvalidMetadata => "not valid firmware metadata",
            RejectReason::NoMetadata => "its firmware entry has no valid metadata.json in the archive",
            RejectReason::EmptyFirmware => "an empty firmware binary",
            RejectReason::ChecksumMismatch => "the binary does not match the size or SHA-256 in its metadata.json",
            RejectReason::ReferenceNameMismatch => "the file name is not the one its camera model and serial give",
            RejectReason::EntryIncomplete => "another file of its firmware entry was rejected, so the entry was not imported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rejected {
    name: String,
    reason: RejectReason,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ImportReport {
    snapshots: usize,
    references: usize,
    firmware: usize,
    bytes: u64,
    /// Already present in the data directory, so kept as they are. Never replaced.
    skipped_existing: usize,
    rejected: Vec<Rejected>,
    /// The import stopped on a filesystem error: what it was.
    aborted: Option<String>,
    /// Entries after the failing one, never attempted.
    not_attempted: usize,
}

enum ImportOutcome {
    Imported(u64),
    Existing,
    Rejected(RejectReason, String),
}

fn reject(reason: RejectReason, detail: impl Into<String>) -> ImportOutcome {
    ImportOutcome::Rejected(reason, detail.into())
}

/// The validation a snapshot or reference file must pass: `Snapshot::from_json` on UTF-8 text,
/// exactly what `ops::list::list_dir` applies, so an imported file is listed.
fn parse_snapshot_bytes(bytes: &[u8]) -> Result<Snapshot, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    Snapshot::from_json(text).map_err(|e| e.to_string())
}

/// What `scan_archives` requires of `metadata.json`, and that it describes the entry it sits in:
/// `unit` is `firmware/<slug>/<version>`, and the metadata's model (as a slug) and version must be
/// those, not just any valid metadata under that name.
fn parse_firmware_meta_bytes(bytes: &[u8], unit: &str) -> Result<FirmwareMeta, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let meta: FirmwareMeta = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if meta.format_version != FIRMWARE_META_FORMAT_VERSION {
        return Err(format!("format_version {} (expected {FIRMWARE_META_FORMAT_VERSION})", meta.format_version));
    }
    let mut dirs = unit.split('/').skip(1);
    let (slug, version) = (dirs.next().unwrap_or(""), dirs.next().unwrap_or(""));
    if model_slug(&meta.model) != slug || meta.firmware_version != version {
        return Err(format!(
            "it describes {:?} {:?} but sits in {unit}",
            meta.model, meta.firmware_version
        ));
    }
    Ok(meta)
}

/// The imported binary must be the one its metadata describes: size, then SHA-256.
fn verify_binary(path: &Path, copied: u64, meta: &FirmwareMeta) -> Result<(), String> {
    if copied != meta.bin_size_bytes {
        return Err(format!("{copied} bytes, metadata says {}", meta.bin_size_bytes));
    }
    let sha = sha256_file(path).map_err(|e| format!("could not hash {}: {e}", path.display()))?;
    if sha != meta.bin_sha256 {
        return Err(format!("SHA-256 {sha}, metadata says {}", meta.bin_sha256));
    }
    Ok(())
}

enum CreateOutcome {
    Created,
    Exists,
}

/// Create `path` (and its directories) without ever replacing a file: `create_new` is atomic.
/// A partial file this call made is removed if the write fails. `Err` is a filesystem failure
/// that stops the import.
fn create_new_with(path: &Path, write: impl FnOnce(&mut fs::File) -> std::io::Result<()>) -> Result<CreateOutcome, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(CreateOutcome::Exists),
        Err(e) => return Err(format!("creating {}: {e}", path.display())),
    };
    match write(&mut file).and_then(|()| file.sync_all()) {
        Ok(()) => Ok(CreateOutcome::Created),
        Err(e) => {
            drop(file);
            // The write error is what to report; the partial file is ours (create_new succeeded).
            let _ = fs::remove_file(path);
            Err(format!("writing {}: {e}", path.display()))
        }
    }
}

enum CopyFailure {
    Read(std::io::Error),
    Write(std::io::Error),
    /// More than `cap` bytes arrived.
    TooBig,
}

/// Copy `reader` to `writer`, failing as soon as more than `cap` bytes arrive (whatever the
/// archive's header claimed): a read failure and a write failure are told apart.
fn copy_capped(reader: &mut impl Read, writer: &mut impl Write, cap: u64) -> Result<u64, CopyFailure> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(CopyFailure::Read)?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
        if total > cap {
            return Err(CopyFailure::TooBig);
        }
        writer.write_all(&buf[..n]).map_err(CopyFailure::Write)?;
    }
}

/// Remove the empty directories above `path`'s file, up to but not including `stop`: what is left
/// behind when a file this import created is taken back. A directory with anything in it stays.
fn prune_empty_dirs(path: &Path, stop: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| *d != stop && d.starts_with(stop)) {
        // `remove_dir` refuses a non-empty directory, which is exactly where to stop.
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn unit_of(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(unit, _)| unit)
}

/// Add an archive's files to `data_dir`. Existing files are never replaced (they are counted as
/// `skipped_existing`); each snapshot and reference is validated as listing would; a firmware
/// entry is imported only whole (with a valid `metadata.json`, written last); sizes are limited.
/// Every entry is accounted for: imported, existing, or rejected with its reason. A filesystem
/// failure stops the import and the report says how far it got.
fn import_archive<R: Read + Seek>(data_dir: &Path, archive: &mut ZipArchive<R>, limits: &ImportLimits) -> ImportReport {
    let mut report = ImportReport::default();
    let mut plan: Vec<(usize, EntryKind, String)> = Vec::new();
    for i in 0..archive.len() {
        let Some(name) = archive.name_for_index(i).map(str::to_string) else {
            report.rejected.push(Rejected { name: format!("entry #{i}"), reason: RejectReason::Unreadable, detail: "no readable name".into() });
            continue;
        };
        // A directory entry carries no data.
        if name.ends_with('/') || name.ends_with('\\') {
            continue;
        }
        match classify_zip_entry(&name) {
            Some(kind) => plan.push((i, kind, name)),
            None => report.rejected.push(Rejected { name, reason: RejectReason::BadName, detail: String::new() }),
        }
    }
    plan.sort_by_key(|(i, kind, _)| (kind.rank(), *i));

    // Firmware metadata first, in memory: an entry is only imported with a valid one.
    let mut metas: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    let mut meta_judged: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (i, _, name) in plan.iter().filter(|(_, k, _)| *k == EntryKind::FirmwareMeta) {
        meta_judged.insert(*i);
        match read_json_entry(archive, *i, limits.json_entry) {
            Ok(bytes) => match parse_firmware_meta_bytes(&bytes, unit_of(name)) {
                Ok(_) => {
                    metas.entry(unit_of(name).to_string()).or_insert(bytes);
                }
                Err(e) => report.rejected.push(Rejected { name: name.clone(), reason: RejectReason::InvalidMetadata, detail: e }),
            },
            Err((reason, detail)) => report.rejected.push(Rejected { name: name.clone(), reason, detail }),
        }
    }

    let total_entries = plan.len();
    // Files this import created per firmware entry, so a rejected sibling can take them back.
    let mut created: std::collections::HashMap<String, Vec<(PathBuf, u64)>> = std::collections::HashMap::new();
    let mut failed_units: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (n, (i, kind, name)) in plan.iter().enumerate() {
        let unit = unit_of(name);
        if meta_judged.contains(i) && !metas.contains_key(unit) {
            continue; // rejected above, already counted
        }
        let outcome = if kind.is_firmware() && !metas.contains_key(unit) {
            Ok(reject(RejectReason::NoMetadata, ""))
        } else if kind.is_firmware() && failed_units.contains(unit) {
            Ok(reject(RejectReason::EntryIncomplete, ""))
        } else {
            import_one(data_dir, archive, *i, *kind, name, limits, report.bytes, metas.get(unit))
        };
        match outcome {
            Ok(ImportOutcome::Imported(bytes)) => {
                report.bytes += bytes;
                match kind {
                    EntryKind::Snapshot => report.snapshots += 1,
                    EntryKind::Reference => report.references += 1,
                    _ => {
                        report.firmware += 1;
                        created.entry(unit.to_string()).or_default().push((entry_path(data_dir, name), bytes));
                    }
                }
            }
            Ok(ImportOutcome::Existing) => report.skipped_existing += 1,
            Ok(ImportOutcome::Rejected(reason, detail)) => {
                report.rejected.push(Rejected { name: name.clone(), reason, detail });
                if kind.is_firmware() && reason != RejectReason::NoMetadata && failed_units.insert(unit.to_string()) {
                    // Take back what this import already wrote of the entry.
                    for (path, bytes) in created.remove(unit).unwrap_or_default() {
                        if let Err(e) = fs::remove_file(&path) {
                            report.aborted = Some(format!("removing {} (part of a rejected firmware entry): {e}", path.display()));
                            report.not_attempted = total_entries - n - 1;
                            return report;
                        }
                        prune_empty_dirs(&path, data_dir);
                        report.firmware -= 1;
                        report.bytes -= bytes;
                    }
                }
            }
            Err(abort) => {
                report.aborted = Some(abort);
                report.not_attempted = total_entries - n - 1;
                break;
            }
        }
    }
    report
}

/// Where an archive entry lands. `name` passed `classify_zip_entry`: every component is plain,
/// so the result stays inside `data_dir`.
fn entry_path(data_dir: &Path, name: &str) -> PathBuf {
    name.split('/').fold(data_dir.to_path_buf(), |p, c| p.join(c))
}

/// Read a small entry into memory, refusing one over `limit` bytes however the header declared it.
fn read_json_entry<R: Read + Seek>(archive: &mut ZipArchive<R>, index: usize, limit: u64) -> Result<Vec<u8>, (RejectReason, String)> {
    let mut entry = archive.by_index(index).map_err(|e| (RejectReason::Unreadable, e.to_string()))?;
    if !entry.is_file() {
        return Err((RejectReason::NotAFile, String::new()));
    }
    if entry.size() > limit {
        return Err((RejectReason::TooLarge, format!("{} bytes declared, limit {limit}", entry.size())));
    }
    let mut buf = Vec::new();
    entry.by_ref().take(limit + 1).read_to_end(&mut buf).map_err(|e| (RejectReason::Unreadable, e.to_string()))?;
    if buf.len() as u64 > limit {
        return Err((RejectReason::TooLarge, format!("more than {limit} bytes"))) ;
    }
    Ok(buf)
}

#[allow(clippy::too_many_arguments)]
fn import_one<R: Read + Seek>(
    data_dir: &Path,
    archive: &mut ZipArchive<R>,
    index: usize,
    kind: EntryKind,
    name: &str,
    limits: &ImportLimits,
    total_so_far: u64,
    meta_bytes: Option<&Vec<u8>>,
) -> Result<ImportOutcome, String> {
    let target = entry_path(data_dir, name);
    let exists = |p: &Path| fs::symlink_metadata(p).is_ok();
    // Already there: kept. A firmware entry is a unit: once its metadata.json exists it is whole.
    if exists(&target) || (kind.is_firmware() && target.parent().is_some_and(|d| exists(&d.join("metadata.json")))) {
        return Ok(ImportOutcome::Existing);
    }

    if kind == EntryKind::FirmwareMeta {
        // Validated and held in memory before any file of the entry was written.
        let bytes = meta_bytes.expect("a firmware entry is only imported with its validated metadata");
        if total_so_far + bytes.len() as u64 > limits.total {
            return Ok(reject(RejectReason::TotalLimit, ""));
        }
        return Ok(match create_new_with(&target, |f| f.write_all(bytes))? {
            CreateOutcome::Created => ImportOutcome::Imported(bytes.len() as u64),
            CreateOutcome::Exists => ImportOutcome::Existing,
        });
    }

    if kind == EntryKind::FirmwareBin {
        let mut entry = match archive.by_index(index) {
            Ok(e) => e,
            Err(e) => return Ok(reject(RejectReason::Unreadable, e.to_string())),
        };
        if !entry.is_file() {
            return Ok(reject(RejectReason::NotAFile, ""));
        }
        if entry.size() > limits.firmware_entry {
            return Ok(reject(RejectReason::TooLarge, format!("{} bytes declared, limit {}", entry.size(), limits.firmware_entry)));
        }
        if total_so_far.saturating_add(entry.size()) > limits.total {
            return Ok(reject(RejectReason::TotalLimit, format!("{} bytes declared", entry.size())));
        }
        let cap = limits.firmware_entry.min(limits.total.saturating_sub(total_so_far));
        let mut failure: Option<CopyFailure> = None;
        let mut copied = 0u64;
        let created = create_new_with(&target, |file| match copy_capped(&mut entry, file, cap) {
            Ok(n) => {
                copied = n;
                Ok(())
            }
            Err(CopyFailure::Write(e)) => Err(e),
            Err(other) => {
                // Not a destination failure: remove what was written and report the entry.
                failure = Some(other);
                Err(std::io::Error::other("entry rejected"))
            }
        });
        let outcome = match (created, failure) {
            (_, Some(CopyFailure::TooBig)) => {
                let reason = if cap < limits.firmware_entry { RejectReason::TotalLimit } else { RejectReason::TooLarge };
                Ok(reject(reason, "more bytes arrived than the limit allows"))
            }
            (_, Some(CopyFailure::Read(e))) => Ok(reject(RejectReason::Unreadable, e.to_string())),
            (_, Some(CopyFailure::Write(_))) => unreachable!("write failures are returned, not parked"),
            (Err(e), None) => Err(e),
            (Ok(CreateOutcome::Exists), None) => Ok(ImportOutcome::Existing),
            (Ok(CreateOutcome::Created), None) if copied == 0 => {
                // `create_new_with` made the file; an empty firmware is not an archive entry.
                let _ = fs::remove_file(&target);
                Ok(reject(RejectReason::EmptyFirmware, ""))
            }
            (Ok(CreateOutcome::Created), None) => {
                // Only imported with its metadata (checked and parsed before any write).
                let meta = meta_bytes
                    .and_then(|b| parse_firmware_meta_bytes(b, unit_of(name)).ok())
                    .expect("a firmware binary is only imported with its validated metadata");
                match verify_binary(&target, copied, &meta) {
                    Ok(()) => Ok(ImportOutcome::Imported(copied)),
                    Err(detail) => {
                        // Not what the metadata describes: take the file back and reject the entry.
                        fs::remove_file(&target).map_err(|e| format!("removing {}: {e}", target.display()))?;
                        Ok(reject(RejectReason::ChecksumMismatch, detail))
                    }
                }
            }
        };
        if matches!(outcome, Ok(ImportOutcome::Rejected(..))) {
            // The directories made for the file are of no use now.
            prune_empty_dirs(&target, data_dir);
        }
        return outcome;
    }

    // Snapshot, reference, source page: small, read whole, judged before anything is written.
    let bytes = match read_json_entry(archive, index, limits.json_entry) {
        Ok(b) => b,
        Err((reason, detail)) => return Ok(reject(reason, detail)),
    };
    if total_so_far + bytes.len() as u64 > limits.total {
        return Ok(reject(RejectReason::TotalLimit, ""));
    }
    if matches!(kind, EntryKind::Snapshot | EntryKind::Reference) {
        let snap = match parse_snapshot_bytes(&bytes) {
            Ok(s) => s,
            Err(e) => return Ok(reject(RejectReason::InvalidSnapshot, e)),
        };
        // A reference is found by the name its own content gives; under any other name it is dead weight.
        if kind == EntryKind::Reference {
            let want = checked_reference_filename(&snap.camera.model, &snap.camera.serial).ok();
            if want.as_deref() != name.strip_prefix("references/") {
                return Ok(reject(
                    RejectReason::ReferenceNameMismatch,
                    format!("{:?} {:?} belongs in {}", snap.camera.model, snap.camera.serial, want.as_deref().unwrap_or("no valid file name")),
                ));
            }
        }
    }
    Ok(match create_new_with(&target, |f| f.write_all(&bytes))? {
        CreateOutcome::Created => ImportOutcome::Imported(bytes.len() as u64),
        CreateOutcome::Exists => ImportOutcome::Existing,
    })
}

fn do_import(data_dir: &Path, src: &Path) -> Result<ImportReport, String> {
    let file = fs::File::open(src).map_err(|e| format!("opening {}: {e}", src.display()))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("{} is not a readable zip archive: {e}", src.display()))?;
    Ok(import_archive(data_dir, &mut archive, &ImportLimits::default()))
}

fn describe_import(r: &ImportReport) -> OpDone {
    let mut head = format!(
        "Imported {} snapshot(s), {} reference(s), {} firmware file(s) ({}); {} skipped because they already exist (kept, never replaced); {} rejected",
        r.snapshots,
        r.references,
        r.firmware,
        mib(r.bytes),
        r.skipped_existing,
        r.rejected.len()
    );
    let mut notices: Vec<String> = Vec::new();
    let level = if let Some(e) = &r.aborted {
        head = format!("Import STOPPED by an error after {head}");
        notices.push(format!("The import stopped: {e}. {} further entr{} were not attempted.", r.not_attempted, if r.not_attempted == 1 { "y" } else { "ies" }));
        Level::Error
    } else if !r.rejected.is_empty() {
        Level::Warn
    } else {
        Level::Info
    };
    notices.extend(list_notices(r.rejected.iter().map(|x| {
        if x.detail.is_empty() {
            format!("Rejected {}: {}", x.name, x.reason.text())
        } else {
            format!("Rejected {}: {} ({})", x.name, x.reason.text(), x.detail)
        }
    })));
    OpDone { status: Status::new(level, format!("{head}.")), notices, cli_log: String::new(), refresh_listing: true }
}

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 520.0])
            .with_title("Fleet — Nikon Camera Manager"),
        ..Default::default()
    };
    eframe::run_native("fleet-gui", opts, Box::new(|cc| Ok(Box::new(FleetApp::new(cc))))).unwrap();
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_does_not_follow_the_selection_to_another_camera() {
        // The bug: a label typed for the Z 9 stayed in the box after clicking the Z 5.
        assert_eq!(label_after_selection(Some(0), 1, "z9a-reference".into()), "");
        assert_eq!(label_after_selection(Some(1), 0, "x".into()), "");
        // Nothing was selected before: the box is cleared too (it was typed for no camera).
        assert_eq!(label_after_selection(None, 0, "x".into()), "");
        // Clicking the camera that is already selected keeps what was typed.
        assert_eq!(label_after_selection(Some(1), 1, "z9a-reference".into()), "z9a-reference");
        // An empty label stays empty either way.
        assert_eq!(label_after_selection(Some(0), 1, String::new()), "");
    }
    use nikon_fleet::ops::baseline::{GateOutcome, NoBaselineReason};
    use nikon_fleet::ops::capture::CaptureStats;
    use nikon_fleet::ops::capture_snapshot::CaptureOutcome;
    use nikon_fleet::ops::cameras::{describe_device, describe_scan};
    use nikon_fleet::sdk::{DeviceInfo, UsbCameraInfo, UsbScan, UsbStage, UsbUnreadable};
    use nikon_fleet::snapshot::{Camera, Transport};
    use std::collections::HashMap;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const T: Duration = Duration::from_secs(30);

    // ── helpers ────────────────────────────────────────────────────────────

    /// A path that is absolute on this platform: `/x` is only relative on Windows, where the
    /// settings code refuses it.
    pub(crate) fn abs_path(unix: &str) -> String {
        if cfg!(windows) { format!("C:{}", unix.replace('/', "\\")) } else { unix.to_string() }
    }

    #[cfg(unix)]
    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    /// A stand-in for the CLI: records its argv (one per line) and stdin next to it, then runs `tail`.
    pub(crate) fn recording_cli(dir: &Path, tail: &str) -> PathBuf {
        script(
            dir,
            "nikon-fleet",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{d}/argv.txt'\ncat > '{d}/stdin.txt'\n{tail}",
                d = dir.display()
            ),
        )
    }

    pub(crate) fn argv_of(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("argv.txt")).unwrap().lines().map(str::to_string).collect()
    }

    fn snapshot_bytes(model: &str, serial: &str, captured_at: &str, label: &str) -> Vec<u8> {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.00".into() },
            Transport::Usb,
            captured_at.into(),
        );
        s.label = Some(label.into());
        s.insert("kNkMAIDCapability_A", 1, serde_json::json!(1));
        s.to_pretty_json().unwrap().into_bytes()
    }

    pub(crate) fn write_snapshot(dir: &Path, file: &str, model: &str, serial: &str, captured_at: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), snapshot_bytes(model, serial, captured_at, "test")).unwrap();
    }

    fn sha_of(bytes: &[u8]) -> String {
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::write(f.path(), bytes).unwrap();
        sha256_file(f.path()).unwrap()
    }

    /// Valid metadata for the binary `bin`, as the archive records it.
    fn meta_bytes(model: &str, version: &str, bin: &[u8]) -> Vec<u8> {
        let meta = FirmwareMeta {
            format_version: FIRMWARE_META_FORMAT_VERSION,
            model: model.into(),
            firmware_version: version.into(),
            archived_at: "2026-01-01T00:00:00Z".into(),
            archived_by: "test".into(),
            bin_sha256: sha_of(bin),
            bin_size_bytes: bin.len() as u64,
            source_path: "x".into(),
            canonical_snapshot_path: None,
            notes: None,
            provenance: None,
        };
        serde_json::to_vec_pretty(&meta).unwrap()
    }

    fn firmware_unit(data: &Path, model: &str, slug: &str, version: &str, bin: &[u8]) {
        let dir = data.join("firmware").join(slug).join(version);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("firmware.bin"), bin).unwrap();
        fs::write(dir.join("metadata.json"), meta_bytes(model, version, bin)).unwrap();
        fs::write(dir.join("source-page.html"), b"<html></html>").unwrap();
    }

    pub(crate) fn row(model: &str, serial: &str, model_count: usize) -> CameraRow {
        CameraRow {
            model: model.into(),
            serial: serial.into(),
            firmware: "5.31".into(),
            known_model: true,
            baseline: true,
            model_count,
        }
    }

    pub(crate) fn usb(model: &str, serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: model.into() }
    }

    pub(crate) fn scan_of(cameras: Vec<UsbCameraInfo>) -> CameraList {
        describe_scan(&UsbScan { cameras, ..UsbScan::default() })
    }

    fn unreadable_usb() -> UsbUnreadable {
        UsbUnreadable { bus: 3, address: 7, stage: UsbStage::Open, error: "Access denied".into() }
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> ZipArchive<Cursor<Vec<u8>>> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(bytes).unwrap();
        }
        ZipArchive::new(Cursor::new(w.finish().unwrap().into_inner())).unwrap()
    }

    fn import(data: &Path, entries: &[(&str, &[u8])], limits: &ImportLimits) -> ImportReport {
        import_archive(data, &mut make_zip(entries), limits)
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures")
    }

    // ── Settings (T11): the file's rules are nikon_fleet::settings'; these pin the GUI's use ──

    fn write_settings(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("net.blw.fleet").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_missing_settings_file_means_the_default_directory_and_no_error() {
        let d = TempDir::new().unwrap();
        let s = load_settings_state(&d.path().join("nope.json"));
        assert_eq!(s.error, None);
        assert_eq!(s.data_dir, Some(settings::default_data_dir()));
        assert_eq!(s.configured, None);
    }

    #[test]
    fn a_blank_data_dir_is_the_default_in_every_spelling() {
        let d = TempDir::new().unwrap();
        for blank in [r#""""#, r#""   ""#, r#""\t""#, "null"] {
            let path = write_settings(d.path(), &format!(r#"{{"data_dir": {blank}}}"#));
            let s = load_settings_state(&path);
            assert_eq!(s.error, None, "{blank}");
            assert_eq!(s.data_dir, Some(settings::default_data_dir()), "{blank}");
            assert_eq!(s.configured, None, "{blank}");
        }
    }

    #[test]
    fn a_configured_data_dir_is_used_as_written() {
        let d = TempDir::new().unwrap();
        let s = load_settings_state(&write_settings(d.path(), r#"{"data_dir": "/data/fleet"}"#));
        assert_eq!(s.data_dir, Some(PathBuf::from("/data/fleet")));
        assert_eq!(s.configured.as_deref(), Some("/data/fleet"));
    }

    #[test]
    fn a_settings_file_that_cannot_be_parsed_is_an_error_not_a_default() {
        let d = TempDir::new().unwrap();
        for bad in ["{not json", "[1,2]", r#"{"data_dir": 5}"#] {
            let path = write_settings(d.path(), bad);
            let s = load_settings_state(&path);
            assert_eq!(s.data_dir, None, "{bad}: no directory is guessed");
            let err = s.error.clone().expect(bad);
            assert!(err.contains("settings.json"), "{err}");
            // What the GUI shows, and what its worker answers a data command with.
            assert_eq!(settings_error_status(&s).unwrap().level, Level::Error);
            assert!(s.data_dir_or_err().unwrap_err().contains("unusable"));
        }
    }

    #[test]
    fn a_healthy_settings_file_shows_no_error_status() {
        let d = TempDir::new().unwrap();
        let s = load_settings_state(&write_settings(d.path(), "{}"));
        assert_eq!(settings_error_status(&s), None);
        assert!(s.data_dir_or_err().is_ok());
    }

    #[test]
    fn saving_the_data_dir_keeps_every_key_it_does_not_own() {
        let d = TempDir::new().unwrap();
        let path = write_settings(d.path(), r#"{"data_dir": "/old", "window": {"w": 800}, "theme": "dark"}"#);
        let new_dir = abs_path("/new/dir");
        let s = save_data_dir(&path, &format!("  {new_dir}  ")).unwrap();
        assert_eq!(s.data_dir, Some(PathBuf::from(&new_dir)), "pasted whitespace is trimmed");
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["data_dir"], new_dir.as_str());
        assert_eq!(v["window"]["w"], 800);
        assert_eq!(v["theme"], "dark");
    }

    #[test]
    fn saving_a_blank_data_dir_clears_it_and_keeps_the_other_keys() {
        let d = TempDir::new().unwrap();
        let path = write_settings(d.path(), r#"{"data_dir": "/old", "theme": "dark"}"#);
        let s = save_data_dir(&path, "   ").unwrap();
        assert_eq!((s.configured, s.data_dir), (None, Some(settings::default_data_dir())));
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v.get("data_dir").is_none() && v["theme"] == "dark", "{v}");
    }

    #[test]
    fn saving_never_overwrites_a_settings_file_it_could_not_parse() {
        let d = TempDir::new().unwrap();
        let path = write_settings(d.path(), "{ half written");
        let err = save_data_dir(&path, &abs_path("/new")).unwrap_err();
        assert!(err.contains("left as it is"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ half written");
    }

    #[test]
    fn saving_creates_the_file_when_there_is_none_and_refuses_a_relative_path() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("new").join("settings.json");
        let err = save_data_dir(&path, "relative/dir").unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
        assert!(!path.exists(), "a refused value writes nothing");
        // The boundary: an absolute path is accepted.
        let abs = abs_path("/abs");
        assert_eq!(save_data_dir(&path, &abs).unwrap().data_dir, Some(PathBuf::from(&abs)));
        assert!(path.exists());
    }

    // ── listing ─────────────────────────────────────────────────────────────

    #[test]
    fn listing_a_missing_directory_is_empty_and_not_a_problem() {
        let d = TempDir::new().unwrap();
        let l = list_all(d.path());
        assert!(!l.dir_exists && l.rows.is_empty() && l.error.is_none());
        assert!(l.problems().is_empty());
    }

    #[test]
    fn an_unreadable_snapshots_directory_is_reported_not_shown_as_no_snapshots() {
        let d = TempDir::new().unwrap();
        fs::write(d.path().join("snapshots"), "i am a file").unwrap();
        let l = list_all(d.path());
        assert!(l.rows.is_empty());
        let e = l.error.clone().expect("listing a file as a directory is an error");
        assert!(l.problems().iter().any(|p| p.contains("could not be listed") && p.contains(&e)), "{:?}", l.problems());
    }

    #[test]
    fn listing_matches_the_shared_validity_fixture() {
        let root = fixtures().join("snapshot_validity");
        let expected: HashMap<String, String> =
            serde_json::from_str(&fs::read_to_string(root.join("expected.json")).unwrap()).unwrap();
        assert!(expected.len() >= 8, "the fixture is the point of this test");
        let l = list_all(&root);
        let ok: Vec<&str> = l.rows.iter().map(|r| r.file.as_str()).collect();
        let bad: HashMap<&str, &str> = l.unreadable.iter().map(|u| (u.file.as_str(), u.error_kind)).collect();
        for (file, want) in &expected {
            if want == "ok" {
                assert!(ok.contains(&file.as_str()), "{file} must list as ok; ok={ok:?} bad={bad:?}");
            } else {
                assert_eq!(bad.get(file.as_str()), Some(&want.as_str()), "{file}");
            }
        }
        assert_eq!(ok.len() + bad.len(), expected.len(), "every file accounted for");
        assert!(l.problems().iter().any(|p| p.contains("not valid snapshots")), "{:?}", l.problems());
    }

    #[test]
    fn listing_matches_the_shared_reference_parity_fixture() {
        // Same directory and expectations as ops::list's test and gui/test_fleet_lib.py.
        let fx = fixtures().join("reference_parity");
        let expected: HashMap<String, HashMap<String, bool>> =
            serde_json::from_str(&fs::read_to_string(fx.join("expected.json")).unwrap()).unwrap();
        assert!(!expected.is_empty());
        let l = list_all(&fx);
        for (serial, want) in &expected {
            let got: HashMap<String, bool> =
                l.rows.iter().filter(|r| &r.serial == serial).map(|r| (r.file.clone(), r.is_reference)).collect();
            assert_eq!(&got, want, "serial {serial:?}");
        }
        assert!(l.unreadable.iter().any(|u| u.file == "broken.json"));
    }

    fn stored(file: &str, model: &str, serial: &str) -> SnapRow {
        SnapRow {
            file: file.into(),
            model: model.into(),
            serial: serial.into(),
            label: None,
            captured_at: "2026-01-01T00:00:00Z".into(),
            is_reference: false,
            name_lossy: false,
        }
    }

    #[test]
    fn with_no_camera_attached_every_stored_body_is_offered_once_in_order() {
        let l = SnapListing {
            rows: vec![
                stored("1.json", "Z 9", "S2"),
                stored("2.json", "Z 5", "S1"),
                stored("3.json", "Z 9", "S2"),
                stored("4.json", "Z 9", "S1"),
            ],
            ..SnapListing::default()
        };
        let want: Vec<(String, String)> =
            [("Z 5", "S1"), ("Z 9", "S1"), ("Z 9", "S2")].iter().map(|(m, s)| (m.to_string(), s.to_string())).collect();
        assert_eq!(l.stored_only_bodies(&[]), want, "sorted by model then serial, two files of one body counted once");
        assert!(SnapListing::default().stored_only_bodies(&[]).is_empty(), "nothing stored, nothing offered");
    }

    #[test]
    fn a_connected_body_is_not_offered_again_but_the_same_model_or_serial_alone_does_not_hide_one() {
        let l = SnapListing {
            rows: vec![
                stored("a.json", "Z 9", "S1"),
                stored("b.json", "Z 9", "S2"),
                stored("c.json", "Z 5", "S1"),
            ],
            ..SnapListing::default()
        };
        let connected = [row("Z 9", "S1", 1)];
        let got = l.stored_only_bodies(&connected);
        assert_eq!(
            got,
            [("Z 5".to_string(), "S1".to_string()), ("Z 9".to_string(), "S2".to_string())],
            "Z 9/S1 is connected; the other Z 9 (S2) and the Z 5 with the same serial are still stored-only"
        );
    }

    #[test]
    fn rows_are_newest_first_by_capture_time_with_ties_by_file_name() {
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        // File-name order is a, b, c, d, e; time order is e, c, b/d (tied), a.
        write_snapshot(&snaps, "a_oldest.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "b_tie.json", "Z 9", "S1", "2026-05-05T00:00:00Z");
        write_snapshot(&snaps, "c_mid.json", "Z 9", "S1", "2026-06-06T00:00:00Z");
        write_snapshot(&snaps, "d_tie.json", "Z 9", "S1", "2026-05-05T00:00:00Z");
        write_snapshot(&snaps, "e_newest.json", "Z 9", "S1", "2026-09-01T00:00:00Z");
        fs::write(snaps.join("0_broken.json"), "{ nope").unwrap();
        let l = list_all(d.path());
        let files: Vec<&str> = l.rows_for("Z 9", "S1").map(|r| r.file.as_str()).collect();
        assert_eq!(files, ["e_newest.json", "c_mid.json", "b_tie.json", "d_tie.json", "a_oldest.json"]);
        // Unreadable files are not rows at all; they are listed apart, after the rows.
        assert_eq!(l.unreadable.iter().map(|u| u.file.as_str()).collect::<Vec<_>>(), ["0_broken.json"]);
    }

    #[test]
    fn rows_are_filtered_by_model_and_serial_together() {
        // `id-1` is any model's first body without a USB serial.
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        write_snapshot(&snaps, "z9.json", "Z 9", "id-1", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "z6.json", "Z6_3", "id-1", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "other.json", "Z 9", "ABC", "2026-01-01T00:00:00Z");
        let l = list_all(d.path());
        assert_eq!(l.rows_for("Z 9", "id-1").map(|r| r.file.as_str()).collect::<Vec<_>>(), ["z9.json"]);
        assert_eq!(l.rows_for("Z6_3", "id-1").map(|r| r.file.as_str()).collect::<Vec<_>>(), ["z6.json"]);
        assert_eq!(l.rows_for("Z 9", "nope").count(), 0);
    }

    #[test]
    fn the_reference_mark_is_byte_identity_with_the_reference_file() {
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        write_snapshot(&snaps, "ref_candidate.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "other.json", "Z 9", "ABC123", "2026-06-01T00:00:00Z");
        set_reference(d.path(), "ref_candidate.json").unwrap();
        let l = list_all(d.path());
        let by = |n: &str| l.rows.iter().find(|r| r.file == n).unwrap().is_reference;
        assert!(by("ref_candidate.json") && !by("other.json"));
        // An edited reference matches nothing.
        let ref_path = d.path().join("references/Z_9_ABC123.json");
        let mut text = fs::read_to_string(&ref_path).unwrap();
        text.push('\n');
        fs::write(&ref_path, text).unwrap();
        assert!(list_all(d.path()).rows.iter().all(|r| !r.is_reference));
    }

    #[test]
    fn a_file_name_that_was_converted_lossily_is_flagged_and_counted() {
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        write_snapshot(&snaps, "bad\u{FFFD}name.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "fine.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        let l = list_all(d.path());
        assert_eq!(l.rows.iter().filter(|r| r.name_lossy).count(), 1);
        assert!(l.problems().iter().any(|p| p.contains("not valid UTF-8")), "{:?}", l.problems());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_reference_file_is_counted_not_hidden() {
        let d = TempDir::new().unwrap();
        write_snapshot(&d.path().join("snapshots"), "a.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        let refs = d.path().join("references");
        fs::create_dir_all(&refs).unwrap();
        fs::write(refs.join("Z_9_S1.json"), "x").unwrap();
        fs::set_permissions(refs.join("Z_9_S1.json"), fs::Permissions::from_mode(0o000)).unwrap();
        let l = list_all(d.path());
        fs::set_permissions(refs.join("Z_9_S1.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(l.reference_read_failures, 1);
        assert!(l.problems().iter().any(|p| p.contains("could not be read")), "{:?}", l.problems());
    }

    // ── set_reference (G36) ─────────────────────────────────────────────────

    #[test]
    fn a_reference_is_a_byte_copy_under_the_canonical_name() {
        let d = TempDir::new().unwrap();
        write_snapshot(&d.path().join("snapshots"), "orig.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        let r = set_reference(d.path(), "orig.json").unwrap();
        assert_eq!((r.reference_file.as_str(), &r.previous), ("Z_9_ABC123.json", &PreviousReference::None));
        assert_eq!(
            fs::read(d.path().join("references/Z_9_ABC123.json")).unwrap(),
            fs::read(d.path().join("snapshots/orig.json")).unwrap()
        );
    }

    #[test]
    fn replacing_a_reference_is_reported_and_re_setting_the_same_one_is_a_no_op() {
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        fs::create_dir_all(&snaps).unwrap();
        fs::write(snaps.join("a.json"), snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "first")).unwrap();
        fs::write(snaps.join("b.json"), snapshot_bytes("Z 9", "S1", "2026-02-02T00:00:00Z", "second")).unwrap();
        set_reference(d.path(), "a.json").unwrap();
        let again = set_reference(d.path(), "a.json").unwrap();
        assert_eq!(again.previous, PreviousReference::Identical);
        assert!(describe_reference(&again).status.text.contains("already the reference"));
        let replaced = set_reference(d.path(), "b.json").unwrap();
        assert_eq!(
            replaced.previous,
            PreviousReference::Replaced { captured_at: Some("2026-01-01T00:00:00Z".into()), label: Some("first".into()) }
        );
        let text = describe_reference(&replaced).status.text;
        assert!(text.contains("replaced the previous reference") && text.contains("2026-01-01") && text.contains("first"), "{text}");
        assert_eq!(fs::read(d.path().join("references/Z_9_S1.json")).unwrap(), fs::read(snaps.join("b.json")).unwrap());
    }

    #[test]
    fn a_hostile_serial_cannot_steer_the_reference_out_of_its_place() {
        let d = TempDir::new().unwrap();
        // `references/Z_9_x/y.json` is where a naive join would put serial `x/y`.
        fs::create_dir_all(d.path().join("references/Z_9_x")).unwrap();
        write_snapshot(&d.path().join("snapshots"), "h.json", "Z 9", "x/y", "2026-01-01T00:00:00Z");
        let e = set_reference(d.path(), "h.json").unwrap_err();
        assert!(e.contains("cannot be a reference") && e.contains("plain file name"), "{e}");
        assert!(!d.path().join("references/Z_9_x/y.json").exists());
    }

    #[test]
    fn a_hostile_model_is_refused_before_anything_is_created() {
        let d = TempDir::new().unwrap();
        write_snapshot(&d.path().join("snapshots"), "h.json", "..", "S1", "2026-01-01T00:00:00Z");
        assert!(set_reference(d.path(), "h.json").is_err());
        assert!(!d.path().join("references").exists());
    }

    #[test]
    fn set_reference_refuses_names_that_are_not_plain_and_files_that_are_not_snapshots() {
        let d = TempDir::new().unwrap();
        fs::create_dir_all(d.path().join("snapshots")).unwrap();
        fs::write(d.path().join("snapshots/broken.json"), "{ nope").unwrap();
        assert!(set_reference(d.path(), "../snapshots/broken.json").unwrap_err().contains("not a plain"));
        assert!(set_reference(d.path(), "").is_err());
        assert!(set_reference(d.path(), "broken.json").unwrap_err().contains("loading"));
        assert!(!d.path().join("references").exists());
    }

    #[test]
    fn an_existing_reference_that_cannot_be_read_is_left_alone() {
        let d = TempDir::new().unwrap();
        write_snapshot(&d.path().join("snapshots"), "a.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        fs::create_dir_all(d.path().join("references/Z_9_S1.json")).unwrap(); // a directory in the way
        let e = set_reference(d.path(), "a.json").unwrap_err();
        assert!(e.contains("could not be read") && e.contains("left as it is"), "{e}");
    }

    // ── running the CLI ─────────────────────────────────────────────────────

    #[cfg(unix)]
    fn sh(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("/bin/sh"), vec!["-c".to_string(), script.to_string()])
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_returns_trimmed_stdout_on_success() {
        let (p, a) = sh("echo '  hello  '");
        assert_eq!(run_cli(&p, &a, None, T).unwrap().stdout, "hello");
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_pipes_stdin_exactly_and_closes_it() {
        // `wc -c` only finishes once stdin hits EOF, so this also proves the
        // pipe is closed; and 9 bytes proves no newline was appended.
        let (p, a) = sh("wc -c");
        assert_eq!(run_cli(&p, &a, Some("pa ss--wd"), T).unwrap().stdout, "9");
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_non_zero_exit_returns_stderr_as_a_typed_exit_failure() {
        let (p, a) = sh("echo 'boom: no camera' >&2; exit 3");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (CliFailure::Exit, "boom: no camera"));
        assert!(!e.outcome_unknown());
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_non_zero_exit_with_empty_stderr_reports_the_status() {
        let (p, a) = sh("exit 3");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert!(e.message.contains("exited with") && e.message.contains('3'), "{}", e.message);
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_child_killed_by_signal_is_an_error_not_a_gui_crash() {
        // The point of #12: if the USB churn takes the child down, the GUI
        // gets a message instead of dying with it.
        let (p, a) = sh("kill -SEGV $$");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert!(e.message.contains("signal"), "{}", e.message);
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_stderr_on_success_reaches_the_caller() {
        let (p, a) = sh("echo 'warning: unconfirmed model' >&2; echo ok");
        let out = run_cli(&p, &a, None, T).unwrap();
        assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("ok", "warning: unconfirmed model"));
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_child_that_exits_without_reading_stdin_reports_its_own_error() {
        // The stdin write fails (EPIPE) — a non-zero child's stderr must still win.
        let (p, a) = sh("echo 'rejected: bad args' >&2; exit 2");
        let e = run_cli(&p, &a, Some(&"x".repeat(1_000_000)), T).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (CliFailure::Exit, "rejected: bad args"));
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_a_failed_stdin_write_fails_the_call_even_when_the_child_exits_zero() {
        // G41: a truncated password must not be reported as a success.
        let (p, a) = sh("exit 0");
        let e = run_cli(&p, &a, Some(&"x".repeat(1_000_000)), T).unwrap_err();
        assert_eq!(e.kind, CliFailure::StdinWrite);
        assert!(e.message.contains("could not be given its input"), "{}", e.message);
        // The boundary: the same child that reads its input succeeds.
        let (p, a) = sh("cat > /dev/null");
        assert!(run_cli(&p, &a, Some(&"x".repeat(1_000_000)), T).is_ok());
    }

    #[test]
    fn run_cli_missing_program_is_a_clear_error() {
        let e = run_cli(Path::new("/nonexistent/nikon-fleet"), &[], None, T).unwrap_err();
        assert_eq!(e.kind, CliFailure::Spawn);
        assert!(e.message.contains("could not run") && e.message.contains("/nonexistent/nikon-fleet"), "{}", e.message);
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_kills_a_child_that_outlives_the_timeout_and_keeps_what_it_printed() {
        // G33: a hung CLI must not wedge the worker. `exec` so the killed process is the sleeper.
        let d = TempDir::new().unwrap();
        let pidfile = d.path().join("pid");
        let (p, a) = sh(&format!("echo $$ > '{}'; echo partial-output; exec sleep 30", pidfile.display()));
        let started = Instant::now();
        let e = run_cli(&p, &a, None, Duration::from_millis(3000)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(15), "returned promptly, not after the sleep");
        assert_eq!(e.kind, CliFailure::TimedOut);
        assert!(e.outcome_unknown() && e.message.contains("did not finish"), "{}", e.message);
        assert_eq!(e.stdout, "partial-output");
        let pid = fs::read_to_string(&pidfile).unwrap();
        assert!(!alive(pid.trim()), "the child was killed and reaped");
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_timeout_boundary_a_child_inside_the_limit_succeeds() {
        let (p, a) = sh("sleep 0.2; echo done");
        assert_eq!(run_cli(&p, &a, None, Duration::from_secs(20)).unwrap().stdout, "done");
        let (p, a) = sh("exec sleep 5");
        assert_eq!(run_cli(&p, &a, None, Duration::from_millis(300)).unwrap_err().kind, CliFailure::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_a_child_that_never_reads_its_input_still_times_out() {
        let (p, a) = sh("exec sleep 30");
        let started = Instant::now();
        let e = run_cli(&p, &a, Some(&"x".repeat(2_000_000)), Duration::from_millis(500)).unwrap_err();
        assert_eq!(e.kind, CliFailure::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[test]
    fn resolve_cli_prefers_a_valid_env_override() {
        let got = resolve_cli(Some("/opt/x/nikon-fleet".into()), Some(Path::new("/t/fleet-gui")), "", |_| true);
        assert_eq!(got, Ok(PathBuf::from("/opt/x/nikon-fleet")));
    }

    #[test]
    fn resolve_cli_bad_env_override_does_not_fall_back_to_the_sibling() {
        // A typo'd override must fail loudly even though the sibling exists.
        let got = resolve_cli(Some("/typo/nikon-fleet".into()), Some(Path::new("/t/fleet-gui")), "", |p| {
            p == Path::new("/t/nikon-fleet")
        });
        let e = got.unwrap_err();
        assert!(e.contains(CLI_ENV) && e.contains("/typo/nikon-fleet"), "{e}");
    }

    #[test]
    fn resolve_cli_finds_the_sibling_of_the_gui_executable() {
        let got = resolve_cli(None, Some(Path::new("/t/debug/fleet-gui")), "", |p| p == Path::new("/t/debug/nikon-fleet"));
        assert_eq!(got, Ok(PathBuf::from("/t/debug/nikon-fleet")));
    }

    #[test]
    fn resolve_cli_looks_for_nikon_fleet_exe_on_windows_and_says_so_when_it_is_missing() {
        let found = resolve_cli(None, Some(Path::new("C:/t/debug/fleet-gui.exe")), ".exe", |p| p == Path::new("C:/t/debug/nikon-fleet.exe"));
        assert_eq!(found, Ok(PathBuf::from("C:/t/debug/nikon-fleet.exe")));
        // The suffix-less name is not the CLI there, even if a file by that name exists.
        let e = resolve_cli(None, Some(Path::new("C:/t/debug/fleet-gui.exe")), ".exe", |p| p == Path::new("C:/t/debug/nikon-fleet")).unwrap_err();
        assert!(e.contains("nikon-fleet.exe"), "{e}");
    }

    #[test]
    fn resolve_cli_missing_sibling_says_how_to_build_it() {
        let e = resolve_cli(None, Some(Path::new("/t/debug/fleet-gui")), "", |_| false).unwrap_err();
        assert!(e.contains(&Path::new("/t/debug").join("nikon-fleet").display().to_string()) && e.contains("cargo build --workspace") && e.contains(CLI_ENV), "{e}");
    }

    #[test]
    fn resolve_cli_without_a_known_exe_path_asks_for_the_override() {
        let e = resolve_cli(None, None, "", |_| true).unwrap_err();
        assert!(e.contains(CLI_ENV), "{e}");
    }

    // ── discover (G26, G28, G42) ────────────────────────────────────────────

    const TWO_Z9: &str = r#"{"cameras":[
        {"model":"Z 9","serial":"AAA","firmware":"5.31","sdk_id":1,"usb_serial":"AAA","known_model":true,"baseline":true},
        {"model":"Z 9","serial":"BBB","firmware":"5.31","sdk_id":2,"usb_serial":"BBB","known_model":true,"baseline":true},
        {"model":"Z6_3","serial":"CCC","firmware":"2.00","sdk_id":3,"usb_serial":"CCC","known_model":true,"baseline":true}]}"#;

    #[test]
    fn discover_json_becomes_rows_with_the_same_model_counted() {
        let rows = parse_discover(TWO_Z9).unwrap();
        assert_eq!(rows.iter().map(|r| (r.serial.as_str(), r.model_count)).collect::<Vec<_>>(), [("AAA", 2), ("BBB", 2), ("CCC", 1)]);
        assert!(rows[0].pairing_ambiguous() && !rows[2].pairing_ambiguous());
    }

    #[test]
    fn an_empty_serial_never_reaches_a_row_it_becomes_the_storable_id() {
        let rows = parse_discover(
            r#"{"cameras":[{"model":"Z 9","serial":"","firmware":"5.31","sdk_id":7,"usb_serial":null,"known_model":true,"baseline":true}]}"#,
        )
        .unwrap();
        assert_eq!(rows[0].serial, "id-7");
    }

    #[test]
    fn the_cli_discover_value_and_this_parser_agree_for_every_serial_case() {
        // Built by the library's own describe_device, serialized as `discover --json` does.
        let dev = |id| DeviceInfo { id, name: "Z 9".into(), available: true, connected_pid: 0, version: String::new() };
        let with_usb = |serial: &str| Some(UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: "Z 9".into() });
        let cases = [
            (dev(1), with_usb("30012345"), "30012345"),
            (dev(2), with_usb(""), "id-2"), // an unreadable descriptor is no serial (G26)
            (dev(3), None, "id-3"),
        ];
        let cameras: Vec<_> = cases.iter().map(|(d, u, _)| describe_device(d, u)).collect();
        let json = serde_json::json!({ "cameras": cameras }).to_string();
        let rows = parse_discover(&json).unwrap();
        for (row, (_, _, want)) in rows.iter().zip(&cases) {
            assert_eq!(row.serial, *want);
            assert!(!row.serial.is_empty());
        }
    }

    #[test]
    fn discover_output_that_is_not_the_expected_json_is_an_error() {
        assert!(parse_discover("InitializeSDK Execution duration: 3").unwrap_err().contains("not the expected JSON"));
        assert!(parse_discover(r#"{"cameras":[{"model":"Z 9"}]}"#).is_err(), "a missing key is not defaulted");
        assert_eq!(parse_discover(r#"{"cameras":[]}"#).unwrap(), vec![]);
    }

    #[test]
    fn discover_notices_name_skipped_bodies_and_same_model_pairs() {
        let rows = parse_discover(TWO_Z9).unwrap();
        let scan = describe_scan(&UsbScan {
            cameras: vec![usb("Z 9", "AAA")],
            unreadable: vec![unreadable_usb()],
            enumeration_error: Some("libusb broke".into()),
        });
        let n = discover_notices(&rows, &scan);
        assert!(n.iter().any(|l| l.contains("libusb broke")), "{n:?}");
        assert!(n.iter().any(|l| l.contains("bus 3") && l.contains("address 7") && l.contains("Access denied")), "{n:?}");
        assert_eq!(n.iter().filter(|l| l.contains("enumeration order")).count(), 1, "once per model, not per body: {n:?}");
        assert!(discover_notices(&[row("Z 9", "AAA", 1)], &scan_of(vec![usb("Z 9", "AAA")])).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn discover_via_runs_the_cli_shows_its_stderr_and_adds_the_bus_notes() {
        let d = TempDir::new().unwrap();
        let cli = recording_cli(d.path(), &format!("echo 'warning: 2 \"Z 9\" bodies connected' >&2\ncat <<'EOF'\n{TWO_Z9}\nEOF"));
        let found = discover_via(&cli, &|| describe_scan(&UsbScan { unreadable: vec![unreadable_usb()], ..UsbScan::default() }), T).unwrap();
        assert_eq!(argv_of(d.path()), ["discover", "--json"]);
        assert_eq!(found.rows.len(), 3);
        assert!(found.cli_log.contains("bodies connected"), "stderr of a successful run reaches the user");
        assert!(found.notices.iter().any(|n| n.contains("could not be read")));
        assert!(found.notices.iter().any(|n| n.contains("enumeration order")));
    }

    #[cfg(unix)]
    #[test]
    fn discover_that_prints_its_result_and_then_hangs_is_taken_with_a_note() {
        // F24: the SDK's close hangs after the result is out.
        let d = TempDir::new().unwrap();
        let cli = recording_cli(d.path(), &format!("cat <<'EOF'\n{TWO_Z9}\nEOF\nexec sleep 30"));
        let found = discover_via(&cli, &|| scan_of(vec![]), Duration::from_millis(4000)).unwrap();
        assert_eq!(found.rows.len(), 3);
        assert!(found.notices.iter().any(|n| n.contains("did not exit")), "{:?}", found.notices);
    }

    #[cfg(unix)]
    #[test]
    fn discover_that_hangs_without_a_result_or_fails_is_an_error() {
        let d = TempDir::new().unwrap();
        let hung = script(d.path(), "hung", "echo 'InitializeSDK'; exec sleep 30");
        let e = discover_via(&hung, &|| scan_of(vec![]), Duration::from_millis(500)).unwrap_err();
        assert!(e.contains("did not finish"), "{e}");
        let failed = script(d.path(), "failed", "echo 'Error: camera busy' >&2; exit 1");
        assert_eq!(discover_via(&failed, &|| scan_of(vec![]), T).unwrap_err(), "Error: camera busy");
    }

    // ── snapshot (G25, G28, G42) ────────────────────────────────────────────

    #[test]
    fn a_lone_body_on_a_clean_bus_is_not_refused() {
        assert_eq!(snapshot_refusal(&row("Z 9", "AAA", 1), &scan_of(vec![usb("Z 9", "AAA")])), None);
        // Another model's duplicates do not matter.
        let bus = scan_of(vec![usb("Z 9", "AAA"), usb("Z6_3", "B"), usb("Z6_3", "C")]);
        assert_eq!(snapshot_refusal(&row("Z 9", "AAA", 1), &bus), None);
    }

    #[test]
    fn two_bodies_of_one_model_refuse_the_snapshot_by_the_shared_rule() {
        let bus = scan_of(vec![usb("Z 9", "AAA"), usb("Z 9", "BBB")]);
        let r = snapshot_refusal(&row("Z 9", "AAA", 1), &bus).unwrap();
        assert_eq!(r, SnapshotRefusal::AmbiguousPairing(AmbiguousPairing { model: "Z 9".into(), count: 2 }));
        assert!(r.to_string().contains("Attach one at a time") && r.to_string().contains("nothing was read"));
        // The discover-time count alone refuses too (the bus may have changed since).
        let r = snapshot_refusal(&row("Z 9", "AAA", 3), &scan_of(vec![usb("Z 9", "AAA")])).unwrap();
        assert_eq!(r, SnapshotRefusal::AmbiguousPairing(AmbiguousPairing { model: "Z 9".into(), count: 3 }));
    }

    #[test]
    fn a_body_dropped_as_unreadable_or_a_failed_enumeration_refuses_the_snapshot() {
        let dropped = describe_scan(&UsbScan { cameras: vec![usb("Z 9", "AAA")], unreadable: vec![unreadable_usb()], enumeration_error: None });
        let r = snapshot_refusal(&row("Z 9", "AAA", 1), &dropped).unwrap();
        assert_eq!(r, SnapshotRefusal::BodiesUnreadable { count: 1 });
        assert!(r.to_string().contains("could not be read"));
        let broken = describe_scan(&UsbScan { enumeration_error: Some("no usb".into()), ..UsbScan::default() });
        assert_eq!(snapshot_refusal(&row("Z 9", "AAA", 1), &broken), Some(SnapshotRefusal::UsbEnumerationFailed("no usb".into())));
    }

    #[test]
    fn the_snapshot_argv_passes_the_storable_serial_and_keeps_dashed_values_attached() {
        let argv = snapshot_argv(Path::new("/data/fleet"), "id-3", "").unwrap();
        assert_eq!(argv, ["--data-dir=/data/fleet", "snapshot", "--json", "--serial=id-3"]);
        let argv = snapshot_argv(Path::new("/d"), "-S1", "-guest").unwrap();
        assert_eq!(&argv[3..], ["--serial=-S1", "--label=-guest"]);
    }

    fn passed_stats() -> CaptureStats {
        CaptureStats { read_ok: 196, read_err: 11, skipped_no_get: 3, unknown_schema: 0, failed: Vec::new() }
    }

    fn outcome_json(model: &str, firmware: &str, gate: GateOutcome, stats: CaptureStats) -> String {
        let camera = Camera { model: model.into(), serial: "30012345".into(), firmware: firmware.into() };
        let props = stats.read_ok;
        serde_json::to_string_pretty(&CaptureOutcome::new("Z_9_30012345_x.json".into(), Some("x".into()), camera, props, stats, gate)).unwrap()
    }

    #[test]
    fn a_passed_capture_is_reported_with_the_gate_and_the_capture_statistics() {
        let json = outcome_json("Z 9", "5.31", GateOutcome::Passed, passed_stats());
        let done = describe_capture(&json, "warn line".into()).unwrap();
        assert_eq!(done.status.level, Level::Info);
        let t = &done.status.text;
        assert!(t.starts_with("Saved Z_9_30012345_x.json") && t.contains("completeness check passed"), "{t}");
        assert!(t.contains("196 read") && t.contains("11 failed") && t.contains("3 without a get bit"), "{t}");
        assert!(done.refresh_listing && done.cli_log == "warn line");
    }

    #[test]
    fn a_capture_that_was_not_judged_is_a_warning_with_its_reason_never_plain_saved() {
        // The Z 5 has no baseline.
        let unknown = describe_capture(&outcome_json("Z 5", "1.00", GateOutcome::NoBaseline(NoBaselineReason::UnknownModel), passed_stats()), String::new()).unwrap();
        assert_eq!(unknown.status.level, Level::Warn);
        assert!(unknown.status.text.contains("NOT checked for completeness") && unknown.status.text.contains("no baseline exists for Z 5"), "{}", unknown.status.text);
        let changed = describe_capture(
            &outcome_json(
                "Z 9",
                "6.00",
                GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "5.31".into(), capture: "6.00".into() }),
                passed_stats(),
            ),
            String::new(),
        )
        .unwrap();
        assert_eq!(changed.status.level, Level::Warn);
        assert!(changed.status.text.contains("firmware 6.00 differs from the baseline's 5.31"), "{}", changed.status.text);
    }

    #[test]
    fn an_incomplete_capture_reported_by_the_cli_is_a_warning_with_the_missing_count() {
        let missing = vec![nikon_fleet::ops::baseline::BaselineCapability { code: 1, name: "A".into() }];
        let done = describe_capture(&outcome_json("Z 9", "5.31", GateOutcome::Failed { missing }, passed_stats()), String::new()).unwrap();
        assert_eq!(done.status.level, Level::Warn);
        assert!(done.status.text.contains("INCOMPLETE") && done.status.text.contains("1 baseline"), "{}", done.status.text);
    }

    #[test]
    fn a_capture_that_read_nothing_is_a_failure_even_without_a_baseline() {
        // `read_ok == 0` at the boundary: one read is a capture, none is not.
        let none = CaptureStats { read_ok: 0, read_err: 9, ..CaptureStats::default() };
        let e = describe_capture(&outcome_json("Z 5", "1.00", GateOutcome::NoBaseline(NoBaselineReason::UnknownModel), none), String::new()).unwrap_err();
        assert!(e.contains("no capability at all") && e.contains("9 read error"), "{e}");
        let one = CaptureStats { read_ok: 1, ..CaptureStats::default() };
        assert!(describe_capture(&outcome_json("Z 5", "1.00", GateOutcome::NoBaseline(NoBaselineReason::UnknownModel), one), String::new()).is_ok());
    }

    #[test]
    fn unparseable_snapshot_output_says_the_snapshot_was_probably_saved() {
        let e = describe_capture("not json", String::new()).unwrap_err();
        assert!(e.contains("probably saved") && e.contains("check the list"), "{e}");
    }

    #[test]
    fn unknown_schema_entries_are_called_out() {
        let stats = CaptureStats { unknown_schema: 2, ..passed_stats() };
        let done = describe_capture(&outcome_json("Z 9", "5.31", GateOutcome::Passed, stats), String::new()).unwrap();
        assert!(done.notices.iter().any(|n| n.contains("2 capabilities were not found in the schema")), "{:?}", done.notices);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_via_runs_the_cli_with_the_serial_and_shows_its_verdict() {
        let d = TempDir::new().unwrap();
        let json = outcome_json("Z 9", "5.31", GateOutcome::Passed, passed_stats());
        let cli = recording_cli(d.path(), &format!("echo 'note: x' >&2\ncat <<'EOF'\n{json}\nEOF"));
        let data = d.path().join("data");
        let done = snapshot_via(&cli, &data, &row("Z 9", "30012345", 1), "wedding", &|| scan_of(vec![usb("Z 9", "30012345")]), T).unwrap();
        assert_eq!(
            argv_of(d.path()),
            [format!("--data-dir={}", data.display()), "snapshot".into(), "--json".into(), "--serial=30012345".into(), "--label=wedding".into()]
        );
        assert!(done.status.text.starts_with("Saved Z_9_30012345_x.json") && done.cli_log == "note: x");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_snapshot_never_starts_the_cli() {
        let d = TempDir::new().unwrap();
        let cli = recording_cli(d.path(), "echo '{}'");
        let two = scan_of(vec![usb("Z 9", "AAA"), usb("Z 9", "BBB")]);
        let e = snapshot_via(&cli, d.path(), &row("Z 9", "AAA", 2), "", &|| two.clone(), T).unwrap_err();
        assert!(e.contains("Attach one at a time"), "{e}");
        assert!(!d.path().join("argv.txt").exists(), "the CLI was not run");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_capture_shows_the_cli_message_and_refreshes_the_list() {
        let d = TempDir::new().unwrap();
        let cli = script(d.path(), "nikon-fleet", "echo 'capture is incomplete: 3 baseline capabilities. Nothing was written.' >&2; exit 1");
        let done = snapshot_via(&cli, d.path(), &row("Z 9", "A", 1), "", &|| scan_of(vec![usb("Z 9", "A")]), T).unwrap();
        assert_eq!(done.status.level, Level::Error);
        assert!(done.status.text.contains("Snapshot failed") && done.status.text.contains("Nothing was written"), "{}", done.status.text);
        assert!(!done.status.text.contains("may or may not"));
        assert!(done.refresh_listing);
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_capture_is_stopped_and_says_the_file_may_exist() {
        let d = TempDir::new().unwrap();
        let cli = script(d.path(), "nikon-fleet", "exec sleep 30");
        let done = snapshot_via(&cli, d.path(), &row("Z 9", "A", 1), "", &|| scan_of(vec![usb("Z 9", "A")]), Duration::from_millis(500)).unwrap();
        assert_eq!(done.status.level, Level::Error);
        assert!(done.status.text.contains("did not finish") && done.status.text.contains("may or may not"), "{}", done.status.text);
        assert!(done.refresh_listing);
    }

    #[test]
    fn data_commands_are_refused_when_the_data_directory_is_unknown() {
        let e = with_data_dir(&Err("settings.json is unusable: x".into()), |_| panic!("must not run"));
        assert!(matches!(e, Evt::Err(m) if m.contains("unusable")));
        assert!(matches!(with_data_dir(&Ok(PathBuf::from("/d")), |d| Evt::Err(d.display().to_string())), Evt::Err(m) if m == "/d"));
    }

    // ── vendor ops ──────────────────────────────────────────────────────────

    fn sample_profile() -> FtpProfile {
        FtpProfile {
            profile_name: "CLAUDE".to_string(),
            ssid_24ghz: "n24".to_string(),
            ssid_5ghz: "n5".to_string(),
            host: "ftp.example.com".to_string(),
            port: 2121,
            username: "user".to_string(),
            password: "s3cret pw".to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn vendor_write_sends_the_password_on_stdin_and_never_in_argv() {
        let dir = TempDir::new().unwrap();
        let cli = recording_cli(dir.path(), "echo 'Wrote FTP profile \"CLAUDE\" (user@ftp.example.com:2121)'");
        let p = sample_profile();
        let out = vendor_write_via(&cli, &p, Some("SER1"), T).unwrap();
        assert_eq!(out.text, "Wrote FTP profile \"CLAUDE\" (user@ftp.example.com:2121)");
        assert_eq!(fs::read_to_string(dir.path().join("stdin.txt")).unwrap(), "s3cret pw");
        let argv = argv_of(dir.path());
        assert!(argv.iter().all(|a| !a.contains("s3cret")), "password leaked into argv:\n{argv:?}");
        assert_eq!(argv, vendor_write_ftp_argv(&p, Some("SER1")));
    }

    #[cfg(unix)]
    #[test]
    fn vendor_stderr_from_a_successful_run_is_kept_and_shown_next_to_the_result() {
        // H27: the unconfirmed-model warning and a reply that did not match its command.
        let dir = TempDir::new().unwrap();
        let cli = recording_cli(dir.path(), "echo 'warning: model not confirmed' >&2\necho '0xd053: 2 byte(s): aa bb'");
        let out = vendor_read_via(&cli, 0xD053, Some("SER1"), T).unwrap();
        assert_eq!(out.text, "0xd053: 2 byte(s): aa bb");
        assert_eq!(out.stderr, "warning: model not confirmed");
        assert_eq!(argv_of(dir.path()), ["vendor-read", "0xd053", "--serial=SER1"]);
        let shown = with_stderr(&out.text, &out.stderr);
        assert!(shown.contains("0xd053: 2 byte(s)") && shown.contains("[stderr]") && shown.contains("model not confirmed"), "{shown}");
        assert_eq!(with_stderr("x", "  \n"), "x", "an empty stderr adds nothing");
    }

    #[cfg(unix)]
    #[test]
    fn vendor_write_failure_is_an_error_and_no_success_message() {
        let dir = TempDir::new().unwrap();
        let cli = script(dir.path(), "nikon-fleet", "cat >/dev/null\necho 'Error: could not claim the PTP USB interface' >&2\nexit 1");
        assert_eq!(vendor_write_via(&cli, &sample_profile(), None, T).unwrap_err(), "Error: could not claim the PTP USB interface");
    }

    #[cfg(unix)]
    #[test]
    fn vendor_read_failure_is_an_error() {
        let dir = TempDir::new().unwrap();
        let cli = script(dir.path(), "nikon-fleet", "echo 'Error: no Nikon USB camera found' >&2\nexit 1");
        assert_eq!(vendor_read_via(&cli, 0xD053, None, T).unwrap_err(), "Error: no Nikon USB camera found");
    }

    #[cfg(unix)]
    #[test]
    fn a_write_that_was_cut_off_or_took_a_truncated_password_says_the_camera_may_have_changed() {
        let dir = TempDir::new().unwrap();
        // Hung: killed at the limit, after the data may have gone out.
        let hung = script(dir.path(), "hung", "cat > /dev/null &\nexec sleep 30");
        let e = vendor_write_via(&hung, &sample_profile(), None, Duration::from_millis(500)).unwrap_err();
        assert!(e.contains("did not finish") && e.contains("MAY have taken the profile"), "{e}");
        // Exit 0 without reading the whole password.
        let early = script(dir.path(), "early", "exit 0");
        let mut p = sample_profile();
        p.password = "x".repeat(1_000_000);
        let e = vendor_write_via(&early, &p, None, T).unwrap_err();
        assert!(e.contains("could not be given its input") && e.contains("MAY have taken"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn exit_status_4_from_the_write_command_means_the_camera_may_have_changed() {
        let dir = TempDir::new().unwrap();
        // The message text is deliberately unhelpful: only the exit status may decide.
        let indeterminate = script(dir.path(), "ind", "cat > /dev/null\necho 'Error: write failed' >&2\nexit 4");
        let e = vendor_write_via(&indeterminate, &sample_profile(), None, T).unwrap_err();
        assert!(e.contains("camera may have changed") && e.contains("camera's menu") && e.contains("cannot read it back"), "{e}");
        assert!(e.contains("Error: write failed"), "the CLI's own text is kept: {e}");
        // The CLI's definitive refusals (1, 2) are plain failures.
        for code in [1, 2] {
            let plain = script(dir.path(), "plain", &format!("cat > /dev/null\necho 'Error: write failed' >&2\nexit {code}"));
            assert_eq!(vendor_write_via(&plain, &sample_profile(), None, T).unwrap_err(), "Error: write failed", "exit {code}");
        }
        // A read is not a write: 4 means nothing special there.
        let read = script(dir.path(), "read", "echo 'Error: x' >&2\nexit 4");
        assert_eq!(vendor_read_via(&read, 0xD053, None, T).unwrap_err(), "Error: x");
        // The code is carried typed on the error.
        let (p, a) = sh("exit 4");
        assert_eq!(run_cli(&p, &a, None, T).unwrap_err().code, Some(4));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_child_that_dies_or_panics_may_have_changed_the_camera() {
        let dir = TempDir::new().unwrap();
        let may = |e: &str| e.contains("MAY have taken the profile");
        // Killed by a signal after reading its input: no exit code at all.
        let killed = script(dir.path(), "killed", "cat > /dev/null\nkill -KILL $$");
        let e = vendor_write_via(&killed, &sample_profile(), None, T).unwrap_err();
        assert!(may(&e), "{e}");
        // A Rust panic exits 101; any status other than the CLI's own 1 and 2 is unknown territory.
        for code in [101, 3, 5, 127] {
            let c = script(dir.path(), "panic", &format!("cat > /dev/null\necho 'thread main panicked' >&2\nexit {code}"));
            let e = vendor_write_via(&c, &sample_profile(), None, T).unwrap_err();
            assert!(may(&e) && e.contains("panicked"), "exit {code}: {e}");
        }
        // Exits 1 and 2 stay definitive, and 4 keeps its own wording.
        for code in [1, 2] {
            let c = script(dir.path(), "refused", &format!("cat > /dev/null\necho 'refused' >&2\nexit {code}"));
            assert_eq!(vendor_write_via(&c, &sample_profile(), None, T).unwrap_err(), "refused", "exit {code}");
        }
        let four = script(dir.path(), "four", "cat > /dev/null\necho 'x' >&2\nexit 4");
        let e = vendor_write_via(&four, &sample_profile(), None, T).unwrap_err();
        assert!(e.contains("camera may have changed") && !may(&e), "{e}");
        // A read that dies is just a failed read.
        let read = script(dir.path(), "read", "kill -KILL $$");
        assert!(!may(&vendor_read_via(&read, 0xD053, None, T).unwrap_err()));
    }

    #[test]
    fn a_plain_exit_failure_of_a_write_is_not_dressed_up_as_indeterminate() {
        let e = CliError { kind: CliFailure::Exit, message: "refused".into(), stdout: String::new(), stderr: String::new(), code: Some(1) };
        assert_eq!(ftp_write_failure(&e), "refused");
        for kind in [CliFailure::TimedOut, CliFailure::Wait, CliFailure::StdinWrite] {
            let e = CliError { kind, ..e.clone() };
            assert!(ftp_write_failure(&e).contains("MAY have taken"), "{kind:?}");
        }
    }

    // ── the FTP form (G50, G40, T04) ────────────────────────────────────────

    fn filled_form() -> FtpForm {
        FtpForm {
            profile_name: "CLAUDE".into(),
            ssid_24ghz: "n24".into(),
            ssid_5ghz: "n5".into(),
            host: "ftp.example.com".into(),
            port: "2121".into(),
            username: "user".into(),
            password: "topsecret".into(),
        }
    }

    #[test]
    fn the_trim_rule_comes_from_the_field_table_for_every_field() {
        let mut form = filled_form();
        form.profile_name = "  p  ".into();
        form.ssid_24ghz = "  a  ".into();
        form.ssid_5ghz = "  b  ".into();
        form.host = "  h  ".into();
        form.username = "  u  ".into();
        form.password = "  pw  ".into();
        form.port = "  2121  ".into();
        let profile = form.build().unwrap();
        let sent = |f: FtpField| match f {
            FtpField::ProfileName => &profile.profile_name,
            FtpField::Ssid24Ghz => &profile.ssid_24ghz,
            FtpField::Ssid5Ghz => &profile.ssid_5ghz,
            FtpField::Host => &profile.host,
            FtpField::Username => &profile.username,
            FtpField::Password => &profile.password,
        };
        for f in FtpField::ALL {
            let expected = if f.spec().trimmed_for_required_check { form.raw(f).trim() } else { form.raw(f) };
            assert_eq!(sent(f), expected, "{}", f.spec().key);
        }
        // Pinned: the password is the one field that is not trimmed.
        assert_eq!(profile.password, "  pw  ");
        assert_eq!(profile.port, 2121);
    }

    #[test]
    fn required_fields_are_named_from_the_table_in_wire_order() {
        let form = FtpForm { port: "21".into(), ..FtpForm::default() };
        let e = form.build().unwrap_err();
        let names: Vec<&str> = nikon_fleet::ptp_usb::FTP_PROFILE_FIELDS.iter().map(|s| s.display_name).collect();
        assert_eq!(e, format!("Missing required field(s): {}", names.join(", ")));
        // Whitespace-only is missing for a trimmed field, present for the password.
        let mut f = filled_form();
        f.host = "   ".into();
        assert!(f.build().unwrap_err().contains("host"));
        let mut f = filled_form();
        f.password = "   ".into();
        assert_eq!(f.build().unwrap().password, "   ");
    }

    #[test]
    fn the_port_boundaries() {
        for (text, ok) in [("0", true), ("65535", true), ("65536", false), ("-1", false), ("", false), ("21a", false)] {
            let f = FtpForm { port: text.into(), ..filled_form() };
            assert_eq!(f.build().is_ok(), ok, "{text:?}");
        }
        assert!(FtpForm { port: "70000".into(), ..filled_form() }.build().unwrap_err().starts_with("Invalid port"));
    }

    #[test]
    fn the_dry_run_redacts_the_password_in_the_blob_and_shows_the_rest_of_the_encoding() {
        let form = filled_form();
        let (profile, preview) = prepare_ftp(&form).unwrap();
        let redacted_blob = ptp_usb::encode_ftp_profile(&profile.with_password_redacted()).unwrap();
        let plain_blob = ptp_usb::encode_ftp_profile(&profile).unwrap();
        let password_hex = ptp_usb::hex_bytes(b"topsecret");
        // The unredacted encoding DOES contain the password's bytes: the negative below can fail.
        assert!(ptp_usb::hex_bytes(&plain_blob).contains(&password_hex));
        assert_eq!(
            preview,
            format!(
                "Would write {} byte(s) (password redacted in this preview): {}",
                redacted_blob.len(),
                ptp_usb::hex_bytes(&redacted_blob)
            )
        );
        assert!(!preview.contains(&password_hex), "{preview}");
        // Positive: nine asterisks stand where the nine password characters were.
        assert!(preview.contains(&vec!["2a"; 9].join(" ")), "{preview}");
        // The other fields are in the preview as the real write would send them.
        assert!(preview.contains(&ptp_usb::hex_bytes(b"n24")) && preview.contains(&ptp_usb::hex_bytes(b"user")), "{preview}");
        assert_eq!(profile.password, "topsecret", "the profile to send keeps the real password");
        assert_eq!(redacted_blob.len(), plain_blob.len());
    }

    #[test]
    fn the_preview_rejects_what_the_encoder_rejects_before_anything_is_confirmed() {
        let mut f = filled_form();
        f.ssid_24ghz = "x".repeat(33); // 32 bytes is the limit
        assert!(prepare_ftp(&f).is_err());
        f.ssid_24ghz = "x".repeat(32);
        assert!(prepare_ftp(&f).is_ok());
        f.ssid_5ghz = "caf\u{e9}".into();
        assert!(prepare_ftp(&f).is_err(), "non-ASCII where the wire format is ASCII");
    }

    #[test]
    fn a_confirmation_names_the_exact_camera_and_not_the_password() {
        let form = filled_form();
        let (profile, _) = prepare_ftp(&form).unwrap();
        let cam = row("Z 9", "30012345", 1);
        let p = PendingWrite::new(profile.clone(), &form, Some(&cam));
        let text = p.confirm_text();
        assert!(text.contains("Z 9") && text.contains("30012345") && text.contains("CLAUDE") && text.contains("ftp.example.com:2121"), "{text}");
        assert!(!text.contains("topsecret"));
        let none = PendingWrite::new(profile, &form, None).confirm_text();
        assert!(none.contains("only Nikon camera"), "{none}");
    }

    #[test]
    fn a_confirmation_is_withdrawn_when_the_camera_or_any_form_field_changes() {
        let form = filled_form();
        let (profile, _) = prepare_ftp(&form).unwrap();
        let cam = row("Z 9", "AAA", 2);
        let p = PendingWrite::new(profile, &form, Some(&cam));
        assert!(!p.is_stale(&form, Some(&cam)), "unchanged: still good");
        // The other body of the same model (the case the finding describes).
        assert!(p.is_stale(&form, Some(&row("Z 9", "BBB", 2))));
        assert!(p.is_stale(&form, Some(&row("Z6_3", "AAA", 2))));
        assert!(p.is_stale(&form, None));
        let edits: [fn(&mut FtpForm); 7] = [
            |f| f.profile_name.push('x'),
            |f| f.ssid_24ghz.push('x'),
            |f| f.ssid_5ghz.push('x'),
            |f| f.host.push('x'),
            |f| f.port = "22".into(),
            |f| f.username.push('x'),
            |f| f.password.push('x'),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut changed = form.clone();
            edit(&mut changed);
            assert!(p.is_stale(&changed, Some(&cam)), "field edit #{i}");
        }
        // A re-discover that merely re-reports the same body is not a change.
        assert!(!p.is_stale(&form, Some(&CameraRow { firmware: "9.99".into(), ..cam.clone() })));
    }

    #[test]
    fn what_is_sent_is_what_was_confirmed_not_the_form_as_it_is_later() {
        let mut form = filled_form();
        let (profile, _) = prepare_ftp(&form).unwrap();
        let p = PendingWrite::new(profile, &form, Some(&row("Z 9", "AAA", 1)));
        form.host = "attacker.example".into();
        assert_eq!(p.profile.host, "ftp.example.com");
        assert_eq!(p.target.as_ref().map(|t| t.serial.as_str()), Some("AAA"));
    }

    // ── archive names (T01, T02) ────────────────────────────────────────────

    fn name_cases() -> Vec<(String, bool)> {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests-fixtures/zip_entry_names.json")).unwrap()).unwrap();
        v["cases"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap().to_string(), c["accept"].as_bool().unwrap())).collect()
    }

    #[test]
    fn every_name_in_the_shared_table_is_judged_as_the_table_says() {
        let cases = name_cases();
        assert!(cases.len() >= 40 && cases.iter().any(|c| c.1) && cases.iter().any(|c| !c.1));
        for (name, accept) in &cases {
            assert_eq!(classify_zip_entry(name).is_some(), *accept, "{name:?}");
        }
    }

    #[test]
    fn the_kind_of_each_accepted_name() {
        let k = |n: &str| classify_zip_entry(n);
        assert_eq!(k("snapshots/a.json"), Some(EntryKind::Snapshot));
        assert_eq!(k("references/a.json"), Some(EntryKind::Reference));
        assert_eq!(k("firmware/Z_9/5.31/firmware.bin"), Some(EntryKind::FirmwareBin));
        assert_eq!(k("firmware/Z_9/5.31/source-page.html"), Some(EntryKind::FirmwareSource));
        assert_eq!(k("firmware/Z_9/5.31/metadata.json"), Some(EntryKind::FirmwareMeta));
    }

    #[test]
    fn depth_alone_rejects_names_whose_file_name_part_is_valid() {
        // `sub.json` and `firmware.bin` pass the file-name checks, so only the number of
        // parts can reject these (a `len == 2` that became `>= 2` would accept them).
        for name in [
            "snapshots/sub.json/foo.json",
            "references/sub.json/foo.json",
            "firmware/Z_9/5.31/firmware.bin/firmware.bin",
            "firmware/Z_9/5.31/metadata.json/metadata.json",
            "firmware/Z_9/firmware.bin",
            "snapshots/foo.json/x",
        ] {
            assert_eq!(classify_zip_entry(name), None, "{name}");
        }
    }

    #[test]
    fn import_rejects_traversal_and_absolute_entries_and_still_imports_the_good_one() {
        let base = TempDir::new().unwrap();
        let data = base.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let good = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "good");
        let absolute = format!("{}/abs_evil.txt", base.path().display());
        let report = import(
            &data,
            &[
                ("snapshots/good.json", &good),
                // Valid snapshots, so only the name rule stands between them and the disk.
                ("../evil_sibling.txt", &good),
                (&absolute, &good),
                ("snapshots/../../evil_deep.json", &good),
                ("firmware/../5.31/firmware.bin", b"escaped"),
            ],
            &ImportLimits::default(),
        );
        assert!(!base.path().join("evil_sibling.txt").exists(), "escaped to the data dir's parent");
        assert!(!Path::new(&absolute).exists());
        assert!(!base.path().join("evil_deep.json").exists());
        assert_eq!(fs::read(data.join("snapshots/good.json")).unwrap(), good, "the good entry was imported");
        assert_eq!(report.snapshots, 1);
        assert!(!data.join("5.31").exists(), "`..` as a firmware slug wrote beside firmware/");
        assert_eq!(report.rejected.len(), 4);
        assert!(report.rejected.iter().all(|r| r.reason == RejectReason::BadName), "{:?}", report.rejected);
        assert_eq!(report.aborted, None);
    }

    // ── import ──────────────────────────────────────────────────────────────

    #[test]
    fn import_never_replaces_an_existing_file_and_counts_what_it_kept() {
        let d = TempDir::new().unwrap();
        let data = d.path();
        let original = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "original");
        let replacement = snapshot_bytes("Z 9", "S1", "2026-09-09T00:00:00Z", "replacement");
        fs::create_dir_all(data.join("snapshots")).unwrap();
        fs::create_dir_all(data.join("references")).unwrap();
        fs::write(data.join("snapshots/a.json"), &original).unwrap();
        fs::write(data.join("references/Z_9_S1.json"), &original).unwrap();
        firmware_unit(data, "Z 9", "Z_9", "5.31", b"OLD");
        let report = import(
            data,
            &[
                ("snapshots/a.json", &replacement),
                ("snapshots/new.json", &replacement),
                ("references/Z_9_S1.json", &replacement),
                ("firmware/Z_9/5.31/firmware.bin", b"NEW"),
                ("firmware/Z_9/5.31/metadata.json", &meta_bytes("Z 9", "5.31", b"NEW")),
            ],
            &ImportLimits::default(),
        );
        assert_eq!(fs::read(data.join("snapshots/a.json")).unwrap(), original, "a snapshot was overwritten");
        assert_eq!(fs::read(data.join("references/Z_9_S1.json")).unwrap(), original, "a reference was overwritten");
        assert_eq!(fs::read(data.join("firmware/Z_9/5.31/firmware.bin")).unwrap(), b"OLD", "firmware was overwritten");
        assert_eq!((report.snapshots, report.references, report.firmware), (1, 0, 0));
        // a.json, the reference, and bin + metadata of the existing unit (the zip writer itself
        // refuses two entries of one name, so that case cannot be built here).
        assert_eq!(report.skipped_existing, 4);
        assert!(report.rejected.is_empty() && report.aborted.is_none());
    }

    #[test]
    fn import_validates_each_snapshot_with_the_rule_listing_uses() {
        let fx = fixtures().join("snapshot_validity");
        let expected: HashMap<String, String> =
            serde_json::from_str(&fs::read_to_string(fx.join("expected.json")).unwrap()).unwrap();
        let files: Vec<(String, Vec<u8>)> =
            expected.keys().map(|f| (format!("snapshots/{f}"), fs::read(fx.join("snapshots").join(f)).unwrap())).collect();
        let entries: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        let d = TempDir::new().unwrap();
        let report = import(d.path(), &entries, &ImportLimits::default());
        let ok = expected.values().filter(|v| *v == "ok").count();
        assert_eq!(report.snapshots, ok);
        assert_eq!(report.rejected.len(), expected.len() - ok);
        assert!(report.rejected.iter().all(|r| r.reason == RejectReason::InvalidSnapshot), "{:?}", report.rejected);
        // What was imported lists exactly as ok: the two rules are one.
        let listed = list_all(d.path());
        assert_eq!(listed.rows.len(), ok);
        assert!(listed.unreadable.is_empty(), "an invalid file was imported: {:?}", listed.unreadable);
        // A reference is judged the same way.
        let d2 = TempDir::new().unwrap();
        let r = import(d2.path(), &[("references/Z_9_S1.json", b"{ nope")], &ImportLimits::default());
        assert_eq!((r.references, r.rejected.len()), (0, 1));
        assert!(!d2.path().join("references/Z_9_S1.json").exists());
    }

    fn padded_snapshot(len: usize) -> Vec<u8> {
        let mut b = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "pad");
        assert!(b.len() <= len);
        b.resize(len, b' '); // trailing whitespace keeps it valid JSON
        b
    }

    #[test]
    fn the_per_entry_json_limit_at_below_and_above() {
        let base = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "pad").len();
        let limit = (base + 50) as u64;
        let limits = ImportLimits { json_entry: limit, ..ImportLimits::default() };
        for (len, accepted) in [(limit as usize - 1, true), (limit as usize, true), (limit as usize + 1, false)] {
            let d = TempDir::new().unwrap();
            let bytes = padded_snapshot(len);
            let r = import(d.path(), &[("snapshots/s.json", &bytes)], &limits);
            assert_eq!(r.snapshots == 1, accepted, "len {len}");
            if !accepted {
                assert_eq!(r.rejected[0].reason, RejectReason::TooLarge);
                assert!(!d.path().join("snapshots/s.json").exists());
            }
        }
    }

    #[test]
    fn the_total_limit_at_below_and_above() {
        let n = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "pad").len() + 20;
        let a = padded_snapshot(n);
        let b = padded_snapshot(n);
        let two = 2 * n as u64;
        for (total, second_in) in [(two, true), (two - 1, false), (two + 1, true)] {
            let d = TempDir::new().unwrap();
            let r = import(d.path(), &[("snapshots/a.json", &a), ("snapshots/b.json", &b)], &ImportLimits { total, ..ImportLimits::default() });
            assert_eq!(r.snapshots, if second_in { 2 } else { 1 }, "total {total}");
            if !second_in {
                assert_eq!(r.rejected[0].reason, RejectReason::TotalLimit);
                assert_eq!(r.rejected[0].name, "snapshots/b.json");
            }
            assert_eq!(r.bytes, if second_in { two } else { n as u64 });
        }
    }

    #[test]
    fn the_firmware_limit_at_below_and_above_and_a_refused_binary_leaves_no_partial_entry() {
        let limits = ImportLimits { firmware_entry: 100, ..ImportLimits::default() };
        for (len, accepted) in [(99usize, true), (100, true), (101, false)] {
            let d = TempDir::new().unwrap();
            let bin = vec![7u8; len];
            let meta = meta_bytes("Z 9", "5.31", &bin);
            let r = import(
                d.path(),
                &[("firmware/Z_9/5.31/firmware.bin", &bin), ("firmware/Z_9/5.31/metadata.json", &meta)],
                &limits,
            );
            assert_eq!(r.firmware == 2, accepted, "len {len}: {:?}", r.rejected);
            if accepted {
                assert_eq!(fs::read(d.path().join("firmware/Z_9/5.31/firmware.bin")).unwrap(), bin);
            } else {
                assert_eq!(r.firmware, 0);
                assert!(r.rejected.iter().any(|x| x.reason == RejectReason::TooLarge));
                assert!(!d.path().join("firmware").exists(), "no half entry (metadata without its binary) was left");
            }
        }
    }

    #[test]
    fn a_firmware_entry_is_imported_only_whole() {
        let meta = meta_bytes("Z 9", "5.31", b"abc");
        // Whole: all three files, and the CLI's own scan lists it.
        let d = TempDir::new().unwrap();
        let r = import(
            d.path(),
            &[
                ("firmware/Z_9/5.31/metadata.json", &meta),
                ("firmware/Z_9/5.31/source-page.html", b"<html/>"),
                ("firmware/Z_9/5.31/firmware.bin", b"abc"),
            ],
            &ImportLimits::default(),
        );
        assert_eq!(r.firmware, 3);
        let scan = scan_archives(d.path(), None);
        assert_eq!(scan.entries.len(), 1);
        assert!(scan.entries[0].bin_present && scan.skipped.is_empty());
        // No metadata in the archive: nothing of the entry is written.
        let d = TempDir::new().unwrap();
        let r = import(d.path(), &[("firmware/Z_9/5.31/firmware.bin", b"abc")], &ImportLimits::default());
        assert_eq!((r.firmware, r.rejected[0].reason), (0, RejectReason::NoMetadata));
        assert!(!d.path().join("firmware").exists());
        // Invalid metadata (wrong format_version): the binary is not imported either.
        let d = TempDir::new().unwrap();
        let bad = String::from_utf8(meta.clone()).unwrap().replace("\"format_version\": 1", "\"format_version\": 99");
        let r = import(
            d.path(),
            &[("firmware/Z_9/5.31/firmware.bin", b"abc"), ("firmware/Z_9/5.31/metadata.json", bad.as_bytes())],
            &ImportLimits::default(),
        );
        assert_eq!(r.firmware, 0);
        assert!(r.rejected.iter().any(|x| x.reason == RejectReason::InvalidMetadata));
        assert!(!d.path().join("firmware").exists());
        // An empty binary is not an archive entry; the whole entry is refused.
        let d = TempDir::new().unwrap();
        let r = import(
            d.path(),
            &[("firmware/Z_9/5.31/firmware.bin", b""), ("firmware/Z_9/5.31/metadata.json", &meta_bytes("Z 9", "5.31", b""))],
            &ImportLimits::default(),
        );
        assert_eq!(r.firmware, 0);
        assert!(r.rejected.iter().any(|x| x.reason == RejectReason::EmptyFirmware));
        assert!(!d.path().join("firmware/Z_9/5.31/metadata.json").exists());
    }

    #[test]
    fn a_firmware_entry_whose_metadata_exists_is_whole_and_is_left_alone() {
        // The marker file decides: with metadata.json present nothing of the entry is imported
        // (its recorded checksum belongs to the binary already there, or to none).
        let d = TempDir::new().unwrap();
        let dir = d.path().join("firmware/Z_9/5.31");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("metadata.json"), meta_bytes("Z 9", "5.31", b"abc")).unwrap();
        let r = import(
            d.path(),
            &[
                ("firmware/Z_9/5.31/firmware.bin", b"abc"),
                ("firmware/Z_9/5.31/source-page.html", b"<html/>"),
                ("firmware/Z_9/5.31/metadata.json", &meta_bytes("Z 9", "5.31", b"abc")),
            ],
            &ImportLimits::default(),
        );
        assert_eq!((r.firmware, r.skipped_existing, r.rejected.len()), (0, 3, 0));
        assert!(!dir.join("firmware.bin").exists() && !dir.join("source-page.html").exists());
    }

    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("bad crc"))
        }
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn copy_capped_judges_the_bytes_that_arrive_not_the_size_the_header_claimed() {
        for (len, ok) in [(99usize, true), (100, true), (101, false)] {
            let mut out = Vec::new();
            let got = copy_capped(&mut Cursor::new(vec![1u8; len]), &mut out, 100);
            assert_eq!(matches!(got, Ok(n) if n == len as u64), ok, "len {len}");
            assert_eq!(matches!(got, Err(CopyFailure::TooBig)), !ok, "len {len}");
        }
        // A read failure and a write failure are told apart: only the second stops an import.
        assert!(matches!(copy_capped(&mut FailingReader, &mut Vec::new(), 100), Err(CopyFailure::Read(_))));
        assert!(matches!(copy_capped(&mut Cursor::new(vec![1u8; 10]), &mut FailingWriter, 100), Err(CopyFailure::Write(_))));
    }

    #[test]
    fn a_rejected_binary_takes_the_whole_firmware_entry_with_it_including_its_source_page() {
        let bin = vec![7u8; 101];
        let limits = ImportLimits { firmware_entry: 100, ..ImportLimits::default() };
        let d = TempDir::new().unwrap();
        let r = import(
            d.path(),
            &[
                ("firmware/Z_9/5.31/firmware.bin", &bin),
                ("firmware/Z_9/5.31/source-page.html", b"<html/>"),
                ("firmware/Z_9/5.31/metadata.json", &meta_bytes("Z 9", "5.31", &bin)),
            ],
            &limits,
        );
        assert!(!d.path().join("firmware").exists(), "a lone source-page.html (or anything) was left");
        assert_eq!((r.firmware, r.bytes), (0, 0), "nothing is counted as imported");
        let reasons: Vec<RejectReason> = r.rejected.iter().map(|x| x.reason).collect();
        assert_eq!(reasons, [RejectReason::TooLarge, RejectReason::EntryIncomplete, RejectReason::EntryIncomplete]);
        // The same for an empty binary.
        let d = TempDir::new().unwrap();
        let r = import(
            d.path(),
            &[("firmware/Z_9/5.31/firmware.bin", b""), ("firmware/Z_9/5.31/source-page.html", b"<html/>"), ("firmware/Z_9/5.31/metadata.json", &meta_bytes("Z 9", "5.31", b""))],
            &ImportLimits::default(),
        );
        assert!(!d.path().join("firmware").exists());
        assert_eq!(r.firmware, 0);
    }

    #[test]
    fn a_file_rejected_after_the_binary_was_written_takes_the_binary_back() {
        let bin = b"firmware bytes".to_vec();
        let d = TempDir::new().unwrap();
        // The limit lets the metadata through; the page is the one entry over it.
        let meta = meta_bytes("Z 9", "5.31", &bin);
        let limits = ImportLimits { json_entry: meta.len() as u64 + 1, ..ImportLimits::default() };
        let big_page = vec![b'x'; meta.len() + 2];
        let r = import(
            d.path(),
            &[
                ("firmware/Z_9/5.31/firmware.bin", &bin),
                ("firmware/Z_9/5.31/source-page.html", &big_page),
                ("firmware/Z_9/5.31/metadata.json", &meta),
            ],
            &limits,
        );
        assert!(!d.path().join("firmware").exists(), "the binary written first was left behind");
        assert_eq!((r.firmware, r.bytes), (0, 0));
        assert!(r.rejected.iter().any(|x| x.reason == RejectReason::TooLarge), "{:?}", r.rejected);
    }

    #[test]
    fn a_binary_that_is_not_the_one_its_metadata_describes_is_rejected_and_rolled_back() {
        let good = b"the real firmware".to_vec();
        let run = |bin: &[u8], meta: Vec<u8>| {
            let d = TempDir::new().unwrap();
            let r = import(
                d.path(),
                &[
                    ("firmware/Z_9/5.31/firmware.bin", bin),
                    ("firmware/Z_9/5.31/source-page.html", b"<html/>"),
                    ("firmware/Z_9/5.31/metadata.json", &meta),
                ],
                &ImportLimits::default(),
            );
            (d, r)
        };
        // Match: all three files land.
        let (d, r) = run(&good, meta_bytes("Z 9", "5.31", &good));
        assert_eq!((r.firmware, r.rejected.len()), (3, 0), "{:?}", r.rejected);
        assert_eq!(scan_archives(d.path(), None).entries.len(), 1);
        // Same size, different bytes: the SHA-256 differs.
        let mut other = good.clone();
        other[0] ^= 1;
        let (d, r) = run(&other, meta_bytes("Z 9", "5.31", &good));
        assert_eq!(r.rejected[0].reason, RejectReason::ChecksumMismatch);
        assert!(r.rejected[0].detail.contains("SHA-256"), "{}", r.rejected[0].detail);
        assert_eq!(r.firmware, 0);
        assert!(!d.path().join("firmware").exists(), "the entry was rolled back");
        // One byte longer: the size differs (checked before the hash).
        let mut longer = good.clone();
        longer.push(0);
        let (d, r) = run(&longer, meta_bytes("Z 9", "5.31", &good));
        assert_eq!(r.rejected[0].reason, RejectReason::ChecksumMismatch);
        assert!(r.rejected[0].detail.contains("bytes"), "{}", r.rejected[0].detail);
        assert!(!d.path().join("firmware").exists());
    }

    #[test]
    fn firmware_metadata_must_describe_the_directory_it_sits_in() {
        let bin = b"abc".to_vec();
        let run = |dir: &str, meta: Vec<u8>| {
            let d = TempDir::new().unwrap();
            let r = import(
                d.path(),
                &[(&format!("{dir}/firmware.bin"), &bin), (&format!("{dir}/metadata.json"), &meta)],
                &ImportLimits::default(),
            );
            (d, r)
        };
        // Z 9 / 5.31 in firmware/Z_9/5.31: imported.
        let (_, r) = run("firmware/Z_9/5.31", meta_bytes("Z 9", "5.31", &bin));
        assert_eq!(r.firmware, 2);
        // Wrong version, wrong model: the metadata is rejected and the binary with it.
        for (dir, model, version) in [("firmware/Z_9/5.30", "Z 9", "5.31"), ("firmware/Z_9/5.31", "Z6_3", "5.31"), ("firmware/Z6_3/5.31", "Z 9", "5.31")] {
            let (d, r) = run(dir, meta_bytes(model, version, &bin));
            assert_eq!(r.firmware, 0, "{dir} vs {model} {version}");
            assert!(r.rejected.iter().any(|x| x.reason == RejectReason::InvalidMetadata && x.detail.contains("sits in")), "{:?}", r.rejected);
            assert!(!d.path().join("firmware").exists());
        }
    }

    #[test]
    fn a_reference_must_carry_the_name_its_own_content_gives() {
        let content = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "ref");
        let d = TempDir::new().unwrap();
        let r = import(
            d.path(),
            &[("references/Z_9_S1.json", &content), ("references/wrong_name.json", &content), ("references/Z_9_S2.json", &content)],
            &ImportLimits::default(),
        );
        assert_eq!(r.references, 1);
        assert!(d.path().join("references/Z_9_S1.json").exists());
        assert!(!d.path().join("references/wrong_name.json").exists() && !d.path().join("references/Z_9_S2.json").exists());
        assert_eq!(r.rejected.len(), 2);
        assert!(r.rejected.iter().all(|x| x.reason == RejectReason::ReferenceNameMismatch), "{:?}", r.rejected);
        assert!(r.rejected[0].detail.contains("Z_9_S1.json"), "{}", r.rejected[0].detail);
        // A snapshot (not a reference) may carry any name.
        let r = import(d.path(), &[("snapshots/anything.json", &content)], &ImportLimits::default());
        assert_eq!(r.snapshots, 1);
        // A hostile serial has no valid reference name, so no name matches it.
        let hostile = snapshot_bytes("Z 9", "x/y", "2026-01-01T00:00:00Z", "h");
        let r = import(d.path(), &[("references/Z_9_x.json", &hostile)], &ImportLimits::default());
        assert_eq!((r.references, r.rejected[0].reason), (0, RejectReason::ReferenceNameMismatch));
    }

    #[test]
    fn an_import_that_fails_midway_reports_how_far_it_got() {
        let d = TempDir::new().unwrap();
        let data = d.path();
        fs::write(data.join("references"), "a file where the directory should be").unwrap();
        let s = snapshot_bytes("Z 9", "S1", "2026-01-01T00:00:00Z", "x");
        let s2 = snapshot_bytes("Z 9", "S2", "2026-01-01T00:00:00Z", "x");
        let report = import(
            data,
            &[("snapshots/a.json", &s), ("references/Z_9_S1.json", &s), ("references/Z_9_S2.json", &s2), ("snapshots/b.json", &s)],
            &ImportLimits::default(),
        );
        // Snapshots first (a, b), then the reference that cannot be created, then one never tried.
        assert_eq!(report.snapshots, 2);
        let why = report.aborted.clone().expect("a filesystem failure stops the import");
        assert!(why.contains("references"), "{why}");
        assert_eq!(report.not_attempted, 1);
        let done = describe_import(&report);
        assert_eq!(done.status.level, Level::Error);
        assert!(done.status.text.contains("STOPPED") && done.status.text.contains("2 snapshot(s)"), "{}", done.status.text);
        assert!(done.notices.iter().any(|n| n.contains("1 further entry")), "{:?}", done.notices);
    }

    #[test]
    fn a_failed_file_write_removes_the_partial_file_it_made() {
        let d = TempDir::new().unwrap();
        let path = d.path().join("x.json");
        let err = create_new_with(&path, |f| {
            f.write_all(b"half")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(matches!(err, Err(e) if e.contains("disk full")));
        assert!(!path.exists());
        // And create_new never replaces.
        fs::write(&path, "keep").unwrap();
        assert!(matches!(create_new_with(&path, |f| f.write_all(b"new")), Ok(CreateOutcome::Exists)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
    }

    #[test]
    fn import_reports_are_worded_by_severity_and_list_what_was_rejected() {
        let clean = ImportReport { snapshots: 2, references: 1, firmware: 3, bytes: 3 * 1024 * 1024, ..ImportReport::default() };
        let done = describe_import(&clean);
        assert_eq!(done.status.level, Level::Info);
        assert!(done.status.text.contains("2 snapshot(s), 1 reference(s), 3 firmware file(s) (3.0 MiB)") && done.status.text.contains("0 rejected"), "{}", done.status.text);
        assert!(done.refresh_listing);
        let with = ImportReport {
            skipped_existing: 4,
            rejected: vec![Rejected { name: "snapshots/x.json".into(), reason: RejectReason::InvalidSnapshot, detail: "EOF".into() }],
            ..clean
        };
        let done = describe_import(&with);
        assert_eq!(done.status.level, Level::Warn);
        assert!(done.status.text.contains("4 skipped because they already exist") && done.status.text.contains("1 rejected"), "{}", done.status.text);
        assert!(done.notices[0].contains("snapshots/x.json") && done.notices[0].contains("not a valid snapshot") && done.notices[0].contains("EOF"), "{:?}", done.notices);
    }

    #[test]
    fn long_lists_are_cut_for_display_with_an_exact_count() {
        let lines = |n: usize| (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>();
        assert_eq!(list_notices(lines(NOTICES_SHOWN).into_iter()).len(), NOTICES_SHOWN);
        let over = list_notices(lines(NOTICES_SHOWN + 1).into_iter());
        assert_eq!(over.len(), NOTICES_SHOWN + 1);
        assert_eq!(over.last().unwrap(), "… and 1 more");
        assert_eq!(list_notices(lines(NOTICES_SHOWN + 30).into_iter()).last().unwrap(), "… and 30 more");
    }

    // ── export ──────────────────────────────────────────────────────────────

    fn full_data_dir() -> TempDir {
        let d = TempDir::new().unwrap();
        let data = d.path();
        write_snapshot(&data.join("snapshots"), "a.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        write_snapshot(&data.join("snapshots"), "b.json", "Z 9", "ABC123", "2026-02-01T00:00:00Z");
        write_snapshot(&data.join("references"), "Z_9_ABC123.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        firmware_unit(data, "Z 9", "Z_9", "5.31", b"fake-firmware-bytes");
        d
    }

    #[test]
    fn export_then_import_round_trips_everything_the_cli_treats_as_archive_content() {
        let src = full_data_dir();
        let zip_dir = TempDir::new().unwrap();
        let zip = zip_dir.path().join("export.zip");
        let report = do_export(src.path(), &zip).unwrap();
        // 2 snapshots + 1 reference + firmware.bin + source-page.html + metadata.json
        assert_eq!((report.files, report.skipped.len()), (6, 0), "{:?}", report.skipped);
        assert_eq!(describe_export(&report).status.level, Level::Info);

        let dst = TempDir::new().unwrap();
        let imported = do_import(dst.path(), &zip).unwrap();
        assert_eq!((imported.snapshots, imported.references, imported.firmware), (2, 1, 3));
        assert!(imported.rejected.is_empty() && imported.skipped_existing == 0 && imported.aborted.is_none());
        for rel in [
            "snapshots/a.json",
            "snapshots/b.json",
            "references/Z_9_ABC123.json",
            "firmware/Z_9/5.31/firmware.bin",
            "firmware/Z_9/5.31/metadata.json",
            "firmware/Z_9/5.31/source-page.html",
        ] {
            assert_eq!(fs::read(dst.path().join(rel)).unwrap(), fs::read(src.path().join(rel)).unwrap(), "{rel}");
        }
        assert_eq!(list_all(dst.path()), list_all_with_dir(src.path(), dst.path()));
        // Importing the same archive again changes nothing and says so.
        let again = do_import(dst.path(), &zip).unwrap();
        assert_eq!((again.snapshots, again.references, again.firmware, again.skipped_existing), (0, 0, 0, 6));
    }

    /// `list_all(src)` with its directory name replaced by `dst`'s, for a comparison across two roots.
    fn list_all_with_dir(src: &Path, dst: &Path) -> SnapListing {
        SnapListing { dir: dst.join("snapshots").display().to_string(), ..list_all(src) }
    }

    #[cfg(unix)]
    #[test]
    fn export_counts_and_names_everything_it_leaves_out() {
        let d = full_data_dir();
        let data = d.path();
        fs::write(data.join("snapshots/notes.txt"), "x").unwrap();
        fs::create_dir_all(data.join("snapshots/sub")).unwrap();
        fs::write(data.join("firmware/Z_9/5.31/firmware.bin.part"), "x").unwrap();
        fs::write(data.join("firmware/Z_9_0531.bin"), "legacy").unwrap(); // the flat layout
        fs::create_dir_all(data.join("firmware/Z_9/9.99")).unwrap();
        fs::write(data.join("firmware/Z_9/9.99/firmware.bin"), "orphan").unwrap(); // a binary with no metadata
        write_snapshot(&data.join("snapshots"), "locked.json", "Z 9", "ABC123", "2026-03-01T00:00:00Z");
        fs::set_permissions(data.join("snapshots/locked.json"), fs::Permissions::from_mode(0o000)).unwrap();
        let zip = data.join("out.zip");
        let report = do_export(data, &zip).unwrap();
        fs::set_permissions(data.join("snapshots/locked.json"), fs::Permissions::from_mode(0o644)).unwrap();
        let skipped: Vec<(&str, &str)> = report.skipped.iter().map(|s| (s.what.as_str(), s.why.as_str())).collect();
        let has = |what: &str, why: &str| skipped.iter().any(|(w, y)| w.ends_with(what) && y.contains(why));
        assert!(has("snapshots/notes.txt", "not a .json"), "{skipped:?}");
        assert!(has("snapshots/sub", "not a regular file"), "{skipped:?}");
        assert!(has("5.31/firmware.bin.part", "not part of the archive layout"), "{skipped:?}");
        assert!(has("firmware/Z_9_0531.bin", "legacy layout"), "{skipped:?}");
        assert!(has("9.99", "not a valid archive entry"), "{skipped:?}");
        assert!(has("snapshots/locked.json", "could not be opened"), "{skipped:?}");
        assert_eq!(report.skipped.len(), 6, "{skipped:?}");
        assert_eq!(report.files, 6, "everything readable is in");
        let done = describe_export(&report);
        assert_eq!(done.status.level, Level::Warn);
        assert!(done.status.text.contains("6 item(s) are NOT in the archive"), "{}", done.status.text);
        assert_eq!(done.notices.len(), 6);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_counted_not_dropped() {
        let d = full_data_dir();
        let locked = d.path().join("references");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let zip = d.path().join("out.zip");
        let report = do_export(d.path(), &zip);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let report = report.unwrap();
        assert!(report.skipped.iter().any(|s| s.what == "references/" && s.why.contains("could not be read")), "{:?}", report.skipped);
        assert_eq!(describe_export(&report).status.level, Level::Warn);
    }

    #[test]
    fn exporting_nothing_is_not_reported_as_a_clean_success() {
        let d = TempDir::new().unwrap();
        let report = do_export(d.path(), &d.path().join("out.zip")).unwrap();
        assert_eq!((report.files, report.skipped.len()), (0, 0));
        let done = describe_export(&report);
        assert_eq!(done.status.level, Level::Warn);
        assert!(done.status.text.contains("nothing to export"), "{}", done.status.text);
    }

    #[test]
    fn a_failed_export_never_replaces_a_previous_backup_or_leaves_a_partial_file() {
        let d = TempDir::new().unwrap();
        let dest = d.path().join("backup.zip");
        fs::write(&dest, "PREVIOUS BACKUP").unwrap();
        let err = export_via_temp(&dest, |mut f| {
            f.write_all(b"half an archive").unwrap();
            Err::<fs::File, _>("disk full".to_string())
        });
        assert_eq!(err, Err("disk full".to_string()));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "PREVIOUS BACKUP");
        let leftovers: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, [OsString::from("backup.zip")], "no temporary file is left");
        // A whole archive does replace it.
        export_via_temp(&dest, |mut f| {
            f.write_all(b"NEW").unwrap();
            Ok(f)
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "NEW");
    }

    #[test]
    fn an_export_that_cannot_be_moved_into_place_keeps_what_was_there() {
        let d = TempDir::new().unwrap();
        let dest = d.path().join("backup.zip");
        fs::create_dir(&dest).unwrap(); // a directory cannot be replaced by a file
        let data = full_data_dir();
        assert!(do_export(data.path(), &dest).unwrap_err().contains("moving the archive"));
        assert!(dest.is_dir());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temporary file is left");
    }

    // ── snapshot viewer ────────────────────────────────────────────────────

    fn viewer_snapshot() -> Snapshot {
        let mut s = Snapshot::new(
            Camera { model: "Z 9".into(), serial: "30012345".into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-10-27T12:34:56Z".into(),
        );
        s.label = Some("base".into());
        s.insert("kNkMAIDCapability_Beta", 0xd053, serde_json::json!("Auto ISO"));
        s.insert("kNkMAIDCapability_Alpha", 12, serde_json::json!(true));
        s.insert("kNkMAIDCapability_Gamma", 4242, serde_json::json!({"lower": 0.0, "upper": 10.0, "steps": 11, "value": 0.0, "value_index": 99}));
        s
    }

    fn rendered() -> render::RenderedSnapshot {
        render::render_snapshot(&viewer_snapshot(), None)
    }

    fn loaded_viewer() -> SnapshotViewer {
        let mut v = SnapshotViewer::loading("a.json", false);
        v.finish(Ok(rendered()));
        v
    }

    fn visible(v: &SnapshotViewer) -> Vec<&str> {
        v.visible_rows().iter().map(|r| r.short_name()).collect()
    }

    #[test]
    fn opening_a_snapshot_starts_a_load_once_and_closing_removes_the_window() {
        let mut vs = SnapshotViewers::default();
        assert!(vs.open("a.json", false), "a new window needs its file read");
        assert!(vs.open("b.json", true));
        assert_eq!(vs.open.len(), 2);
        assert!(vs.open[0].loading && vs.open[0].rows.is_empty());
        assert!(vs.finish("a.json", Ok(rendered())));
        vs.open[0].filter = "beta".into();
        assert!(!vs.open("a.json", false), "already showing the file: no second window, no reload");
        assert_eq!(vs.open.len(), 2);
        assert_eq!(vs.open[0].filter, "beta", "the filter text survives a second open");
        assert!(vs.close("a.json"));
        assert_eq!(vs.open.iter().map(|v| v.file.as_str()).collect::<Vec<_>>(), ["b.json"]);
        assert!(!vs.close("a.json"), "closing a window that is not open changes nothing");
        assert!(vs.open("a.json", false), "after closing, opening reads the file again");
    }

    #[test]
    fn a_close_request_closes_only_the_window_that_asked() {
        let mut vs = SnapshotViewers::default();
        for f in ["a.json", "b.json", "c.json"] {
            vs.open(f, false);
        }
        let req = |a, b, c| vec![("a.json".to_string(), a), ("b.json".to_string(), b), ("c.json".to_string(), c)];
        assert!(vs.close_requested(req(false, false, false)).is_empty(), "no one asked: nothing closes");
        assert_eq!(vs.open.len(), 3);
        assert_eq!(vs.close_requested(req(false, true, false)), ["b.json"]);
        assert_eq!(vs.open.iter().map(|v| v.file.as_str()).collect::<Vec<_>>(), ["a.json", "c.json"]);
        // A stale request for a window that is already gone changes nothing and is not reported.
        assert!(vs.close_requested([("b.json".to_string(), true)]).is_empty());
        assert_eq!(vs.close_requested(req(true, false, true)).len(), 2, "several windows may close in one frame");
        assert!(vs.open.is_empty());
        assert!(vs.close_requested(Vec::new()).is_empty(), "no windows, no requests");
    }

    #[test]
    fn each_snapshot_has_its_own_stable_native_window_id() {
        assert_eq!(viewer_viewport_id("a.json"), viewer_viewport_id("a.json"), "the same file keeps its window");
        assert_ne!(viewer_viewport_id("a.json"), viewer_viewport_id("b.json"));
        assert_ne!(viewer_viewport_id("a.json"), egui::ViewportId::ROOT, "never the main window");
        assert_ne!(viewer_viewport_id(""), egui::ViewportId::ROOT);
    }

    #[test]
    fn a_window_that_failed_is_reloaded_when_opened_again() {
        let mut vs = SnapshotViewers::default();
        vs.open("a.json", false);
        vs.finish("a.json", Err("gone".into()));
        assert!(vs.open("a.json", false), "opening again is the retry");
        assert!(vs.open[0].loading && vs.open[0].error.is_none());
        assert_eq!(vs.open.len(), 1);
    }

    #[test]
    fn a_load_that_finishes_after_its_window_was_closed_is_dropped() {
        let mut vs = SnapshotViewers::default();
        vs.open("a.json", false);
        vs.close("a.json");
        assert!(!vs.finish("a.json", Ok(rendered())));
        assert!(vs.open.is_empty());
    }

    #[test]
    fn an_empty_filter_shows_every_row_in_name_order() {
        let v = loaded_viewer();
        assert_eq!(visible(&v), ["Alpha", "Beta", "Gamma"]);
        assert_eq!(v.count_line().as_deref(), Some("3 of 3 properties"));
    }

    #[test]
    fn the_filter_keeps_matching_rows_and_none_when_nothing_matches() {
        let mut v = loaded_viewer();
        v.filter = "beta".into();
        assert_eq!(visible(&v), ["Beta"]);
        assert_eq!(v.count_line().as_deref(), Some("1 of 3 properties"));
        v.filter = "no such property".into();
        assert!(v.visible_rows().is_empty());
        assert_eq!(v.count_line().as_deref(), Some("0 of 3 properties"), "a filter that matches nothing is not an empty file");
        v.filter = String::new();
        assert_eq!(visible(&v).len(), 3, "clearing the filter brings every row back");
    }

    #[test]
    fn the_filter_ignores_case_and_looks_at_values_and_codes_too() {
        let mut v = loaded_viewer();
        for (text, want) in [
            ("BETA", "Beta"),
            ("bEtA", "Beta"),
            ("auto iso", "Beta"),
            ("0XD053", "Beta"),
            ("4242", "Gamma"),
            ("malformed", "Gamma"),
            ("yes", "Alpha"),
        ] {
            v.filter = text.into();
            assert_eq!(visible(&v), [want], "{text}");
        }
    }

    #[test]
    fn a_snapshot_that_cannot_be_shown_shows_its_error_and_no_rows() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("snapshots")).unwrap();
        fs::write(dir.path().join("snapshots/broken.json"), "{ nope").unwrap();
        let mut v = SnapshotViewer::loading("broken.json", false);
        v.finish(load_for_view(dir.path(), dir.path(), "broken.json"));
        assert!(!v.loading);
        assert!(v.error.as_deref().is_some_and(|e| e.contains("broken.json") && e.contains("JSON")), "{:?}", v.error);
        assert!(v.rows.is_empty() && v.header.is_none());
        assert!(v.visible_rows().is_empty());
        assert_eq!(v.count_line(), None, "no count for a file that was not read");
    }

    #[test]
    fn a_failed_reload_clears_the_rows_of_the_earlier_success() {
        let mut v = loaded_viewer();
        assert_eq!(v.rows.len(), 3);
        v.finish(Err("the file changed".into()));
        assert!(v.rows.is_empty(), "stale rows must not stay under an error");
        assert!(v.header.is_none());
        assert_eq!(v.error.as_deref(), Some("the file changed"));
        v.finish(Ok(rendered()));
        assert!(v.error.is_none(), "a later success clears the error");
        assert_eq!(v.rows.len(), 3);
    }

    #[test]
    fn the_title_names_model_serial_firmware_label_and_capture_time_and_marks_the_reference() {
        let mut v = SnapshotViewer::loading("a.json", false);
        assert_eq!(v.title(), "a.json", "until the file is read, the file name");
        v.finish(Ok(rendered()));
        assert_eq!(v.title(), "Z 9 30012345  fw 5.31  base  2026-10-27T12:34:56");
        v.is_reference = true;
        assert!(v.title().ends_with("  ◄ ref"), "{}", v.title());
        let mut unlabelled = viewer_snapshot();
        unlabelled.label = None;
        v.finish(Ok(render::render_snapshot(&unlabelled, None)));
        assert!(v.title().contains("(no label)"), "{}", v.title());
        v.finish(Err("x".into()));
        assert_eq!(v.title(), "a.json  ◄ ref", "an unreadable file is still named, and still marked");
    }

    #[test]
    fn a_reference_set_while_a_window_is_open_marks_it() {
        let listing = SnapListing {
            rows: vec![
                SnapRow { file: "a.json".into(), model: "Z 9".into(), serial: "1".into(), label: None, captured_at: "t".into(), is_reference: true, name_lossy: false },
                SnapRow { file: "b.json".into(), model: "Z 9".into(), serial: "1".into(), label: None, captured_at: "t".into(), is_reference: false, name_lossy: false },
            ],
            ..SnapListing::default()
        };
        let mut vs = SnapshotViewers::default();
        vs.open("a.json", false);
        vs.open("b.json", true);
        vs.open("gone.json", true);
        vs.sync_reference(&listing);
        let marks: Vec<bool> = vs.open.iter().map(|v| v.is_reference).collect();
        assert_eq!(marks, [true, false, true], "a is now the reference, b no longer is, a file not in the listing keeps its mark");
    }

    /// A `sdk-runtime/` holding the synthetic schema and documentation of the shared render fixture:
    /// model "Test 1", firmware 2.00, `ModeA` (40001) named "Mode A" with the values Alpha, Beta, Unset, Seven.
    fn fixture_sdk_runtime() -> (TempDir, Snapshot) {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("../tests-fixtures/render_labels.json")).unwrap();
        let lines: Vec<&str> = fixture["schema_lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        let rt = TempDir::new().unwrap();
        fs::write(rt.path().join("MaidLayer.config"), lines.join("\n")).unwrap();
        fs::write(rt.path().join("maid-labels.json"), fixture["doc"].to_string()).unwrap();
        let mut s = Snapshot::new(
            Camera { model: "Test 1".into(), serial: "1".into(), firmware: "2.00".into() },
            Transport::Usb,
            "2026-10-27T12:34:56Z".into(),
        );
        s.insert(
            "kNkMAIDCapability_ModeA",
            40001,
            serde_json::json!({"elem_type": 2, "value_index": 1, "elem_count": 3, "elem_bytes": 4, "default_index": 0, "values": [0, 1, 7]}),
        );
        s.insert("kNkMAIDCapability_Range", 40009, serde_json::json!({"lower": 0.0, "upper": 10.0, "steps": 11, "value": 4.0, "value_index": 4}));
        (rt, s)
    }

    #[test]
    fn the_viewer_shows_the_sdks_names_labels_and_range_scales_when_sdk_runtime_has_them() {
        let (rt, snap) = fixture_sdk_runtime();
        let data = TempDir::new().unwrap();
        fs::create_dir_all(data.path().join("snapshots")).unwrap();
        snap.save_to_file(data.path().join("snapshots/t.json")).unwrap();
        let mut v = SnapshotViewer::loading("t.json", false);
        v.finish(load_for_view(data.path(), rt.path(), "t.json"));
        let mode = v.rows.iter().find(|r| r.short_name() == "ModeA").unwrap();
        assert_eq!(mode.extra_display_name(), Some("Mode A"));
        assert_eq!(mode.value.display, "Beta (1)");
        let range = v.rows.iter().find(|r| r.short_name() == "Range").unwrap();
        assert!(range.value.display.ends_with("(0..10, 11 steps)"), "{}", range.value.display);
        assert_eq!(v.labels_note, None);
        assert!(!v.show_codes, "the code column starts hidden");
    }

    #[test]
    fn the_viewer_without_sdk_runtime_shows_the_numbers_and_says_why_the_names_are_missing() {
        let (_rt, snap) = fixture_sdk_runtime();
        let empty = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        fs::create_dir_all(data.path().join("snapshots")).unwrap();
        snap.save_to_file(data.path().join("snapshots/t.json")).unwrap();
        let mut v = SnapshotViewer::loading("t.json", false);
        v.finish(load_for_view(data.path(), empty.path(), "t.json"));
        let mode = v.rows.iter().find(|r| r.short_name() == "ModeA").unwrap();
        assert_eq!((mode.extra_display_name(), mode.value.display.as_str()), (None, "1"));
        assert!(v.labels_note.as_deref().is_some_and(|n| n.contains("MaidLayer.config")), "{:?}", v.labels_note);
        v.finish(Err("gone".into()));
        assert_eq!(v.labels_note, None, "a note about an earlier load does not stay under an error");
    }

    #[test]
    fn the_filter_still_finds_a_row_by_its_display_name_and_by_its_code_while_codes_are_hidden() {
        let (rt, snap) = fixture_sdk_runtime();
        let data = TempDir::new().unwrap();
        fs::create_dir_all(data.path().join("snapshots")).unwrap();
        snap.save_to_file(data.path().join("snapshots/t.json")).unwrap();
        let mut v = SnapshotViewer::loading("t.json", false);
        v.finish(load_for_view(data.path(), rt.path(), "t.json"));
        for text in ["mode a", "9c41", "beta"] {
            v.filter = text.into();
            assert_eq!(visible(&v), ["ModeA"], "{text}");
        }
    }

    #[test]
    fn the_sdk_runtime_directory_is_the_override_or_the_projects() {
        assert_eq!(resolve_sdk_runtime(Some(OsString::from("/x/rt"))), PathBuf::from("/x/rt"));
        for none in [None, Some(OsString::new())] {
            let p = resolve_sdk_runtime(none);
            assert!(p.ends_with("sdk-runtime"), "{p:?}");
            assert!(p.starts_with(env!("CARGO_MANIFEST_DIR")), "{p:?}");
        }
    }

    #[test]
    fn load_for_view_reads_a_snapshot_from_snapshots_and_nowhere_else() {
        let dir = TempDir::new().unwrap();
        let no_names = TempDir::new().unwrap();
        let snapshots = dir.path().join("snapshots");
        fs::create_dir_all(&snapshots).unwrap();
        fs::create_dir_all(dir.path().join("references")).unwrap();
        viewer_snapshot().save_to_file(snapshots.join("ok.json")).unwrap();
        viewer_snapshot().save_to_file(dir.path().join("references/ref.json")).unwrap();
        let r = load_for_view(dir.path(), no_names.path(), "ok.json").unwrap();
        assert_eq!((r.header.property_count, r.rows.len(), r.filtered_out), (3, 3, 0));
        assert!(load_for_view(dir.path(), no_names.path(), "ref.json").unwrap_err().contains("ref.json"), "a listing row is a snapshots/ file");
        assert!(load_for_view(dir.path(), no_names.path(), "missing.json").unwrap_err().contains("missing.json"));
        for hostile in ["../references/ref.json", "..", "", "a/b.json"] {
            assert!(load_for_view(dir.path(), no_names.path(), hostile).is_err(), "{hostile:?}");
        }
        let old = viewer_snapshot().to_pretty_json().unwrap().replace("\"format_version\": 1", "\"format_version\": 7");
        fs::write(snapshots.join("old.json"), old).unwrap();
        assert!(load_for_view(dir.path(), no_names.path(), "old.json").unwrap_err().contains("unsupported format_version 7"));
    }

    #[test]
    fn the_loader_thread_answers_on_the_channel_with_the_file_it_read() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("snapshots")).unwrap();
        viewer_snapshot().save_to_file(dir.path().join("snapshots/ok.json")).unwrap();
        fs::write(dir.path().join("snapshots/bad.json"), "[]").unwrap();
        let (tx, rx) = mpsc::channel();
        for file in ["ok.json", "bad.json"] {
            spawn_view_load(dir.path().to_path_buf(), file.into(), tx.clone(), egui::Context::default()).unwrap();
        }
        let mut got: Vec<(String, bool)> = (0..2)
            .map(|_| rx.recv_timeout(T).expect("a load answers"))
            .map(|l| (l.file, l.result.is_ok()))
            .collect();
        got.sort();
        assert_eq!(got, [("bad.json".to_string(), false), ("ok.json".to_string(), true)]);
    }

    // ── transplant: plan, confirm, apply ──────────────────────────────────────
    //
    // The JSON the GUI reads is produced here by the library itself (`report_from`,
    // `apply_transplant` on its fake port), so a field the library renames or drops fails these
    // tests; the CLI is a script that prints it, and no camera is ever touched.

    use nikon_fleet::maid_layer::MaidLayerConfig;
    use nikon_fleet::ops::apply::{ApplyRequest, apply_transplant};
    use nikon_fleet::ops::baseline::{GatePolicy, baseline_for};
    use nikon_fleet::ops::plan::{PlanInput, PlanReport, build_plan, report_from};
    use nikon_fleet::ops::port::{FakeCamera, Failure};
    use nikon_fleet::sdk::{OP_GET, OP_SET};
    use nikon_fleet::transplant::{
        CodeNameGuard, PlannedWrite, ResolvedChoice, SkipReason, Strategy, TransplantOptions, TransplantOutcome,
    };

    const TP_FILE: &str = "2026-10-27T100000Z_Z6_3_SRC.json";
    const WB_AUTO: &str = "kNkMAIDCapability_WBAutoType";
    const METERING: &str = "kNkMAIDCapability_MeteringMode";

    fn tp_target() -> CameraRow {
        row("Z 9", "T1", 1)
    }

    fn tp_request(optins: &[&str]) -> TransplantRequest {
        TransplantRequest {
            source_body: ("Z6_3".into(), "SRC".into()),
            source_file: TP_FILE.into(),
            target: tp_target(),
            allow_raw_code: optins.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn copied(name: &str, code: u32, strategy: Strategy, choice: Option<ResolvedChoice>, changes: Option<bool>) -> TransplantOutcome {
        TransplantOutcome {
            name: format!("kNkMAIDCapability_{name}"),
            code,
            result: Ok(strategy),
            choice,
            planned: Some(PlannedWrite { value: serde_json::json!(0), changes }),
        }
    }

    fn skipped(name: &str, code: u32, reason: SkipReason) -> TransplantOutcome {
        TransplantOutcome { name: format!("kNkMAIDCapability_{name}"), code, result: Err(reason), choice: None, planned: None }
    }

    fn choice(target: u64, current: u64) -> Option<ResolvedChoice> {
        Some(ResolvedChoice {
            target_index: target,
            choice: format!("Option{target}"),
            current_index: Some(current),
            current: Some(format!("Option{current}")),
        })
    }

    /// One outcome of every kind a plan shows. With `WBAutoType` opted in it is copied, as the
    /// library would plan it.
    fn library_outcomes(optins: &[&str]) -> Vec<TransplantOutcome> {
        let mut v = vec![
            copied("ExposureMode", 1, Strategy::EnumByLabel, choice(2, 1), Some(true)),
            copied("WhiteBalance", 2, Strategy::EnumByLabel, choice(3, 3), Some(false)),
            copied("Banner", 3, Strategy::Literal, None, None),
            skipped("Odd", 6, SkipReason::NoMatchingOption),
            skipped("Absent", 7, SkipReason::AbsentOnTarget),
            skipped("Scale", 8, SkipReason::RangeUnverified { source_model: "Z6_3".into(), target_model: "Z 9".into() }),
        ];
        if optins.contains(&WB_AUTO) {
            v.push(copied("WBAutoType", 4, Strategy::Literal, None, Some(true)));
        } else {
            v.push(skipped("WBAutoType", 4, SkipReason::LiteralUnconfirmed { proposed: "5, was 3".into() }));
        }
        if optins.contains(&METERING) {
            v.push(copied("MeteringMode", 5, Strategy::EnumByRawValue, choice(2, 1), Some(true)));
        } else {
            v.push(skipped(
                "MeteringMode",
                5,
                SkipReason::RawCodeUnconfirmed { proposed: "code 5 (#2), was code 3 (#1)".into() },
            ));
        }
        v
    }

    fn library_plan_report(optins: &[&str]) -> PlanReport {
        let mut src = Snapshot::new(
            Camera { model: "Z6_3".into(), serial: "SRC".into(), firmware: "2.00".into() },
            Transport::Usb,
            "2026-10-27T10:00:00Z".into(),
        );
        src.label = Some("studio".into());
        let target = Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() };
        let options = TransplantOptions { allow_raw_code: optins.iter().map(|s| s.to_string()).collect() };
        report_from(library_outcomes(optins), &PlanInput { source: &src, source_bytes: b"{}", target: &target, options: &options })
    }

    fn plan_json_text(optins: &[&str]) -> String {
        serde_json::to_string_pretty(&library_plan_report(optins)).unwrap()
    }

    fn plan_json(optins: &[&str]) -> PlanJson {
        serde_json::from_str(&plan_json_text(optins)).expect("the library's plan parses")
    }

    fn planned(optins: &[&str]) -> PlannedTransplant {
        // What the library's fixture proposed for each opt-in, as the OptIn stage would have recorded it.
        let proposals = [(WB_AUTO, "5, was 3"), (METERING, "code 5 (#2), was code 3 (#1)")];
        let optin_values = proposals.iter().filter(|(n, _)| optins.contains(n)).map(|(n, v)| (n.to_string(), v.to_string())).collect();
        PlannedTransplant { request: tp_request(optins), plan: plan_json(optins), optin_values, cli_log: String::new() }
    }

    fn names<'a>(rows: &[&'a OutcomeJson]) -> Vec<&'a str> {
        rows.iter().map(|o| short_name(&o.name)).collect()
    }

    // ── the library's plan, read by the GUI ───────────────────────────────

    #[test]
    fn the_librarys_plan_is_read_and_sorted_into_what_the_person_must_look_at() {
        let p = plan_json(&[]);
        assert_eq!((p.target.model.as_str(), p.target.serial.as_str(), p.source_label.as_deref()), ("Z 9", "T1", Some("studio")));
        let s = plan_sections(&p.outcomes);
        let mut changing = names(&s.will_change);
        changing.sort();
        assert_eq!(changing, ["Banner", "ExposureMode"], "a copy whose effect is unknown is a possible change");
        assert_eq!(names(&s.unchanged), ["WhiteBalance"]);
        let mut deciding = names(&s.needs_decision);
        deciding.sort();
        assert_eq!(deciding, ["MeteringMode", "WBAutoType"]);
        assert_eq!(names(&s.attention), ["Odd"], "a warn-severity skip wants attention");
        assert_eq!(s.routine.keys().copied().collect::<Vec<_>>(), ["absent_on_target", "range_unverified"]);
        // Every outcome is in exactly one place.
        let placed = s.will_change.len() + s.unchanged.len() + s.needs_decision.len() + s.attention.len()
            + s.routine.values().map(Vec::len).sum::<usize>();
        assert_eq!(placed, p.outcomes.len());
    }

    #[test]
    fn the_sections_count_what_the_library_counts_as_a_possible_change() {
        for optins in [&[][..], &[WB_AUTO][..], &[WB_AUTO, METERING][..]] {
            let p = plan_json(optins);
            assert_eq!(plan_sections(&p.outcomes).will_change.len(), p.counts.would_change, "{optins:?}");
        }
        assert_eq!(plan_json(&[WB_AUTO]).counts.would_change, 3);
    }

    #[test]
    fn an_opted_in_property_is_copied_and_no_longer_waits_for_a_decision() {
        let p = plan_json(&[WB_AUTO]);
        let s = plan_sections(&p.outcomes);
        assert_eq!(names(&s.needs_decision), ["MeteringMode"]);
        assert!(names(&s.will_change).contains(&"WBAutoType"));
        assert_eq!(p.allow_raw_code, [WB_AUTO]);
    }

    #[test]
    fn only_the_librarys_two_opt_in_skips_ask_for_a_decision() {
        let reasons = vec![
            SkipReason::AbsentOnTarget,
            SkipReason::ReadOnly,
            SkipReason::UnsupportedWriteType,
            SkipReason::NoMatchingOption,
            SkipReason::UnsupportedValueShape,
            SkipReason::TargetReadFailed("x".into()),
            SkipReason::MalformedSource,
            SkipReason::WriteFailed("x".into()),
            SkipReason::EnumUndecodedBySdk { side: "source", reason: "x".into() },
            SkipReason::EnumTypeMismatch { source_elem_type: 7, target_elem_type: 5 },
            SkipReason::CapabilityNameMismatch { source_names: "A".into(), target_names: "B".into() },
            SkipReason::AmbiguousLabel { label: "125".into(), source_count: 2, target_count: 1 },
            SkipReason::NeverCopied { reason: "state" },
            SkipReason::RawCodeUnconfirmed { proposed: "p".into() },
            SkipReason::LiteralUnconfirmed { proposed: "p".into() },
            SkipReason::RangeUnverified { source_model: "A".into(), target_model: "B".into() },
            SkipReason::RangeScaleDiffers { source: "0..1".into(), target: "0..2".into() },
        ];
        let tags: Vec<&str> = reasons.iter().map(SkipReason::tag).collect();
        let report = report_from(
            reasons.into_iter().enumerate().map(|(i, r)| skipped(&format!("P{i}"), i as u32, r)).collect(),
            &PlanInput {
                source: &Snapshot::new(Camera { model: "A".into(), serial: "s".into(), firmware: "1".into() }, Transport::Usb, "t".into()),
                source_bytes: b"{}",
                target: &Camera { model: "B".into(), serial: "t".into(), firmware: "1".into() },
                options: &TransplantOptions::default(),
            },
        );
        let parsed: PlanJson = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        let asking: Vec<&str> = parsed.outcomes.iter().filter(|o| o.needs_opt_in()).filter_map(|o| o.tag()).collect();
        let mut asking_sorted = asking.clone();
        asking_sorted.sort();
        assert_eq!(asking_sorted, ["literal_unconfirmed", "raw_code_unconfirmed"], "of {tags:?}");
        assert!(OPT_IN_TAGS.iter().all(|t| tags.contains(t)), "the GUI's tags are the library's");
        for o in parsed.outcomes.iter().filter(|o| o.needs_opt_in()) {
            assert_eq!(o.reason.as_ref().unwrap().severity, Severity::Policy);
        }
    }

    #[test]
    fn an_outcome_reads_as_a_line_with_its_choice_or_its_reason() {
        let p = plan_json(&[]);
        let line = |n: &str| p.outcomes.iter().find(|o| short_name(&o.name) == n).unwrap().line();
        assert_eq!(line("ExposureMode"), "ExposureMode  [enum by label]: \"Option2\" (#2), was \"Option1\" (#1)");
        assert_eq!(line("WhiteBalance"), "WhiteBalance  [enum by label]: \"Option3\" (#3), unchanged");
        assert_eq!(line("Banner"), "Banner  [literal]: current value unknown, so it may change");
        assert_eq!(line("WBAutoType"), "WBAutoType: literal unconfirmed (proposed: 5, was 3)");
        assert_eq!(line("Absent"), "Absent: absent on target");
    }

    // ── the argument lists ────────────────────────────────────────────────

    /// The snapshot as the CLI is given it: absolute, under the data directory's `snapshots/`.
    fn source_arg(data_dir: &str, file: &str) -> String {
        std::path::absolute(Path::new(data_dir).join("snapshots").join(file)).unwrap().to_str().unwrap().to_string()
    }

    fn pos(argv: &[String], what: &str) -> usize {
        argv.iter().position(|a| a == what).unwrap_or_else(|| panic!("{what} not in {argv:?}"))
    }

    #[test]
    fn a_plan_asks_for_a_dry_run_and_cannot_carry_a_confirmation() {
        let argv = transplant_argv(Path::new("/data"), &tp_request(&[]), None).unwrap();
        assert_eq!(argv[..4], ["--data-dir=/data", "transplant", "--json", "--serial=T1"]);
        assert!(argv.contains(&"--dry-run".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("--expect-")), "{argv:?}");
        assert_eq!(argv[argv.len() - 2..], ["--".to_string(), source_arg("/data", TP_FILE)]);
    }

    #[test]
    fn an_apply_presents_the_confirmed_digest_and_summary_and_is_never_a_dry_run() {
        let argv = transplant_argv(Path::new("/data"), &tp_request(&[]), Some(("abc123", "Z 9 T1: 2 properties would change; opted in: none"))).unwrap();
        assert!(argv.contains(&"--expect-digest=abc123".to_string()), "{argv:?}");
        assert!(argv.contains(&"--expect-summary=Z 9 T1: 2 properties would change; opted in: none".to_string()), "{argv:?}");
        assert!(!argv.contains(&"--dry-run".to_string()), "{argv:?}");
    }

    #[test]
    fn only_the_opt_ins_in_the_request_reach_the_cli_and_none_by_default() {
        let none = transplant_argv(Path::new("/data"), &tp_request(&[]), None).unwrap();
        assert!(!none.iter().any(|a| a.contains("allow-raw-code")), "{none:?}");
        let one = transplant_argv(Path::new("/data"), &tp_request(&[WB_AUTO]), None).unwrap();
        let allowed: Vec<&String> = one.iter().filter(|a| a.starts_with("--allow-raw-code")).collect();
        assert_eq!(allowed, [&format!("--allow-raw-code={WB_AUTO}")]);
        let two = transplant_argv(Path::new("/data"), &tp_request(&[METERING, WB_AUTO]), None).unwrap();
        assert_eq!(two.iter().filter(|a| a.starts_with("--allow-raw-code")).count(), 2);
    }

    #[test]
    fn a_hostile_source_name_is_refused_or_kept_from_being_a_flag() {
        for bad in ["", ".", "..", "../x.json", "a/b.json", "a\\b.json"] {
            let mut r = tp_request(&[]);
            r.source_file = bad.into();
            assert!(transplant_argv(Path::new("/d"), &r, None).is_err(), "{bad:?}");
        }
        let mut r = tp_request(&[]);
        r.source_file = "--allow-partial".into();
        let argv = transplant_argv(Path::new("/d"), &r, None).unwrap();
        assert_eq!(argv[argv.len() - 2..], ["--".to_string(), source_arg("/d", "--allow-partial")]);
        assert!(!argv.contains(&"--allow-partial".to_string()), "never a flag of its own: {argv:?}");
        let mut r = tp_request(&[]);
        r.target.serial = "-x".into();
        assert!(transplant_argv(Path::new("/d"), &r, None).unwrap().contains(&"--serial=-x".to_string()));
    }

    // ── a plan is for the body and the opt-ins that were asked for ────────────

    #[test]
    fn a_plan_for_another_body_or_other_opt_ins_is_not_shown() {
        let plan = plan_json(&[WB_AUTO]);
        assert!(check_plan_is_for(&plan, &tp_request(&[WB_AUTO])).is_ok());
        let mut other_serial = tp_request(&[WB_AUTO]);
        other_serial.target.serial = "T2".into();
        assert!(check_plan_is_for(&plan, &other_serial).unwrap_err().contains("T2"));
        let mut other_model = tp_request(&[WB_AUTO]);
        other_model.target.model = "Z 8".into();
        assert!(check_plan_is_for(&plan, &other_model).is_err());
        assert!(check_plan_is_for(&plan, &tp_request(&[])).unwrap_err().contains("opt-ins"), "an opt-in nobody asked for");
        assert!(check_plan_is_for(&plan_json(&[]), &tp_request(&[WB_AUTO])).unwrap_err().contains("opt-ins"), "an opt-in that was lost");
    }

    // ── the target ───────────────────────────────────────────────────────

    #[test]
    fn a_target_without_a_usb_serial_or_sharing_its_model_is_refused() {
        assert!(transplant_target_refusal(&row("Z 9", "T1", 1)).is_none());
        let id = transplant_target_refusal(&row("Z 9", "id-1", 1)).unwrap();
        assert!(id.contains("no USB serial") && id.contains("id-1"), "{id}");
        assert!(transplant_target_refusal(&row("Z 9", "id-12", 1)).is_some());
        assert!(transplant_target_refusal(&row("Z 9", "id-", 1)).is_none(), "only id-<digits> is the fallback");
        assert!(transplant_target_refusal(&row("Z 9", "T1", 2)).unwrap().contains("wrong physical body"), "two of one model cannot be told apart");
    }

    #[test]
    fn the_pre_flight_also_applies_the_snapshot_rule_for_the_bus() {
        let clean = scan_of(vec![usb("Z 9", "T1")]);
        assert!(transplant_refusal(&tp_target(), &clean).is_none());
        let two = scan_of(vec![usb("Z 9", "T1"), usb("Z 9", "T2")]);
        assert!(transplant_refusal(&tp_target(), &two).unwrap().starts_with("Transplant refused, nothing was read or written:"));
        let mut unreadable = scan_of(vec![usb("Z 9", "T1")]);
        unreadable.unreadable.push(unreadable_usb());
        assert!(transplant_refusal(&tp_target(), &unreadable).unwrap().contains("could not be read"));
        let mut failed = scan_of(vec![]);
        failed.enumeration_error = Some("usb gone".into());
        assert!(transplant_refusal(&tp_target(), &failed).unwrap().contains("usb gone"));
        // The snapshot's own wording is unchanged by sharing its reason.
        assert!(SnapshotRefusal::BodiesUnreadable { count: 1 }.to_string().starts_with("Snapshot refused, nothing was read: 1 Nikon USB"));
    }

    // ── the stages ───────────────────────────────────────────────────────

    fn ui_ready() -> (TransplantUi, Vec<CameraRow>) {
        let cameras = vec![tp_target()];
        let ui = TransplantUi {
            open: true,
            source_body: Some(("Z6_3".into(), "SRC".into())),
            source_file: Some(TP_FILE.into()),
            target_key: Some(("Z 9".into(), "T1".into())),
            ..TransplantUi::default()
        };
        (ui, cameras)
    }

    /// A form that has been planned and has the library's plan on screen.
    fn ui_with_plan(optins: &[&str]) -> (TransplantUi, Vec<CameraRow>) {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        ui.optins = optins.iter().map(|s| s.to_string()).collect();
        let cmd = ui.begin_plan(&cameras).unwrap();
        let Cmd::TransplantPlan { request } = cmd else { panic!("a plan command") };
        ui.plan_arrived(request, Ok((plan_json(optins), String::new())));
        assert!(matches!(ui.stage, TransplantStage::Planned(_)));
        (ui, cameras)
    }

    #[test]
    fn nothing_is_asked_of_the_camera_until_a_source_and_a_target_are_chosen() {
        let mut ui = TransplantUi::default();
        assert!(ui.begin_plan(&[tp_target()]).is_err());
        ui.source_file = Some(TP_FILE.into());
        assert!(ui.begin_plan(&[tp_target()]).is_err(), "no target yet, and no body for the snapshot");
        ui.source_body = Some(("Z6_3".into(), "SRC".into()));
        assert!(ui.begin_plan(&[tp_target()]).is_err(), "no target yet");
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        assert!(ui.begin_plan(&[]).is_err(), "the chosen target is not connected");
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert!(matches!(ui.begin_plan(&[tp_target()]), Ok(Cmd::TransplantPlan { .. })));
    }

    #[test]
    fn a_target_that_cannot_be_named_for_a_write_is_not_planned() {
        let (mut ui, _) = ui_ready();
        let cameras = vec![row("Z 9", "T1", 2)];
        assert!(ui.begin_plan(&cameras).unwrap_err().contains("wrong physical body"));
        assert_eq!(ui.stage, TransplantStage::Idle);
    }

    #[test]
    fn a_plan_that_comes_back_for_another_request_is_dropped() {
        let (mut ui, cameras) = ui_ready();
        ui.begin_plan(&cameras).unwrap();
        ui.plan_arrived(tp_request(&[WB_AUTO]), Ok((plan_json(&[WB_AUTO]), String::new())));
        assert!(matches!(ui.stage, TransplantStage::Planning(_)), "still waiting for its own plan");
        ui.stage = TransplantStage::Idle;
        ui.plan_arrived(tp_request(&[]), Ok((plan_json(&[]), String::new())));
        assert_eq!(ui.stage, TransplantStage::Idle, "a plan nobody is waiting for is not shown");
    }

    #[test]
    fn a_failed_plan_is_a_notice_and_leaves_nothing_to_confirm() {
        let (mut ui, cameras) = ui_ready();
        let Cmd::TransplantPlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.plan_arrived(request, Err("Planning failed: camera busy".into()));
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert_eq!(ui.notice, "Planning failed: camera busy");
        ui.ask_apply();
        assert_eq!(ui.stage, TransplantStage::Idle);
    }

    #[test]
    fn apply_is_confirmed_in_two_steps_and_sends_the_plan_that_was_shown() {
        let (mut ui, cameras) = ui_with_plan(&[]);
        let shown = plan_json(&[]);
        assert!(ui.confirm_apply().is_none(), "a click on Confirm without the question having been asked sends nothing");
        ui.ask_apply();
        assert!(matches!(ui.stage, TransplantStage::ConfirmApply(_)));
        // The form changes under the confirmation without the frame sync having run yet: what is
        // sent is still what was confirmed.
        ui.source_file = Some("other.json".into());
        let Some(Cmd::TransplantApply { request, digest, summary }) = ui.confirm_apply() else { panic!("an apply") };
        assert_eq!(request, tp_request(&[]));
        assert_eq!((digest, summary), (shown.digest, shown.summary));
        assert_eq!(ui.stage, TransplantStage::Applying(tp_request(&[])));
        assert!(ui.confirm_apply().is_none(), "one confirmation sends one apply");
        let _ = cameras;
    }

    #[test]
    fn cancelling_a_confirmation_returns_to_the_plan_and_sends_nothing() {
        let (mut ui, _) = ui_with_plan(&[]);
        ui.ask_apply();
        ui.cancel_confirmation();
        assert!(matches!(ui.stage, TransplantStage::Planned(_)));
        assert!(ui.confirm_apply().is_none());
        ui.ask_opt_in(WB_AUTO);
        ui.cancel_confirmation();
        assert!(matches!(ui.stage, TransplantStage::Planned(_)));
        assert!(ui.optins.is_empty(), "a cancelled opt-in is not taken");
    }

    #[test]
    fn a_plan_that_would_change_nothing_cannot_be_applied() {
        let (mut ui, _) = ui_with_plan(&[]);
        let TransplantStage::Planned(p) = &mut ui.stage else { panic!() };
        p.plan.counts.would_change = 0;
        ui.ask_apply();
        assert!(matches!(ui.stage, TransplantStage::Planned(_)));
        let TransplantStage::Planned(p) = &mut ui.stage else { panic!() };
        p.plan.counts.would_change = 1;
        ui.ask_apply();
        assert!(matches!(ui.stage, TransplantStage::ConfirmApply(_)), "one possible change is enough");
    }

    #[test]
    fn a_property_is_opted_in_only_after_its_own_confirmation_and_then_the_plan_is_made_again() {
        let (mut ui, cameras) = ui_with_plan(&[]);
        // Asking about a property the plan did not skip for want of an opt-in does nothing.
        for not_asking in ["kNkMAIDCapability_ExposureMode", "kNkMAIDCapability_Odd", "kNkMAIDCapability_Nope"] {
            ui.ask_opt_in(not_asking);
            assert!(matches!(ui.stage, TransplantStage::Planned(_)), "{not_asking}");
        }
        assert!(ui.confirm_opt_in(&cameras).is_none(), "confirming a question nobody asked opts nothing in");
        assert!(ui.optins.is_empty());
        ui.ask_opt_in(WB_AUTO);
        assert!(ui.optins.is_empty(), "asking is not opting in");
        let Some(Cmd::TransplantPlan { request }) = ui.confirm_opt_in(&cameras) else { panic!("a new plan") };
        assert_eq!(request.allow_raw_code, BTreeSet::from([WB_AUTO.to_string()]), "exactly the property that was confirmed");
        assert!(matches!(&ui.stage, TransplantStage::Planning(r) if *r == request));
    }

    #[test]
    fn an_opt_in_confirmation_names_the_property_both_bodies_and_the_value() {
        let p = planned(&[]);
        let text = opt_in_text(&p, WB_AUTO);
        for needle in ["WBAutoType", "Z6_3", "SRC", "Z 9", "T1", "5, was 3", "bare number"] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
        let raw = opt_in_text(&p, METERING);
        assert!(raw.contains("MeteringMode") && raw.contains("numeric code") && raw.contains("code 5 (#2), was code 3 (#1)"), "{raw}");
    }

    #[test]
    fn the_apply_confirmation_names_the_body_the_summary_and_every_opt_in() {
        let text = apply_confirm_text(&planned(&[WB_AUTO]));
        for needle in ["Z 9", "serial T1", "firmware 5.31", TP_FILE, "Z6_3", "SRC", "studio", "WBAutoType", "restore point", "read back"] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
        assert!(text.contains(&plan_json(&[WB_AUTO]).summary), "the line that is approved is the plan's own");
        assert!(
            text.contains("Opted in by raw code or bare number") && text.contains("WBAutoType (sets 5, was 3)."),
            "the opt-in is stated apart from the summary, with the value it sets: {text}"
        );
        let both = apply_confirm_text(&planned(&[WB_AUTO, METERING]));
        assert!(both.contains("WBAutoType (sets 5, was 3)") && both.contains("MeteringMode (sets code 5 (#2), was code 3 (#1))"), "{both}");
        let mut unstated = planned(&[WB_AUTO]);
        unstated.optin_values.clear();
        assert!(apply_confirm_text(&unstated).contains("WBAutoType (sets a value that was not stated)"), "never a silent blank");
        assert!(!apply_confirm_text(&planned(&[])).contains("Opted in"), "no opt-in, none claimed");
    }

    // ── a change withdraws the plan and any confirmation ──────────────────────

    #[test]
    fn a_changed_form_or_camera_withdraws_the_plan_and_each_confirmation() {
        type Change = fn(&mut TransplantUi, &mut Vec<CameraRow>);
        let changes: [(&str, Change); 6] = [
            ("another snapshot", |ui, _| ui.source_file = Some("other.json".into())),
            ("no snapshot", |ui, _| ui.source_file = None),
            ("another target", |ui, c| {
                c.push(row("Z 9", "T9", 1));
                ui.target_key = Some(("Z 9".into(), "T9".into()));
            }),
            ("the target's firmware", |_, c| c[0].firmware = "5.40".into()),
            ("a second body of its model", |_, c| c[0].model_count = 2),
            ("the target unplugged", |_, c| c.clear()),
        ];
        for (what, change) in changes {
            for stage in ["planned", "opt-in", "confirm"] {
                let (mut ui, mut cameras) = ui_with_plan(&[]);
                match stage {
                    "opt-in" => ui.ask_opt_in(WB_AUTO),
                    "confirm" => ui.ask_apply(),
                    _ => {}
                }
                ui.sync(&cameras);
                assert!(!matches!(ui.stage, TransplantStage::Idle), "no change yet: {stage}");
                change(&mut ui, &mut cameras);
                ui.sync(&cameras);
                assert_eq!(ui.stage, TransplantStage::Idle, "{what} while {stage}");
                assert!(ui.notice.contains("withdrawn") && ui.notice.contains("Nothing was written"), "{}", ui.notice);
                assert!(ui.confirm_apply().is_none(), "{what} while {stage}: nothing left to confirm");
            }
        }
    }

    #[test]
    fn a_plan_still_being_made_is_withdrawn_by_a_change_and_its_answer_is_then_ignored() {
        let (mut ui, mut cameras) = ui_ready();
        let Cmd::TransplantPlan { request } = ui.begin_plan(&cameras).unwrap() else { panic!() };
        ui.sync(&cameras);
        assert!(matches!(ui.stage, TransplantStage::Planning(_)));
        cameras[0].firmware = "9.99".into();
        ui.sync(&cameras);
        assert_eq!(ui.stage, TransplantStage::Idle);
        ui.plan_arrived(request, Ok((plan_json(&[]), String::new())));
        assert_eq!(ui.stage, TransplantStage::Idle, "the late plan is for a request nobody holds any more");
    }

    #[test]
    fn an_apply_in_flight_and_its_result_are_not_withdrawn_by_a_change() {
        let (mut ui, mut cameras) = ui_with_plan(&[]);
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        cameras.clear();
        ui.sync(&cameras);
        assert!(matches!(ui.stage, TransplantStage::Applying(_)), "it is on the wire; the answer must find it");
        let outcome = ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message: "busy".into(), cli_log: String::new() };
        ui.applied(tp_request(&[]), outcome.clone());
        ui.sync(&cameras);
        assert!(matches!(&ui.stage, TransplantStage::Done(a) if a.outcome == outcome));
        let mut stray = ui_ready().0;
        stray.applied(tp_request(&[]), outcome);
        assert_eq!(stray.stage, TransplantStage::Idle, "an answer nobody is waiting for is not recorded");
    }

    #[test]
    fn opt_ins_belong_to_the_source_and_target_they_were_given_for_and_do_not_come_back() {
        let (mut ui, mut cameras) = ui_with_plan(&[WB_AUTO]);
        ui.sync(&cameras);
        assert_eq!(ui.optins.len(), 1);
        cameras.push(row("Z 9", "T9", 1));
        ui.target_key = Some(("Z 9".into(), "T9".into()));
        ui.sync(&cameras);
        assert!(ui.optins.is_empty(), "another target starts with none");
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        ui.sync(&cameras);
        assert!(ui.optins.is_empty(), "and going back does not resurrect them");
        assert!(ui.request(&cameras).unwrap().allow_raw_code.is_empty());
    }

    #[test]
    fn taking_an_opt_in_back_makes_the_plan_again_without_it_and_with_the_others() {
        let (mut ui, cameras) = ui_with_plan(&[WB_AUTO, METERING]);
        ui.sync(&cameras);
        assert!(ui.remove_opt_in("kNkMAIDCapability_Nope", &cameras).is_none(), "not an opt-in: nothing happens");
        assert!(matches!(ui.stage, TransplantStage::Planned(_)));
        let Some(Cmd::TransplantPlan { request }) = ui.remove_opt_in(WB_AUTO, &cameras) else { panic!("a new plan") };
        assert_eq!(request.allow_raw_code, BTreeSet::from([METERING.to_string()]));
        assert!(matches!(&ui.stage, TransplantStage::Planning(r) if *r == request));
        ui.sync(&cameras);
        assert!(matches!(ui.stage, TransplantStage::Planning(_)), "the new plan is not withdrawn by the change it is for");
        assert!(ui.remove_opt_in(METERING, &cameras).is_none(), "only a plan on screen can have an opt-in taken back");
    }

    #[test]
    fn the_opt_ins_do_not_carry_into_the_next_plan_after_an_apply() {
        let (mut ui, _) = ui_with_plan(&[WB_AUTO]);
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        ui.applied(tp_request(&[WB_AUTO]), ApplyOutcome::Indeterminate { message: "x".into(), cli_log: String::new() });
        assert!(ui.optins.is_empty());
    }

    // ── how an apply ended ───────────────────────────────────────────────

    pub(crate) fn cli_error(kind: CliFailure, code: Option<i32>, stdout: &str) -> CliError {
        CliError { kind, message: "the message".into(), stdout: stdout.into(), stderr: "log".into(), code }
    }

    /// The library's own apply reports, in the three ways an apply that wrote can end.
    pub(crate) fn library_apply_json(scenario: &str) -> (String, ApplyStatus) {
        let b = baseline_for("Z 9").unwrap();
        let codes: Vec<(String, u32)> = b.readable.iter().skip(6).take(4).map(|c| (c.name.clone(), c.code)).collect();
        let mut cam = FakeCamera::new();
        let mut names = HashMap::new();
        for c in &b.readable {
            let held = codes.iter().position(|(_, code)| *code == c.code).map_or(serde_json::json!(1), |i| {
                if i < 3 { serde_json::json!("old") } else { serde_json::json!(1) }
            });
            cam = cam.with(c.code, OP_GET | OP_SET, held);
            names.insert(c.code, c.name.clone());
        }
        match scenario {
            "failed" => cam = cam.failing_write(codes[1].1, Failure::Call(-125)),
            "unverified" => cam = cam.failing_write(codes[0].1, Failure::UnsupportedWrite),
            _ => {}
        }
        let mut source = Snapshot::new(
            Camera { model: "Z6_3".into(), serial: "SRC".into(), firmware: "2.00".into() },
            Transport::Usb,
            "2026-10-27T10:00:00Z".into(),
        );
        for (i, (name, code)) in codes.iter().enumerate() {
            source.insert(name.clone(), *code, if i == 3 { serde_json::json!(55) } else { serde_json::json!(format!("text {i}")) });
        }
        let bytes = source.to_pretty_json().unwrap().into_bytes();
        let mut cfg = String::from("<ProfileVersion>9.0.3000</ProfileVersion>\n");
        for model in ["Z6_3", "Z 9"] {
            cfg.push_str(&format!("<model:{model}>\n<version>common</version>\n<caplist>\n"));
            for (name, code) in &codes {
                let n = short_name(name);
                cfg.push_str(&format!(
                    "<capability:kNkMAIDCapability_{n}-{code}>\n<description>0,0,\"{n}\"</description>\n\
                     <allowedoperation:14></allowedoperation>\n<DeviceCommand>1,0,0,0,0</DeviceCommand>\n</capability>\n"
                ));
            }
            cfg.push_str("</caplist>\n</model>\n");
        }
        let guard = CodeNameGuard::new(&MaidLayerConfig::parse(&cfg).unwrap(), "Z6_3", "Z 9").unwrap();
        let target = Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() };
        let options = TransplantOptions { allow_raw_code: BTreeSet::from([codes[3].0.clone()]) };
        let input = || PlanInput { source: &source, source_bytes: &bytes, target: &target, options: &options };
        let plan = build_plan(&cam, &guard, &input());
        let dir = TempDir::new().unwrap();
        let report = apply_transplant(
            &cam,
            &ApplyRequest {
                plan: input(),
                guard: &guard,
                snapshots_dir: dir.path(),
                name_lookup: &names,
                expected_digest: &plan.digest,
                expected_summary: &plan.summary,
                captured_at: "2026-10-27T17:26:00.688812Z".into(),
                gate_policy: GatePolicy::Strict,
            },
            &mut |_| {},
        )
        .expect("the apply ran");
        (serde_json::to_string_pretty(&report).unwrap(), report.status)
    }

    /// `json`, an apply report, as the CLI would print it when held to `digest`: the same report
    /// with the plan it was held to named in it.
    fn with_plan_digest(json: &str, digest: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["plan_digest"] = serde_json::json!(digest);
        serde_json::to_string_pretty(&v).unwrap()
    }

    #[test]
    fn the_librarys_apply_reports_are_read_with_their_status_for_every_way_a_write_ends() {
        let mut seen = BTreeSet::new();
        for scenario in ["verified", "unverified", "failed"] {
            let (json, status) = library_apply_json(scenario);
            let r: ApplyJson = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{scenario}: {e}"));
            assert_eq!(r.status, status, "{scenario}");
            assert!(r.restore_point.is_some(), "{scenario}: the restore point is offered");
            seen.insert(status.tag());
        }
        assert_eq!(seen, BTreeSet::from(["applied_verified", "applied_unverified", "failed"]), "three different endings");
    }

    #[test]
    fn a_report_is_the_answer_whatever_the_exit_status_was() {
        let (json, status) = library_apply_json("failed");
        assert_eq!(status, ApplyStatus::Failed);
        let digest = serde_json::from_str::<ApplyJson>(&json).unwrap().plan_digest;
        // A failed apply exits non-zero after printing its report.
        let ApplyOutcome::Report { report, note, .. } = classify_apply(Err(cli_error(CliFailure::Exit, Some(1), &json)), &digest) else {
            panic!("a report")
        };
        assert_eq!((report.status, note), (ApplyStatus::Failed, None));
        // A clean exit is the same report.
        let ok = Ok(CliOutput { stdout: json.clone(), stderr: "log".into() });
        assert!(matches!(classify_apply(ok, &digest), ApplyOutcome::Report { note: None, .. }));
        // A run stopped after it printed keeps the report, with a note.
        let ApplyOutcome::Report { note, .. } = classify_apply(Err(cli_error(CliFailure::TimedOut, None, &json)), &digest) else {
            panic!("a report")
        };
        assert!(note.unwrap().contains("stopped after it reported"));
    }

    #[test]
    fn a_report_for_a_different_plan_than_the_one_confirmed_is_not_taken_as_the_answer() {
        let (json, _) = library_apply_json("verified");
        let out = classify_apply(Ok(CliOutput { stdout: json, stderr: String::new() }), "not-the-confirmed-digest");
        let ApplyOutcome::Indeterminate { message, .. } = out else { panic!("{out:?}") };
        assert!(message.contains("different plan"), "{message}");
    }

    #[test]
    fn without_a_report_only_the_clis_own_refusals_say_nothing_was_written() {
        let d = "digest";
        let stale = classify_apply(Err(cli_error(CliFailure::Exit, Some(EXIT_STALE_PLAN), "")), d);
        assert!(matches!(stale, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, .. }), "{stale:?}");
        let refused = classify_apply(Err(cli_error(CliFailure::Exit, Some(EXIT_REFUSED_BEFORE_WRITE), "")), d);
        assert!(matches!(refused, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{refused:?}");
        let never_ran = classify_apply(Err(cli_error(CliFailure::Spawn, None, "")), d);
        assert!(matches!(never_ran, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }));
        // Anything else may have written.
        for e in [
            cli_error(CliFailure::TimedOut, None, ""),
            cli_error(CliFailure::Wait, None, ""),
            cli_error(CliFailure::StdinWrite, None, ""),
            cli_error(CliFailure::Exit, None, ""),
            cli_error(CliFailure::Exit, Some(101), ""),
            // 1 is also "wrote, but did not fully take"; 2 is the parser's, which no CLI of this build exits with mid-run
            cli_error(CliFailure::Exit, Some(1), ""),
            cli_error(CliFailure::Exit, Some(2), ""),
            cli_error(CliFailure::Exit, Some(3), ""),
            cli_error(CliFailure::Exit, Some(4), ""),
            cli_error(CliFailure::Exit, Some(0), ""),
        ] {
            let out = classify_apply(Err(e.clone()), d);
            assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "{e:?} -> {out:?}");
        }
        let unreadable = classify_apply(Ok(CliOutput { stdout: "not json".into(), stderr: String::new() }), d);
        assert!(matches!(unreadable, ApplyOutcome::Indeterminate { .. }), "exit 0 with no report is not a success");
        assert_eq!((EXIT_STALE_PLAN, EXIT_REFUSED_BEFORE_WRITE), (5, 6), "documented in the CLI's --help");
    }

    #[test]
    fn a_report_this_program_cannot_read_is_not_taken_for_a_refusal_even_after_exit_one() {
        // The CLI exits 1 for an apply that wrote but did not fully take, and prints its report first.
        // A report that does not parse here (a newer CLI's new status, one cut off, none) must not be
        // read as "the camera was not changed".
        let (json, _) = library_apply_json("failed");
        let digest = serde_json::from_str::<ApplyJson>(&json).unwrap().plan_digest;
        let new_status = json.replacen("\"failed\"", "\"applied_somehow\"", 1);
        assert_ne!(new_status, json, "the fixture holds the status it is replacing");
        let cut_off = json[..json.len() / 2].to_string();
        for (what, stdout) in [("a status this program does not know", new_status), ("a report cut off", cut_off), ("garbage", "{{".into()), ("nothing", String::new())] {
            let out = classify_apply(Err(cli_error(CliFailure::Exit, Some(1), &stdout)), &digest);
            assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "{what}: {out:?}");
        }
    }

    #[test]
    fn only_applied_verified_is_worded_as_having_taken_and_the_rest_say_otherwise() {
        for s in ApplyStatus::ALL {
            let (level, text) = status_headline(s);
            if s == ApplyStatus::AppliedVerified {
                assert_eq!(level, Level::Info);
                assert!(text.starts_with("Applied and verified"));
            } else {
                assert_ne!(level, Level::Info, "{s:?}");
                assert!(!text.to_lowercase().contains("verified:") || text.contains("NOT"), "{s:?}: {text}");
            }
        }
        assert_eq!(status_headline(ApplyStatus::Failed).0, Level::Error);
        assert_eq!(status_headline(ApplyStatus::Indeterminate).0, Level::Error);
        assert!(status_headline(ApplyStatus::AppliedUnverified).1.contains("NOT fully verified"));
        // The status bar leads with the status word.
        let (json, _) = library_apply_json("unverified");
        let report = serde_json::from_str::<ApplyJson>(&json).unwrap();
        let st = applied_status(&ApplyOutcome::Report { report: Box::new(report), note: None, cli_log: String::new() });
        assert_eq!(st.level, Level::Warn);
        assert!(st.text.starts_with("Transplant applied_unverified:"), "{}", st.text);
        assert_eq!(applied_status(&ApplyOutcome::Indeterminate { message: "m".into(), cli_log: String::new() }).level, Level::Error);
    }

    fn clean_report() -> ApplyJson {
        ApplyJson {
            status: ApplyStatus::AppliedVerified,
            no_op: false,
            plan_digest: "d".into(),
            restore_point: Some(RestorePointJson { file: "rp.json".into(), label: "l".into() }),
            restore_point_gate_waived: false,
            written: vec!["kNkMAIDCapability_A".into()],
            write_errors: vec![],
            rejected_by_kind: vec![],
            outcomes: vec![],
            readback: Some(ReadbackJson {
                gate: Some(GateJson { outcome: GateKind::Passed, reason: None, baseline: None, capture: None, missing: None }),
                not_taken: vec![],
                unverifiable: vec![],
                side_effects: Some(SideEffectsJson { changed: vec![], vanished: vec![], appeared: vec![] }),
            }),
            undo_set: vec!["kNkMAIDCapability_A".into()],
            undo_not_restorable: vec![],
        }
    }

    fn levels(r: &ApplyJson) -> Vec<Level> {
        apply_report_lines(r).into_iter().map(|(l, _)| l).collect()
    }

    #[test]
    fn a_clean_verified_report_shows_no_problem_line() {
        let lines = apply_report_lines(&clean_report());
        assert!(lines.iter().all(|(l, _)| *l == Level::Info), "{lines:?}");
        assert!(lines.iter().any(|(_, t)| t.starts_with("Written (1): A")));
        assert!(lines.iter().any(|(_, t)| t == "Read-back: complete."));
    }

    #[test]
    fn each_thing_the_read_back_found_is_a_line_at_the_level_it_deserves() {
        let line_of = |f: &dyn Fn(&mut ApplyJson), want: Level, needle: &str| {
            let mut r = clean_report();
            f(&mut r);
            let lines = apply_report_lines(&r);
            assert!(lines.iter().any(|(l, t)| *l == want && t.contains(needle)), "{needle}: {lines:?}");
        };
        line_of(&|r| r.readback.as_mut().unwrap().not_taken = vec!["kNkMAIDCapability_B".into()], Level::Error, "Did NOT read back as written (1): B");
        line_of(&|r| r.readback.as_mut().unwrap().unverifiable = vec!["kNkMAIDCapability_B".into()], Level::Warn, "Could not be read back (1): B");
        line_of(&|r| r.readback = None, Level::Warn, "No read-back was made");
        line_of(&|r| r.readback.as_mut().unwrap().side_effects = None, Level::Warn, "Side effects: unknown");
        line_of(
            &|r| r.readback.as_mut().unwrap().side_effects = Some(SideEffectsJson { changed: vec!["kNkMAIDCapability_C".into()], vanished: vec!["kNkMAIDCapability_D".into()], appeared: vec![] }),
            Level::Warn,
            "C (changed), D (vanished)",
        );
        line_of(&|r| r.readback.as_mut().unwrap().gate = None, Level::Warn, "nothing could be read");
        line_of(&|r| r.readback.as_mut().unwrap().gate.as_mut().unwrap().outcome = GateKind::Failed, Level::Warn, "INCOMPLETE");
        line_of(&|r| r.readback.as_mut().unwrap().gate.as_mut().unwrap().outcome = GateKind::NoBaseline, Level::Warn, "could not be judged complete");
        line_of(&|r| r.write_errors = vec![WriteErrorJson { name: "kNkMAIDCapability_E".into(), message: "-125".into() }], Level::Error, "Write failed: E: -125");
        line_of(&|r| r.rejected_by_kind = vec!["kNkMAIDCapability_F".into()], Level::Warn, "Not taken by the body");
        line_of(&|r| r.undo_not_restorable = vec!["kNkMAIDCapability_G".into()], Level::Warn, "undo cannot put these back: G");
        line_of(&|r| r.restore_point_gate_waived = true, Level::Warn, "completeness check");
        assert!(!levels(&clean_report()).iter().any(|l| *l != Level::Info));
    }

    #[test]
    fn a_report_that_wrote_nothing_says_why() {
        let mut r = clean_report();
        r.no_op = true;
        r.written.clear();
        r.restore_point = None;
        r.readback = None;
        assert!(apply_report_lines(&r).iter().all(|(l, _)| *l == Level::Info), "nothing to do is not a problem");
        r.status = ApplyStatus::Failed;
        assert!(apply_report_lines(&r).iter().any(|(l, t)| *l == Level::Error && t.contains("not because there was nothing to change")));
    }

    // ── through a stand-in for the CLI ────────────────────────────────────

    #[cfg(unix)]
    fn tp_cli(dir: &Path, tail: &str) -> PathBuf {
        recording_cli(dir, tail)
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_runs_the_cli_as_a_dry_run_and_shows_what_it_printed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("plan.json"), plan_json_text(&[])).unwrap();
        let cli = tp_cli(dir.path(), &format!("cat '{}/plan.json'\necho 'a warning' >&2", dir.path().display()));
        let (plan, log) = transplant_plan_via(&cli, Path::new("/data"), &tp_request(&[]), &|| scan_of(vec![usb("Z 9", "T1")]), T).unwrap();
        assert_eq!(plan, plan_json(&[]));
        assert_eq!(log, "a warning");
        let argv = argv_of(dir.path());
        assert!(argv.contains(&"--dry-run".to_string()) && !argv.iter().any(|a| a.starts_with("--expect")), "{argv:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_target_never_starts_the_cli() {
        let dir = TempDir::new().unwrap();
        let cli = tp_cli(dir.path(), "exit 0");
        let two = || scan_of(vec![usb("Z 9", "T1"), usb("Z 9", "T2")]);
        assert!(transplant_plan_via(&cli, Path::new("/d"), &tp_request(&[]), &two, T).is_err());
        let out = transplant_apply_via(&cli, Path::new("/d"), &tp_request(&[]), "d", "s", &two, T);
        assert!(matches!(out, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, .. }), "{out:?}");
        assert!(!dir.path().join("argv.txt").exists(), "the CLI was never run");
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_for_another_body_than_asked_is_an_error_not_a_plan() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("plan.json"), plan_json_text(&[])).unwrap();
        let cli = tp_cli(dir.path(), &format!("cat '{}/plan.json'", dir.path().display()));
        let mut r = tp_request(&[]);
        r.target.serial = "OTHER".into();
        let e = transplant_plan_via(&cli, Path::new("/d"), &r, &|| scan_of(vec![usb("Z 9", "OTHER")]), T).unwrap_err();
        assert!(e.contains("not for the Z 9 OTHER"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_plan_the_cli_cannot_produce_is_an_error_with_its_message() {
        let dir = TempDir::new().unwrap();
        let cli = tp_cli(dir.path(), "echo 'camera busy' >&2\nexit 1");
        let e = transplant_plan_via(&cli, Path::new("/d"), &tp_request(&[]), &|| scan_of(vec![usb("Z 9", "T1")]), T).unwrap_err();
        assert!(e.contains("camera busy") && e.starts_with("Planning failed"), "{e}");
        let junk = tp_cli(dir.path(), "echo 'not json'");
        let e = transplant_plan_via(&junk, Path::new("/d"), &tp_request(&[]), &|| scan_of(vec![usb("Z 9", "T1")]), T).unwrap_err();
        assert!(e.contains("nothing was written"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn an_apply_runs_the_cli_with_the_confirmed_plan_and_reports_the_status_it_printed() {
        let dir = TempDir::new().unwrap();
        let (json, status) = library_apply_json("unverified");
        let digest = serde_json::from_str::<ApplyJson>(&json).unwrap().plan_digest;
        fs::write(dir.path().join("report.json"), &json).unwrap();
        // Not verified: the CLI exits non-zero after printing.
        let cli = tp_cli(dir.path(), &format!("cat '{}/report.json'\nexit 1", dir.path().display()));
        let out = transplant_apply_via(&cli, Path::new("/data"), &tp_request(&[WB_AUTO]), &digest, "the summary", &|| scan_of(vec![usb("Z 9", "T1")]), T);
        let ApplyOutcome::Report { report, .. } = out else { panic!("{out:?}") };
        assert_eq!(report.status, status);
        let argv = argv_of(dir.path());
        assert!(argv.contains(&format!("--expect-digest={digest}")) && argv.contains(&"--expect-summary=the summary".to_string()), "{argv:?}");
        assert!(argv.contains(&format!("--allow-raw-code={WB_AUTO}")) && !argv.contains(&"--dry-run".to_string()), "{argv:?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_apply_the_cli_calls_stale_is_not_applied_and_a_killed_one_is_indeterminate() {
        let dir = TempDir::new().unwrap();
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let stale = tp_cli(dir.path(), &format!("echo 'the plan is stale' >&2\nexit {EXIT_STALE_PLAN}"));
        let out = transplant_apply_via(&stale, Path::new("/d"), &tp_request(&[]), "d", "s", &scan, T);
        assert!(matches!(&out, ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message, .. } if message.contains("stale")), "{out:?}");
        let killed = tp_cli(dir.path(), "kill -KILL $$");
        let out = transplant_apply_via(&killed, Path::new("/d"), &tp_request(&[]), "d", "s", &scan, T);
        assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "{out:?}");
        let hung = tp_cli(dir.path(), "sleep 30");
        let out = transplant_apply_via(&hung, Path::new("/d"), &tp_request(&[]), "d", "s", &scan, Duration::from_millis(300));
        assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "a run stopped for time may have written: {out:?}");
    }

    // ── choosing a source ────────────────────────────────────────────────

    #[test]
    fn every_body_with_a_stored_snapshot_is_a_source_connected_or_not() {
        let d = TempDir::new().unwrap();
        let snaps = d.path().join("snapshots");
        write_snapshot(&snaps, "a.json", "Z 9", "S1", "2026-01-01T00:00:00Z");
        write_snapshot(&snaps, "b.json", "Z 9", "S1", "2026-01-02T00:00:00Z");
        write_snapshot(&snaps, "c.json", "Z6_3", "S2", "2026-01-03T00:00:00Z");
        let l = list_all(d.path());
        assert_eq!(l.bodies(), [("Z 9".to_string(), "S1".to_string()), ("Z6_3".to_string(), "S2".to_string())]);
        let connected = [row("Z 9", "S1", 1)];
        assert_eq!(l.stored_only_bodies(&connected), [("Z6_3".to_string(), "S2".to_string())]);
        assert_eq!(l.bodies().len(), 2, "a connected body's snapshots are sources too");
    }

    #[test]
    fn starting_from_a_snapshot_keeps_a_chosen_target_and_otherwise_offers_the_selected_camera() {
        let (mut app, _cmd_rx, _evt_tx, _dir) = test_app();
        app.cameras.push(row("Z 8", "T8", 1));
        app.selected = Some(1);
        // A target chosen earlier is not replaced by what happens to be selected in the sidebar.
        app.transplant.target_key = Some(("Z 9".into(), "T1".into()));
        app.transplant.optins.insert(WB_AUTO.into());
        app.transplant.optins_for = Some(("old.json".into(), tp_target()));
        app.start_transplant_from(("Z6_3".into(), "SRC".into()), TP_FILE.into());
        assert!(app.transplant.open);
        assert_eq!(app.transplant.target_key, Some(("Z 9".into(), "T1".into())));
        assert_eq!(app.transplant.source_file.as_deref(), Some(TP_FILE));
        app.transplant.sync(&app.cameras);
        assert!(app.transplant.optins.is_empty(), "a different source starts with no opt-ins");
        // With none chosen it is the selected camera, which the window and the confirmation still name.
        app.transplant.target_key = None;
        app.start_transplant_from(("Z6_3".into(), "SRC".into()), TP_FILE.into());
        assert_eq!(app.transplant.target_key, Some(("Z 8".into(), "T8".into())));
        // With no camera selected there is no target to assume.
        app.transplant.target_key = None;
        app.selected = None;
        app.start_transplant_from(("Z6_3".into(), "SRC".into()), TP_FILE.into());
        assert_eq!(app.transplant.target_key, None);
    }

    // ── the window and the app around it ──────────────────────────────────

    /// An app with no worker behind it: what it sends is read back from `cmd_rx`, and an answer is
    /// delivered through `evt_tx`.
    pub(crate) fn test_app() -> (FleetApp, mpsc::Receiver<Cmd>, mpsc::Sender<Evt>, TempDir) {
        let dir = TempDir::new().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let (view_tx, view_rx) = mpsc::channel();
        let settings = SettingsState {
            path: dir.path().join("settings.json"),
            data_dir: Some(dir.path().to_path_buf()),
            configured: None,
            error: None,
        };
        let app = FleetApp {
            cmd_tx,
            evt_rx,
            cameras: vec![tp_target()],
            selected: Some(0),
            selected_stored: None,
            listing: Some(SnapListing::default()),
            viewers: SnapshotViewers::default(),
            view_tx,
            view_rx,
            label: String::new(),
            status: Status::new(Level::Info, ""),
            notices: Vec::new(),
            cli_log: String::new(),
            pending: 0,
            worker_gone: false,
            busy_since: None,
            settings,
            prefs_open: false,
            prefs_data_dir: String::new(),
            vendor_ops_open: false,
            vendor_propcode: String::new(),
            vendor_read_result: String::new(),
            ftp_form: FtpForm::default(),
            pending_write: None,
            vendor_write_result: String::new(),
            transplant: TransplantUi::default(),
            restore: RestoreUi::default(),
            check: CheckUi::default(),
        };
        (app, cmd_rx, evt_tx, dir)
    }

    #[test]
    fn the_window_sends_only_what_was_planned_and_confirmed_and_shows_the_answer_honestly() {
        let (mut app, cmd_rx, evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        let (ui, _) = ui_with_plan(&[]);
        app.transplant = ui;
        let shown = plan_json(&[]);

        // Applying without having been asked the question sends nothing.
        app.transplant_act(&ctx, TransplantAction::ConfirmApply);
        assert!(cmd_rx.try_recv().is_err());
        app.transplant_act(&ctx, TransplantAction::AskApply);
        assert!(cmd_rx.try_recv().is_err(), "asking is not applying");
        app.transplant_act(&ctx, TransplantAction::ConfirmApply);
        let Ok(Cmd::TransplantApply { request, digest, summary }) = cmd_rx.try_recv() else { panic!("an apply") };
        let sent_digest = digest.clone();
        assert_eq!((request, digest, summary), (tp_request(&[]), shown.digest.clone(), shown.summary));
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.busy());

        // The answer: unverified is shown as unverified, and the new restore point is re-listed. The
        // CLI echoes the plan it was held to, so the report names the digest the window SENT (read off
        // the command on the wire, which came from the plan on screen), and the outcome is judged
        // against that digest; it is not read back out of the report that is being judged.
        let (json, _) = library_apply_json("unverified");
        let own_digest = serde_json::from_str::<ApplyJson>(&json).unwrap().plan_digest;
        assert_ne!(own_digest, sent_digest, "the fixture's own plan is a different plan from the one on screen");
        let held_to = with_plan_digest(&json, &sent_digest);
        let elsewhere = classify_apply(Ok(CliOutput { stdout: json, stderr: String::new() }), &sent_digest);
        assert!(matches!(elsewhere, ApplyOutcome::Indeterminate { .. }), "a report of another plan is not the answer: {elsewhere:?}");
        let outcome = classify_apply(Ok(CliOutput { stdout: held_to, stderr: String::new() }), &sent_digest);
        assert!(matches!(outcome, ApplyOutcome::Report { .. }), "{outcome:?}");
        let mut request = tp_request(&[]);
        request.target = tp_target();
        evt_tx.send(Evt::TransplantApplied { request: request.clone(), outcome }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Warn);
        assert!(app.status.text.starts_with("Transplant applied_unverified:"), "{}", app.status.text);
        assert!(matches!(app.transplant.stage, TransplantStage::Done(_)));
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::RefreshListing)), "the restore point is a new file");
    }

    #[test]
    fn a_refused_apply_changes_nothing_and_does_not_relist_and_a_stale_plan_says_so() {
        let (mut app, cmd_rx, evt_tx, _dir) = test_app();
        let (ui, _) = ui_with_plan(&[]);
        app.transplant = ui;
        let ctx = egui::Context::default();
        app.transplant_act(&ctx, TransplantAction::AskApply);
        app.transplant_act(&ctx, TransplantAction::ConfirmApply);
        cmd_rx.try_recv().unwrap();
        let outcome = classify_apply(Err(cli_error(CliFailure::Exit, Some(EXIT_STALE_PLAN), "")), "d");
        evt_tx.send(Evt::TransplantApplied { request: tp_request(&[]), outcome }).unwrap();
        app.poll();
        assert!(app.status.text.contains("no longer matches the plan you approved") && app.status.text.contains("nothing was written"), "{}", app.status.text);
        assert!(cmd_rx.try_recv().is_err(), "nothing was written, so nothing to re-list");
    }

    #[test]
    fn planning_goes_through_the_worker_and_a_refused_target_does_not() {
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        let ctx = egui::Context::default();
        app.transplant = ui_ready().0;
        app.transplant_act(&ctx, TransplantAction::Plan);
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::TransplantPlan { request }) if request == tp_request(&[])));
        app.transplant = ui_ready().0;
        app.cameras[0].model_count = 2;
        app.transplant_act(&ctx, TransplantAction::Plan);
        assert!(cmd_rx.try_recv().is_err());
        assert!(app.transplant.notice.contains("wrong physical body"), "{}", app.transplant.notice);
    }

    #[test]
    fn the_window_draws_in_every_stage_and_the_sidebar_flow_opens_it_on_the_chosen_snapshot() {
        let (mut app, _cmd_rx, _evt_tx, dir) = test_app();
        write_snapshot(&dir.path().join("snapshots"), TP_FILE, "Z6_3", "SRC", "2026-10-27T10:00:00Z");
        app.listing = Some(list_all(dir.path()));
        app.start_transplant_from(("Z6_3".into(), "SRC".into()), TP_FILE.into());
        assert!(app.transplant.open);
        assert_eq!(app.transplant.target_key, Some(("Z 9".into(), "T1".into())), "the selected camera, to be read in the window");
        let ctx = egui::Context::default();
        let (ui_planned, _) = ui_with_plan(&[WB_AUTO]);
        let TransplantStage::Planned(p) = ui_planned.stage.clone() else { panic!() };
        let (report, _) = library_apply_json("failed");
        let applied = AppliedTransplant {
            request: tp_request(&[]),
            outcome: ApplyOutcome::Report { report: Box::new(serde_json::from_str(&report).unwrap()), note: None, cli_log: "log".into() },
        };
        let stages = vec![
            TransplantStage::Idle,
            TransplantStage::Planning(tp_request(&[])),
            TransplantStage::Planned(p.clone()),
            TransplantStage::OptIn { plan: p.clone(), property: WB_AUTO.into() },
            TransplantStage::ConfirmApply(p),
            TransplantStage::Applying(tp_request(&[])),
            TransplantStage::Done(Box::new(applied.clone())),
            TransplantStage::Done(Box::new(AppliedTransplant {
                outcome: ApplyOutcome::Indeterminate { message: "m".into(), cli_log: String::new() },
                ..applied.clone()
            })),
            TransplantStage::Done(Box::new(AppliedTransplant {
                outcome: ApplyOutcome::NotApplied { why: NotAppliedWhy::PlanStale, message: "m".into(), cli_log: String::new() },
                ..applied
            })),
        ];
        for stage in stages {
            app.transplant.stage = stage;
            for _ in 0..2 {
                let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui_transplant(ctx));
            }
        }
        app.transplant.open = false;
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui_transplant(ctx));
    }

    // ── the review's findings ─────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn a_stand_in_cli_that_refuses_with_the_documented_status_is_not_applied_and_any_other_silence_is_not() {
        let dir = TempDir::new().unwrap();
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let refused = tp_cli(dir.path(), &format!("echo 'no baseline' >&2\nexit {EXIT_REFUSED_BEFORE_WRITE}"));
        let out = transplant_apply_via(&refused, Path::new("/d"), &tp_request(&[]), "d", "s", &scan, T);
        assert!(matches!(&out, ApplyOutcome::NotApplied { why: NotAppliedWhy::Refused, message, .. } if message.contains("no baseline")), "{out:?}");
        // exit 1 with nothing on stdout is what a CLI that wrote, did not fully take and could not print looks like
        let silent = tp_cli(dir.path(), "exit 1");
        let out = transplant_apply_via(&silent, Path::new("/d"), &tp_request(&[]), "d", "s", &scan, T);
        assert!(matches!(out, ApplyOutcome::Indeterminate { .. }), "{out:?}");
    }

    // F2: opt-ins live and die with the plan they were made for

    #[test]
    fn discarding_a_plan_or_a_result_takes_its_opt_ins_with_it() {
        for from in ["plan", "result"] {
            let (mut ui, cameras) = ui_with_plan(&[WB_AUTO]);
            ui.optin_values.insert(WB_AUTO.into(), "5, was 3".into());
            if from == "result" {
                ui.ask_apply();
                ui.confirm_apply().unwrap();
                ui.optins.insert(WB_AUTO.into());
                ui.applied(tp_request(&[WB_AUTO]), ApplyOutcome::Indeterminate { message: "x".into(), cli_log: String::new() });
            }
            ui.discard();
            assert_eq!(ui.stage, TransplantStage::Idle, "{from}");
            assert!(ui.optins.is_empty() && ui.optin_values.is_empty(), "{from}");
            ui.sync(&cameras);
            let Ok(Cmd::TransplantPlan { request }) = ui.begin_plan(&cameras) else { panic!("a plan") };
            assert!(request.allow_raw_code.is_empty(), "{from}: the next plan starts with none");
        }
        // Only a plan or a result is discarded: a plan being made, a confirmation and an apply on the wire are not.
        for held in [
            TransplantStage::Planning(tp_request(&[WB_AUTO])),
            TransplantStage::Applying(tp_request(&[WB_AUTO])),
            TransplantStage::ConfirmApply(planned(&[WB_AUTO])),
        ] {
            let (mut ui, _) = ui_ready();
            ui.optins.insert(WB_AUTO.into());
            ui.stage = held.clone();
            ui.discard();
            assert_eq!(ui.stage, held);
            assert!(!ui.optins.is_empty());
        }
    }

    #[test]
    fn a_plan_that_fails_takes_the_opt_ins_it_was_made_with() {
        let (mut ui, cameras) = ui_with_plan(&[]);
        ui.ask_opt_in(WB_AUTO);
        let Some(Cmd::TransplantPlan { request }) = ui.confirm_opt_in(&cameras) else { panic!("a new plan") };
        assert!(ui.optins.contains(WB_AUTO));
        ui.plan_arrived(request, Err("Planning failed: camera busy".into()));
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert!(ui.optins.is_empty() && ui.optin_values.is_empty(), "a failed plan leaves no opt-in waiting unseen");
        let Ok(Cmd::TransplantPlan { request }) = ui.begin_plan(&cameras) else { panic!("a plan") };
        assert!(request.allow_raw_code.is_empty());
    }

    #[test]
    fn an_opt_in_that_could_not_be_planned_is_not_kept() {
        let (mut ui, _) = ui_with_plan(&[]);
        ui.ask_opt_in(WB_AUTO);
        assert!(ui.confirm_opt_in(&[]).is_none(), "the target is no longer connected");
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert!(ui.optins.is_empty() && ui.optin_values.is_empty());
    }

    #[test]
    fn a_withdrawn_plan_takes_its_opt_ins_and_they_do_not_ride_into_the_next_plan() {
        let (mut ui, mut cameras) = ui_with_plan(&[WB_AUTO]);
        ui.sync(&cameras);
        assert!(ui.optins.contains(WB_AUTO));
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert!(ui.optins.is_empty(), "the firmware changed under the plan and its opt-ins");
        // The firmware goes back: the opt-ins do not.
        cameras[0].firmware = "5.31".into();
        ui.sync(&cameras);
        let Ok(Cmd::TransplantPlan { request }) = ui.begin_plan(&cameras) else { panic!("a plan") };
        assert!(request.allow_raw_code.is_empty());
        // A withdrawal the key of the opt-ins does not see (same file, same row, another source body)
        // takes them too: leaving a plan always leaves its opt-ins.
        let (mut ui, cameras) = ui_with_plan(&[WB_AUTO]);
        ui.sync(&cameras);
        ui.source_body = Some(("Z6_3".into(), "ELSE".into()));
        ui.sync(&cameras);
        assert_eq!(ui.stage, TransplantStage::Idle);
        assert!(ui.optins.is_empty() && ui.optin_values.is_empty());
    }

    #[test]
    fn opt_ins_are_given_for_the_whole_target_row_not_only_its_name() {
        let (mut ui, mut cameras) = ui_ready();
        ui.sync(&cameras);
        ui.optins.insert(WB_AUTO.into());
        ui.optin_values.insert(WB_AUTO.into(), "5, was 3".into());
        // Nothing is held (no stage to withdraw), so only the key of the opt-ins can end them.
        ui.sync(&cameras);
        assert!(!ui.optins.is_empty(), "the same row keeps them");
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert!(ui.optins.is_empty() && ui.optin_values.is_empty(), "the same model and serial at another firmware is not the body they were given for");
        ui.optins.insert(WB_AUTO.into());
        ui.optins_for = None;
        ui.sync(&cameras);
        assert!(ui.optins.is_empty(), "a target that is not among the connected cameras has none either");
    }

    // F3: the value an opt-in sets reaches the apply confirmation

    #[test]
    fn what_an_opt_in_sets_is_remembered_from_the_plan_that_offered_it_to_the_apply_confirmation() {
        let (mut ui, cameras) = ui_with_plan(&[]);
        ui.ask_opt_in(WB_AUTO);
        let Some(Cmd::TransplantPlan { request }) = ui.confirm_opt_in(&cameras) else { panic!("a new plan") };
        // The re-made plan no longer carries the proposal: the skip became a copy.
        ui.plan_arrived(request, Ok((plan_json(&[WB_AUTO]), String::new())));
        ui.ask_apply();
        let TransplantStage::ConfirmApply(p) = &ui.stage else { panic!("{:?}", ui.stage) };
        let text = apply_confirm_text(p);
        assert!(text.contains("WBAutoType (sets 5, was 3)"), "{text}");
        // A value belongs to an opt-in that is still in the request.
        let TransplantStage::ConfirmApply(p) = ui.stage.clone() else { panic!() };
        ui.stage = TransplantStage::Planned(p);
        let Some(Cmd::TransplantPlan { request }) = ui.remove_opt_in(WB_AUTO, &cameras) else { panic!("a new plan") };
        ui.plan_arrived(request, Ok((plan_json(&[]), String::new())));
        let TransplantStage::Planned(p) = &ui.stage else { panic!() };
        assert!(p.optin_values.is_empty());
    }

    // F4: a plan is checked against the snapshot and the firmware too, and the CLI gets an absolute path

    #[test]
    fn a_plan_of_another_snapshot_body_or_for_another_firmware_is_not_shown() {
        let plan = plan_json(&[]);
        assert!(check_plan_is_for(&plan, &tp_request(&[])).is_ok());
        let mut other_model = tp_request(&[]);
        other_model.source_body.0 = "Z 8".into();
        let e = check_plan_is_for(&plan, &other_model).unwrap_err();
        assert!(e.contains("Z6_3 SRC") && e.contains("Z 8 SRC") && e.contains("nothing was written"), "{e}");
        let mut other_serial = tp_request(&[]);
        other_serial.source_body.1 = "OTHER".into();
        assert!(check_plan_is_for(&plan, &other_serial).unwrap_err().contains("Z6_3 OTHER"));
        let mut newer = tp_request(&[]);
        newer.target.firmware = "5.40".into();
        let e = check_plan_is_for(&plan, &newer).unwrap_err();
        assert!(e.contains("firmware 5.31") && e.contains("5.40"), "{e}");
    }

    #[test]
    fn the_cli_is_given_the_snapshot_by_absolute_path_so_a_file_in_its_working_directory_cannot_stand_in() {
        for data in ["/data", "relative/data"] {
            let argv = transplant_argv(Path::new(data), &tp_request(&[]), None).unwrap();
            let source = Path::new(argv.last().unwrap());
            assert!(source.is_absolute(), "{source:?}");
            assert!(source.ends_with(Path::new("snapshots").join(TP_FILE)), "{source:?}");
            assert_eq!(source, Path::new(&source_arg(data, TP_FILE)));
        }
    }

    // F5: a change of data directory withdraws what was planned under the old one

    #[test]
    fn a_change_of_data_directory_withdraws_a_plan_and_each_confirmation_but_not_an_apply_on_the_wire() {
        for stage in ["planning", "planned", "opt-in", "confirm"] {
            let (mut app, _cmd_rx, _evt_tx, _dir) = test_app();
            let (mut ui, _) = ui_with_plan(&[WB_AUTO]);
            match stage {
                "planning" => ui.stage = TransplantStage::Planning(tp_request(&[WB_AUTO])),
                "opt-in" => ui.ask_opt_in(METERING),
                "confirm" => ui.ask_apply(),
                _ => {}
            }
            app.transplant = ui;
            app.use_settings_data_dir();
            assert_eq!(app.transplant.stage, TransplantStage::Idle, "{stage}");
            assert!(app.transplant.notice.contains("data directory") && app.transplant.notice.contains("Nothing was written"), "{}", app.transplant.notice);
            assert!(app.transplant.optins.is_empty(), "{stage}");
        }
        let (mut app, _cmd_rx, _evt_tx, _dir) = test_app();
        app.transplant.stage = TransplantStage::Applying(tp_request(&[]));
        app.use_settings_data_dir();
        assert!(matches!(app.transplant.stage, TransplantStage::Applying(_)), "it is on the wire; the answer must find it");
        let mut idle = test_app().0;
        idle.use_settings_data_dir();
        assert!(idle.transplant.notice.is_empty(), "nothing was withdrawn, so nothing is said");
    }

    // F6: honest wording, and no wait for a worker that is gone

    #[test]
    fn a_failed_apply_says_some_writes_may_have_landed_and_points_to_the_restore_point() {
        let (json, _) = library_apply_json("failed");
        let report: ApplyJson = serde_json::from_str(&json).unwrap();
        assert!(report.restore_point.is_some());
        let lines = apply_report_lines(&report);
        assert_eq!(lines[0].0, Level::Error);
        assert!(lines[0].1.contains("may have landed") && lines[0].1.contains("restore point"), "{lines:?}");
        let status = applied_status(&ApplyOutcome::Report { report: Box::new(report.clone()), note: None, cli_log: String::new() });
        assert!(status.text.contains("may have landed") && status.text.contains("restore point"), "{}", status.text);
        // Not for a status that did not fail, and not where there is no restore point to point to.
        let (ok, _) = library_apply_json("verified");
        let ok: ApplyJson = serde_json::from_str(&ok).unwrap();
        assert!(!apply_report_lines(&ok).iter().any(|(_, t)| t.contains("may have landed")));
        let mut no_restore_point = report;
        no_restore_point.restore_point = None;
        assert!(!apply_report_lines(&no_restore_point).iter().any(|(_, t)| t.contains("may have landed")));
        let status = applied_status(&ApplyOutcome::Report { report: Box::new(no_restore_point), note: None, cli_log: String::new() });
        assert!(!status.text.contains("may have landed"), "{}", status.text);
    }

    #[test]
    fn a_worker_that_stopped_ends_the_wait_for_an_answer_it_can_no_longer_give() {
        // An apply on the wire: the worker is gone and the CLI may have written.
        let (mut app, _cmd_rx, evt_tx, _dir) = test_app();
        let (mut ui, _) = ui_with_plan(&[WB_AUTO]);
        ui.ask_apply();
        ui.confirm_apply().unwrap();
        app.transplant = ui;
        app.pending = 1;
        drop(evt_tx);
        app.poll();
        let TransplantStage::Done(done) = &app.transplant.stage else { panic!("{:?}", app.transplant.stage) };
        assert!(matches!(&done.outcome, ApplyOutcome::Indeterminate { message, .. } if message.contains("worker thread stopped")), "{:?}", done.outcome);
        assert!(app.transplant.optins.is_empty());
        assert!(!app.busy() && app.status.level == Level::Error, "{:?}", app.status);
        // A plan being made: nothing was written.
        let (mut app, _cmd_rx, evt_tx, _dir) = test_app();
        app.transplant = ui_ready().0;
        let cmd = app.transplant.begin_plan(&app.cameras).unwrap();
        app.send(cmd);
        drop(evt_tx);
        app.poll();
        assert_eq!(app.transplant.stage, TransplantStage::Idle);
        assert!(app.transplant.notice.contains("Nothing was written"), "{}", app.transplant.notice);
        // Said once: a worker already known to be gone does not overwrite every later status.
        app.status = Status::new(Level::Info, "later");
        app.poll();
        assert_eq!(app.status.text, "later");
    }

    #[test]
    fn a_command_the_worker_could_not_take_ends_the_wait_and_says_it_was_never_sent() {
        let ctx = egui::Context::default();
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        drop(cmd_rx);
        app.transplant = ui_ready().0;
        app.transplant_act(&ctx, TransplantAction::Plan);
        assert_eq!(app.transplant.stage, TransplantStage::Idle);
        assert!(app.transplant.notice.contains("Nothing was written"), "{}", app.transplant.notice);
        assert!(!app.busy());
        let (mut app, cmd_rx, _evt_tx, _dir) = test_app();
        let (ui, _) = ui_with_plan(&[]);
        app.transplant = ui;
        app.transplant_act(&ctx, TransplantAction::AskApply);
        drop(cmd_rx);
        app.transplant_act(&ctx, TransplantAction::ConfirmApply);
        let TransplantStage::Done(done) = &app.transplant.stage else { panic!("{:?}", app.transplant.stage) };
        assert!(matches!(&done.outcome, ApplyOutcome::NotApplied { message, .. } if message.contains("never sent")), "{:?}", done.outcome);
        assert!(!app.busy());
    }

    // F7: one definition of "would change"

    #[test]
    fn a_copy_with_no_planned_write_is_no_change_and_one_with_an_unreadable_value_is_a_possible_one() {
        let from_json = |extra: &str| -> OutcomeJson {
            serde_json::from_str(&format!(r#"{{"name":"kNkMAIDCapability_P","code":1,"status":"copied","strategy":"literal"{extra}}}"#)).unwrap()
        };
        assert!(!from_json("").would_change(), "no `changes` key: no planned write");
        assert!(from_json(r#","changes":null"#).would_change(), "null: the live value could not be read");
        assert!(from_json(r#","changes":true"#).would_change());
        assert!(!from_json(r#","changes":false"#).would_change());
        let a_skip: OutcomeJson =
            serde_json::from_str(r#"{"name":"kNkMAIDCapability_P","code":1,"status":"skipped","reason":{"tag":"read_only","severity":"routine","detail":null},"changes":true}"#).unwrap();
        assert!(!a_skip.would_change(), "a skip never changes anything");
        // And it is the library's answer, for every shape the library's own serialiser produces.
        let shapes = [
            copied("A", 1, Strategy::Literal, None, Some(true)),
            copied("B", 2, Strategy::Literal, None, Some(false)),
            copied("C", 3, Strategy::Literal, None, None),
            TransplantOutcome { planned: None, ..copied("D", 4, Strategy::Literal, None, None) },
            skipped("E", 5, SkipReason::ReadOnly),
        ];
        for o in shapes {
            let parsed: OutcomeJson = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
            assert_eq!(parsed.would_change(), nikon_fleet::ops::plan::would_change(&o), "{}", o.name);
        }
    }
}
