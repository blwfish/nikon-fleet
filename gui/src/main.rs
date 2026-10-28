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
use nikon_fleet::ops::target::storable_serial;
use nikon_fleet::ptp_usb::{self, FtpField, FtpProfile};
use nikon_fleet::settings::{self, Settings};
use nikon_fleet::snapshot::Snapshot;
use nikon_fleet::vendor_argv::{vendor_read_argv, vendor_write_ftp_argv};
use serde::Deserialize;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

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
}

enum Evt {
    Cameras(DiscoverResult),
    Listing(SnapListing),
    Done(OpDone),
    Vendor { op: VendorOp, result: Result<VendorOut, String> },
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

// ── App ───────────────────────────────────────────────────────────────────

struct FleetApp {
    cmd_tx: mpsc::Sender<Cmd>,
    evt_rx: mpsc::Receiver<Evt>,
    cameras: Vec<CameraRow>,
    selected: Option<usize>,
    listing: Option<SnapListing>,
    label: String,
    status: Status,
    notices: Vec<String>,
    cli_log: String,
    /// Commands sent and not yet answered; every command is answered by exactly one event.
    pending: usize,
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
}

impl FleetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = load_settings_state(&settings::settings_path());
        let prefs_data_dir = settings.configured.clone().unwrap_or_default();
        let status = settings_error_status(&settings)
            .unwrap_or_else(|| Status::new(Level::Info, "Click Discover to find cameras."));

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        let initial = settings.data_dir_or_err();
        std::thread::spawn(move || worker(cmd_rx, evt_tx, ctx, initial));

        let mut app = Self {
            cmd_tx,
            evt_rx,
            cameras: Vec::new(),
            selected: None,
            listing: None,
            label: String::new(),
            status,
            notices: Vec::new(),
            cli_log: String::new(),
            pending: 0,
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

    fn send(&mut self, cmd: Cmd) {
        if self.pending == 0 {
            self.busy_since = Some(Instant::now());
        }
        self.pending += 1;
        if self.cmd_tx.send(cmd).is_err() {
            self.pending -= 1;
            self.status = Status::new(Level::Error, "The worker thread has stopped; restart the app.");
        }
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

    fn poll(&mut self) {
        while let Ok(evt) = self.evt_rx.try_recv() {
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
        self.drop_stale_confirmation();
        self.ui_toolbar(ctx);
        self.ui_notices(ctx);
        self.ui_preferences(ctx);
        self.ui_vendor_ops(ctx);
        self.ui_sidebar(ctx);
        self.ui_main(ctx);
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
        });
        if let Some(i) = cam_select {
            self.selected = Some(i);
        }
    }

    fn ui_main(&mut self, ctx: &egui::Context) {
        let mut snap_trigger = false;
        let mut ref_action: Option<String> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(cam) = self.selected.and_then(|i| self.cameras.get(i)) else {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Select a camera to see snapshots").italics().weak());
                });
                return;
            };

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
            });
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
                for snap in listing.rows_for(&cam.model, &cam.serial) {
                    ui.horizontal(|ui| {
                        let ts = snap.captured_at.get(..19).unwrap_or(&snap.captured_at);
                        let lbl = snap.label.as_deref().unwrap_or("(no label)");
                        ui.label(format!("{ts}  {lbl}"));
                        if snap.is_reference {
                            ui.label(egui::RichText::new("◀ ref").color(egui::Color32::LIGHT_GREEN));
                        } else {
                            ui.add_enabled_ui(!busy && !snap.name_lossy, |ui| {
                                if ui.small_button("Set ref").clicked() {
                                    ref_action = Some(snap.file.clone());
                                }
                            });
                        }
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
        if let Some(file) = ref_action {
            self.send(Cmd::SetReference { file });
        }
    }
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
/// both in the same target directory). An override that points nowhere is an
/// error — it is not silently ignored in favour of the sibling, which would
/// hide a typo'd path.
fn resolve_cli(
    env_override: Option<OsString>,
    current_exe: Option<&Path>,
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
    let sibling = exe.with_file_name(CLI_NAME);
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
    resolve_cli(std::env::var_os(CLI_ENV), std::env::current_exe().ok().as_deref(), Path::is_file)
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

impl std::fmt::Display for SnapshotRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Snapshot refused, nothing was read: ")?;
        match self {
            SnapshotRefusal::AmbiguousPairing(a) => write!(f, "{a}"),
            SnapshotRefusal::BodiesUnreadable { count } => write!(
                f,
                "{count} Nikon USB device(s) are attached but could not be read, so the serial pairing cannot be \
                 verified. Check the cables and that no other program holds the cameras, then Discover again."
            ),
            SnapshotRefusal::UsbEnumerationFailed(e) => {
                write!(f, "the USB bus could not be enumerated ({e}), so the attached bodies are unknown.")
            }
        }
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
#[derive(Debug, Deserialize)]
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
    use nikon_fleet::ops::baseline::{GateOutcome, NoBaselineReason};
    use nikon_fleet::ops::capture::CaptureStats;
    use nikon_fleet::ops::capture_snapshot::CaptureOutcome;
    use nikon_fleet::ops::cameras::{describe_device, describe_scan};
    use nikon_fleet::sdk::{DeviceInfo, UsbCameraInfo, UsbScan, UsbStage, UsbUnreadable};
    use nikon_fleet::snapshot::{Camera, Transport};
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const T: Duration = Duration::from_secs(30);

    // ── helpers ────────────────────────────────────────────────────────────

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A stand-in for the CLI: records its argv (one per line) and stdin next to it, then runs `tail`.
    fn recording_cli(dir: &Path, tail: &str) -> PathBuf {
        script(
            dir,
            "nikon-fleet",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{d}/argv.txt'\ncat > '{d}/stdin.txt'\n{tail}",
                d = dir.display()
            ),
        )
    }

    fn argv_of(dir: &Path) -> Vec<String> {
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

    fn write_snapshot(dir: &Path, file: &str, model: &str, serial: &str, captured_at: &str) {
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

    fn row(model: &str, serial: &str, model_count: usize) -> CameraRow {
        CameraRow {
            model: model.into(),
            serial: serial.into(),
            firmware: "5.31".into(),
            known_model: true,
            baseline: true,
            model_count,
        }
    }

    fn usb(model: &str, serial: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 0, serial: serial.into(), firmware: "5.31".into(), model: model.into() }
    }

    fn scan_of(cameras: Vec<UsbCameraInfo>) -> CameraList {
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
        let s = save_data_dir(&path, "  /new/dir  ").unwrap();
        assert_eq!(s.data_dir, Some(PathBuf::from("/new/dir")), "pasted whitespace is trimmed");
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["data_dir"], "/new/dir");
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
        let err = save_data_dir(&path, "/new").unwrap_err();
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
        assert_eq!(save_data_dir(&path, "/abs").unwrap().data_dir, Some(PathBuf::from("/abs")));
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

    fn sh(script: &str) -> (PathBuf, Vec<String>) {
        (PathBuf::from("/bin/sh"), vec!["-c".to_string(), script.to_string()])
    }

    #[test]
    fn run_cli_returns_trimmed_stdout_on_success() {
        let (p, a) = sh("echo '  hello  '");
        assert_eq!(run_cli(&p, &a, None, T).unwrap().stdout, "hello");
    }

    #[test]
    fn run_cli_pipes_stdin_exactly_and_closes_it() {
        // `wc -c` only finishes once stdin hits EOF, so this also proves the
        // pipe is closed; and 9 bytes proves no newline was appended.
        let (p, a) = sh("wc -c");
        assert_eq!(run_cli(&p, &a, Some("pa ss--wd"), T).unwrap().stdout, "9");
    }

    #[test]
    fn run_cli_non_zero_exit_returns_stderr_as_a_typed_exit_failure() {
        let (p, a) = sh("echo 'boom: no camera' >&2; exit 3");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (CliFailure::Exit, "boom: no camera"));
        assert!(!e.outcome_unknown());
    }

    #[test]
    fn run_cli_non_zero_exit_with_empty_stderr_reports_the_status() {
        let (p, a) = sh("exit 3");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert!(e.message.contains("exited with") && e.message.contains('3'), "{}", e.message);
    }

    #[test]
    fn run_cli_child_killed_by_signal_is_an_error_not_a_gui_crash() {
        // The point of #12: if the USB churn takes the child down, the GUI
        // gets a message instead of dying with it.
        let (p, a) = sh("kill -SEGV $$");
        let e = run_cli(&p, &a, None, T).unwrap_err();
        assert!(e.message.contains("signal"), "{}", e.message);
    }

    #[test]
    fn run_cli_stderr_on_success_reaches_the_caller() {
        let (p, a) = sh("echo 'warning: unconfirmed model' >&2; echo ok");
        let out = run_cli(&p, &a, None, T).unwrap();
        assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("ok", "warning: unconfirmed model"));
    }

    #[test]
    fn run_cli_child_that_exits_without_reading_stdin_reports_its_own_error() {
        // The stdin write fails (EPIPE) — a non-zero child's stderr must still win.
        let (p, a) = sh("echo 'rejected: bad args' >&2; exit 2");
        let e = run_cli(&p, &a, Some(&"x".repeat(1_000_000)), T).unwrap_err();
        assert_eq!((e.kind, e.message.as_str()), (CliFailure::Exit, "rejected: bad args"));
    }

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

    #[test]
    fn run_cli_timeout_boundary_a_child_inside_the_limit_succeeds() {
        let (p, a) = sh("sleep 0.2; echo done");
        assert_eq!(run_cli(&p, &a, None, Duration::from_secs(20)).unwrap().stdout, "done");
        let (p, a) = sh("exec sleep 5");
        assert_eq!(run_cli(&p, &a, None, Duration::from_millis(300)).unwrap_err().kind, CliFailure::TimedOut);
    }

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
        let got = resolve_cli(Some("/opt/x/nikon-fleet".into()), Some(Path::new("/t/fleet-gui")), |_| true);
        assert_eq!(got, Ok(PathBuf::from("/opt/x/nikon-fleet")));
    }

    #[test]
    fn resolve_cli_bad_env_override_does_not_fall_back_to_the_sibling() {
        // A typo'd override must fail loudly even though the sibling exists.
        let got = resolve_cli(Some("/typo/nikon-fleet".into()), Some(Path::new("/t/fleet-gui")), |p| {
            p == Path::new("/t/nikon-fleet")
        });
        let e = got.unwrap_err();
        assert!(e.contains(CLI_ENV) && e.contains("/typo/nikon-fleet"), "{e}");
    }

    #[test]
    fn resolve_cli_finds_the_sibling_of_the_gui_executable() {
        let got = resolve_cli(None, Some(Path::new("/t/debug/fleet-gui")), |p| p == Path::new("/t/debug/nikon-fleet"));
        assert_eq!(got, Ok(PathBuf::from("/t/debug/nikon-fleet")));
    }

    #[test]
    fn resolve_cli_missing_sibling_says_how_to_build_it() {
        let e = resolve_cli(None, Some(Path::new("/t/debug/fleet-gui")), |_| false).unwrap_err();
        assert!(e.contains("/t/debug/nikon-fleet") && e.contains("cargo build --workspace") && e.contains(CLI_ENV), "{e}");
    }

    #[test]
    fn resolve_cli_without_a_known_exe_path_asks_for_the_override() {
        let e = resolve_cli(None, None, |_| true).unwrap_err();
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

    #[test]
    fn discover_that_prints_its_result_and_then_hangs_is_taken_with_a_note() {
        // F24: the SDK's close hangs after the result is out.
        let d = TempDir::new().unwrap();
        let cli = recording_cli(d.path(), &format!("cat <<'EOF'\n{TWO_Z9}\nEOF\nexec sleep 30"));
        let found = discover_via(&cli, &|| scan_of(vec![]), Duration::from_millis(4000)).unwrap();
        assert_eq!(found.rows.len(), 3);
        assert!(found.notices.iter().any(|n| n.contains("did not exit")), "{:?}", found.notices);
    }

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

    #[test]
    fn a_refused_snapshot_never_starts_the_cli() {
        let d = TempDir::new().unwrap();
        let cli = recording_cli(d.path(), "echo '{}'");
        let two = scan_of(vec![usb("Z 9", "AAA"), usb("Z 9", "BBB")]);
        let e = snapshot_via(&cli, d.path(), &row("Z 9", "AAA", 2), "", &|| two.clone(), T).unwrap_err();
        assert!(e.contains("Attach one at a time"), "{e}");
        assert!(!d.path().join("argv.txt").exists(), "the CLI was not run");
    }

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

    #[test]
    fn vendor_write_failure_is_an_error_and_no_success_message() {
        let dir = TempDir::new().unwrap();
        let cli = script(dir.path(), "nikon-fleet", "cat >/dev/null\necho 'Error: could not claim the PTP USB interface' >&2\nexit 1");
        assert_eq!(vendor_write_via(&cli, &sample_profile(), None, T).unwrap_err(), "Error: could not claim the PTP USB interface");
    }

    #[test]
    fn vendor_read_failure_is_an_error() {
        let dir = TempDir::new().unwrap();
        let cli = script(dir.path(), "nikon-fleet", "echo 'Error: no Nikon USB camera found' >&2\nexit 1");
        assert_eq!(vendor_read_via(&cli, 0xD053, None, T).unwrap_err(), "Error: no Nikon USB camera found");
    }

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
}
