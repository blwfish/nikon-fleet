use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;

use eframe::egui;
use nikon_fleet::firmware::model_slug;
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::sdk::{DeviceInfo, Sdk, OP_GET, UsbCameraInfo, pair_devices, usb_camera_list};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

// ── Compile-time SDK / schema paths ──────────────────────────────────────

#[cfg(target_os = "macos")]
const SDK_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../sdk-runtime/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module"
);
#[cfg(target_os = "windows")]
const SDK_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../sdk-runtime/ControlServiceLayer.dll"
);
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const SDK_PATH: &str = "";

const SCHEMA_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../sdk-runtime/MaidLayer.config"
);

// ── Settings ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    data_dir: Option<String>,
}

impl Settings {
    fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    fn effective_data_dir(&self) -> PathBuf {
        self.data_dir
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir)
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("net.blw.fleet")
        .join("settings.json")
}

fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("net.blw.fleet")
}

// ── Worker thread protocol ────────────────────────────────────────────────

enum Cmd {
    Discover,
    Snapshot { serial: String, label: String },
    ListSnapshots { serial: String },
    SetReference { filename: String },
    SetDataDir(PathBuf),
    Export { dest: PathBuf },
    Import { src: PathBuf },
}

enum Evt {
    Cameras(Vec<CameraRow>),
    SnapshotDone(String),
    Snapshots(Vec<SnapRow>),
    ReferenceDone,
    ExportDone { dest: PathBuf, count: usize },
    ImportDone { snapshots: usize, references: usize },
    Err(String),
}

#[derive(Clone)]
struct CameraRow {
    model: String,
    serial: String,
    firmware: String,
}

#[derive(Clone)]
struct SnapRow {
    filename: String,
    label: Option<String>,
    captured_at: String,
    is_reference: bool,
}

// ── App ───────────────────────────────────────────────────────────────────

struct FleetApp {
    cmd_tx: mpsc::Sender<Cmd>,
    evt_rx: mpsc::Receiver<Evt>,
    cameras: Vec<CameraRow>,
    selected: Option<usize>,
    snapshots: Vec<SnapRow>,
    label: String,
    status: String,
    busy: bool,
    // Preferences
    settings: Settings,
    prefs_open: bool,
    prefs_data_dir: String,
}

impl FleetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        let data_dir = settings.effective_data_dir();
        let prefs_data_dir = data_dir.to_string_lossy().into_owned();

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || worker(cmd_rx, evt_tx, ctx, data_dir));

        Self {
            cmd_tx,
            evt_rx,
            cameras: Vec::new(),
            selected: None,
            snapshots: Vec::new(),
            label: String::new(),
            status: "Click Discover to find cameras.".into(),
            busy: false,
            settings,
            prefs_open: false,
            prefs_data_dir,
        }
    }

    fn send(&mut self, cmd: Cmd) {
        self.busy = true;
        let _ = self.cmd_tx.send(cmd);
    }

    fn poll(&mut self) {
        while let Ok(evt) = self.evt_rx.try_recv() {
            self.busy = false;
            match evt {
                Evt::Cameras(cams) => {
                    self.status = format!("Found {} camera(s).", cams.len());
                    self.cameras = cams;
                    self.selected = (!self.cameras.is_empty()).then_some(0);
                    self.snapshots.clear();
                    if let Some(i) = self.selected {
                        let serial = self.cameras[i].serial.clone();
                        self.send(Cmd::ListSnapshots { serial });
                    }
                }
                Evt::SnapshotDone(filename) => {
                    self.status = format!("Saved: {filename}");
                    if let Some(i) = self.selected {
                        let serial = self.cameras[i].serial.clone();
                        self.send(Cmd::ListSnapshots { serial });
                    }
                }
                Evt::Snapshots(snaps) => {
                    self.snapshots = snaps;
                }
                Evt::ReferenceDone => {
                    self.status = "Reference set.".into();
                    if let Some(i) = self.selected {
                        let serial = self.cameras[i].serial.clone();
                        self.send(Cmd::ListSnapshots { serial });
                    }
                }
                Evt::ExportDone { dest, count } => {
                    self.status = format!("Exported {count} file(s) → {}", dest.display());
                }
                Evt::ImportDone { snapshots, references } => {
                    self.status =
                        format!("Imported {snapshots} snapshot(s), {references} reference(s).");
                    if let Some(i) = self.selected {
                        let serial = self.cameras[i].serial.clone();
                        self.send(Cmd::ListSnapshots { serial });
                    }
                }
                Evt::Err(msg) => {
                    self.status = format!("Error: {msg}");
                }
            }
        }
    }
}

impl eframe::App for FleetApp {
    // eframe 0.34 requires ui() for the simplified single-panel path; our
    // update() override handles layout directly so this is never called.
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    if ui.button("⟳ Discover").clicked() {
                        self.send(Cmd::Discover);
                    }
                });
                if ui.button("⚙ Preferences").clicked() {
                    self.prefs_open = !self.prefs_open;
                }
                ui.separator();
                if self.busy {
                    ui.spinner();
                    ui.label("Working…");
                } else {
                    ui.label(&self.status);
                }
            });
        });

        // ── Preferences window ───────────────────────────────────────────
        let mut prefs_open = self.prefs_open;
        let mut save_prefs = false;
        let mut cancel_prefs = false;
        let mut export_trigger = false;
        let mut import_trigger = false;

        egui::Window::new("Preferences")
            .open(&mut prefs_open)
            .resizable(false)
            .collapsible(false)
            .min_width(480.0)
            .show(ctx, |ui| {
                ui.label("Data directory — snapshots and references are stored here:");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.prefs_data_dir)
                            .desired_width(380.0),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.prefs_data_dir = path.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(format!(
                        "Default: {}",
                        default_data_dir().display()
                    ))
                    .small()
                    .weak(),
                );

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                ui.label("Move data to another machine:");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.busy, |ui| {
                        if ui.button("↑  Export all…").on_hover_text(
                            "Pack all snapshots and references into a zip archive"
                        ).clicked() {
                            export_trigger = true;
                        }
                        if ui.button("↓  Import archive…").on_hover_text(
                            "Unpack a previously exported archive into the current data directory"
                        ).clicked() {
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
            let trimmed = self.prefs_data_dir.trim().to_string();
            self.settings.data_dir = (!trimmed.is_empty()).then(|| trimmed);
            self.settings.save();
            let effective = self.settings.effective_data_dir();
            self.prefs_data_dir = effective.to_string_lossy().into_owned();
            let _ = self.cmd_tx.send(Cmd::SetDataDir(effective.clone()));
            if let Some(i) = self.selected {
                let serial = self.cameras[i].serial.clone();
                self.send(Cmd::ListSnapshots { serial });
            }
            self.status = format!("Data dir: {}", effective.display());
            prefs_open = false;
        }
        if cancel_prefs {
            self.prefs_data_dir =
                self.settings.effective_data_dir().to_string_lossy().into_owned();
            prefs_open = false;
        }
        self.prefs_open = prefs_open;

        // File dialogs for export/import — open native dialogs, then dispatch
        // to the worker so the UI shows a spinner during the zip operation.
        if export_trigger {
            if let Some(dest) = rfd::FileDialog::new()
                .set_file_name("fleet-export.zip")
                .add_filter("Zip archive", &["zip"])
                .save_file()
            {
                self.send(Cmd::Export {
                    dest,
                });
            }
        }
        if import_trigger {
            if let Some(src) = rfd::FileDialog::new()
                .add_filter("Zip archive", &["zip"])
                .pick_file()
            {
                self.send(Cmd::Import { src });
            }
        }

        // ── Camera sidebar ───────────────────────────────────────────────
        let mut cam_select: Option<usize> = None;
        egui::SidePanel::left("cameras").min_width(180.0).show(ctx, |ui| {
            ui.heading("Cameras");
            ui.separator();
            if self.cameras.is_empty() {
                ui.label(egui::RichText::new("No cameras").italics().weak());
            }
            for (i, cam) in self.cameras.iter().enumerate() {
                let text = format!("{}\n{}", cam.model, cam.serial);
                if ui.selectable_label(self.selected == Some(i), text).clicked() {
                    cam_select = Some(i);
                }
            }
        });
        if let Some(i) = cam_select {
            self.selected = Some(i);
            let serial = self.cameras[i].serial.clone();
            self.send(Cmd::ListSnapshots { serial });
        }

        // ── Main panel ───────────────────────────────────────────────────
        let mut snap_trigger = false;
        let mut ref_action: Option<String> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(sel) = self.selected else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("Select a camera to see snapshots")
                            .italics()
                            .weak(),
                    );
                });
                return;
            };

            let cam = &self.cameras[sel];
            ui.horizontal(|ui| {
                ui.heading(&cam.model);
                ui.label(
                    egui::RichText::new(format!("{}  fw {}", cam.serial, cam.firmware)).weak(),
                );
            });
            ui.separator();

            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    ui.label("Label:");
                    ui.text_edit_singleline(&mut self.label);
                    if ui.button("Take Snapshot").clicked() {
                        snap_trigger = true;
                    }
                });
            });
            ui.separator();

            let snaps = self.snapshots.clone();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for snap in &snaps {
                    ui.horizontal(|ui| {
                        let ts = snap.captured_at.get(..19).unwrap_or(&snap.captured_at);
                        let lbl = snap.label.as_deref().unwrap_or("(no label)");
                        ui.label(format!("{ts}  {lbl}"));
                        if snap.is_reference {
                            ui.label(
                                egui::RichText::new("◀ ref")
                                    .color(egui::Color32::LIGHT_GREEN),
                            );
                        } else {
                            ui.add_enabled_ui(!self.busy, |ui| {
                                if ui.small_button("Set ref").clicked() {
                                    ref_action = Some(snap.filename.clone());
                                }
                            });
                        }
                    });
                }
            });
        });

        if snap_trigger {
            if let Some(sel) = self.selected {
                let serial = self.cameras[sel].serial.clone();
                let label = self.label.trim().to_string();
                self.send(Cmd::Snapshot { serial, label });
            }
        }
        if let Some(filename) = ref_action {
            self.send(Cmd::SetReference { filename });
        }
    }
}

// ── Worker thread ─────────────────────────────────────────────────────────

fn worker(
    rx: mpsc::Receiver<Cmd>,
    tx: mpsc::Sender<Evt>,
    ctx: egui::Context,
    initial_data_dir: PathBuf,
) {
    let mut data_dir = initial_data_dir;
    // Parsed once for the worker's lifetime: MaidLayer.config is a
    // multi-hundred-thousand-line file whose content never changes for a
    // running process, so re-parsing it on every "Take Snapshot" click (the
    // prior behavior) was pure redundant work inside a UI-triggered path.
    let schema = MaidLayerConfig::parse_file(Path::new(SCHEMA_PATH));
    for cmd in rx {
        let opt_evt: Option<Result<Evt, String>> = match cmd {
            Cmd::SetDataDir(dir) => {
                data_dir = dir;
                None
            }
            Cmd::Discover => Some(do_discover()),
            Cmd::Snapshot { serial, label } => Some(match &schema {
                Ok(s) => do_snapshot(s, &data_dir, &serial, &label),
                Err(e) => Err(e.to_string()),
            }),
            Cmd::ListSnapshots { serial } => {
                Some(Ok(Evt::Snapshots(list_snapshots(&data_dir, &serial))))
            }
            Cmd::SetReference { filename } => Some(set_reference(&data_dir, &filename)),
            Cmd::Export { dest } => Some(do_export(&data_dir, &dest)),
            Cmd::Import { src } => Some(do_import(&data_dir, &src)),
        };
        if let Some(result) = opt_evt {
            let _ = tx.send(result.unwrap_or_else(Evt::Err));
            ctx.request_repaint();
        }
    }
}

// ── Camera operations ─────────────────────────────────────────────────────

/// See src/main.rs's identical helper: `pair_devices` disambiguates
/// same-model bodies purely by enumeration order, with no per-unit
/// identifier to cross-check. Warn loudly whenever that's ambiguous rather
/// than silently risking a swapped serial/firmware assignment.
fn warn_if_ambiguous_pairing(enriched: &[(DeviceInfo, Option<UsbCameraInfo>)]) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (dev, _) in enriched {
        *counts.entry(dev.name.as_str()).or_insert(0) += 1;
    }
    for (model, count) in counts {
        if count > 1 {
            eprintln!(
                "warning: {count} \"{model}\" bodies connected — serial/firmware assignment \
                 is based on SDK/USB enumeration order, not a per-unit identifier, and could \
                 be swapped between them."
            );
        }
    }
}

fn do_discover() -> Result<Evt, String> {
    let path = Path::new(SDK_PATH);
    if !path.exists() {
        return Err(format!(
            "SDK not found: {}\nRun scripts/setup-sdk-runtime.sh first.",
            path.display()
        ));
    }
    let mut sdk = Sdk::open(path).map_err(|e| e.to_string())?;
    // Skip USB reset in the GUI: resetting cameras from a background thread
    // fires USB disconnect/reconnect events through the main AppKit run loop,
    // which invalidates the Metal drawing surface mid-frame and causes a crash.
    // The AppKit run loop is already running (via NSApplication), so the SDK's
    // IOServiceAddMatchingNotification delivers already-connected cameras via
    // its initial iterator without needing a reset.
    sdk.initialize_no_usb_reset().map_err(|e| e.to_string())?;
    let devices = sdk.devices().map_err(|e| e.to_string())?;
    let usb = usb_camera_list();
    let paired = pair_devices(devices, &usb);
    warn_if_ambiguous_pairing(&paired);
    let rows = paired
        .into_iter()
        .map(|(dev, usb_opt)| CameraRow {
            model: dev.name.clone(),
            serial: usb_opt
                .as_ref()
                .map(|u| u.serial.clone())
                .unwrap_or_else(|| format!("id-{}", dev.id)),
            firmware: usb_opt
                .as_ref()
                .map(|u| u.firmware.clone())
                .unwrap_or_else(|| dev.version.clone()),
        })
        .collect();
    Ok(Evt::Cameras(rows))
}

fn do_snapshot(schema: &MaidLayerConfig, data_dir: &Path, serial: &str, label: &str) -> Result<Evt, String> {
    let mut sdk = Sdk::open(Path::new(SDK_PATH)).map_err(|e| e.to_string())?;
    sdk.initialize_no_usb_reset().map_err(|e| e.to_string())?;
    let devices = sdk.devices().map_err(|e| e.to_string())?;
    let usb = usb_camera_list();

    // Match the real USB serial, the bare SDK id, or the "id-{id}" fallback
    // form do_discover uses when no USB serial is readable — without the
    // last form, such a camera could never be re-found here (see main.rs's
    // serial_matches, which this mirrors).
    let (dev, usb_opt) = pair_devices(devices, &usb)
        .into_iter()
        .find(|(dev, u)| {
            u.as_ref().map(|u| u.serial.as_str()) == Some(serial)
                || dev.id.to_string() == serial
                || format!("id-{}", dev.id) == serial
        })
        .ok_or_else(|| format!("camera {serial} not found after re-discover"))?;

    let name_map = schema.name_map_for_model(&dev.name);
    let camera = Camera {
        model: dev.name.clone(),
        serial: serial.to_string(),
        firmware: usb_opt
            .as_ref()
            .map(|u| u.firmware.clone())
            .unwrap_or_else(|| dev.version.clone()),
    };

    let device = sdk.connect(dev.id).map_err(|e| e.to_string())?;
    let captured_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| e.to_string())?;

    let mut snap = Snapshot::new(camera, Transport::Usb, captured_at);
    snap.label = (!label.is_empty()).then(|| label.to_string());

    for cap in &device.capabilities {
        if cap.operations & OP_GET == 0 {
            continue;
        }
        if let Ok(val) = device.read_capability(cap.id) {
            let name = name_map
                .get(&cap.id)
                .cloned()
                .unwrap_or_else(|| format!("cap_{:#x}", cap.id));
            snap.insert(name, cap.id, val);
        }
    }

    let dir = data_dir.join("snapshots");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let filename = snap.suggested_filename();
    snap.save_to_file(&dir.join(&filename))
        .map_err(|e| e.to_string())?;
    Ok(Evt::SnapshotDone(filename))
}

fn list_snapshots(data_dir: &Path, serial: &str) -> Vec<SnapRow> {
    let snap_dir = data_dir.join("snapshots");
    let ref_dir = data_dir.join("references");

    let Ok(entries) = std::fs::read_dir(&snap_dir) else {
        return Vec::new();
    };

    // A reference is a byte copy of some snapshot, saved under a different
    // (canonical model+serial) filename — so filename membership never
    // matches the original snapshot's filename. Instead, mirror
    // fleet_gui.py's _load_snapshots: read the reference's own captured_at
    // and flag any snapshot whose captured_at[:19] matches. Resolved lazily
    // per model since list_snapshots only receives a serial.
    let mut ref_captured_at_by_model: HashMap<String, Option<String>> = HashMap::new();

    let mut rows: Vec<SnapRow> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let fname = e.file_name().into_string().ok()?;
            if !fname.ends_with(".json") {
                return None;
            }
            let path = snap_dir.join(&fname);
            let snap = match Snapshot::load_from_file(&path) {
                Ok(s) => s,
                Err(err) => {
                    eprintln!("warning: skipping {}: {err}", path.display());
                    return None;
                }
            };
            if snap.camera.serial != serial {
                return None;
            }
            let model = snap.camera.model.clone();
            let ref_captured_at = ref_captured_at_by_model
                .entry(model.clone())
                .or_insert_with(|| {
                    let ref_path = ref_dir.join(format!("{}_{serial}.json", model_slug(&model)));
                    Snapshot::load_from_file(&ref_path).ok().map(|s| s.captured_at)
                })
                .clone();
            let is_reference = ref_captured_at
                .as_deref()
                .is_some_and(|r| snap.captured_at.get(..19) == r.get(..19));
            Some(SnapRow {
                is_reference,
                filename: fname,
                label: snap.label.clone(),
                captured_at: snap.captured_at.clone(),
            })
        })
        .collect();

    rows.sort_by(|a, b| b.captured_at.cmp(&a.captured_at));
    rows
}

fn set_reference(data_dir: &Path, filename: &str) -> Result<Evt, String> {
    let src = data_dir.join("snapshots").join(filename);
    let snap = Snapshot::load_from_file(&src).map_err(|e| e.to_string())?;

    // Canonical reference filename: {model_slug}_{serial}.json
    // Must match the convention in src/main.rs reference_filename() and
    // gui/fleet_lib.py's model_slug — all three must produce identical paths.
    let canonical = format!("{}_{}.json", model_slug(&snap.camera.model), snap.camera.serial);

    let ref_dir = data_dir.join("references");
    std::fs::create_dir_all(&ref_dir).map_err(|e| e.to_string())?;
    std::fs::copy(&src, ref_dir.join(&canonical)).map_err(|e| e.to_string())?;
    Ok(Evt::ReferenceDone)
}

// ── Pack / unpack ─────────────────────────────────────────────────────────

fn do_export(data_dir: &Path, dest: &Path) -> Result<Evt, String> {
    let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    // firmware.bin is stored uncompressed, matching fleet_gui.py's
    // export_data() — it's an already-dense binary (tens/hundreds of MiB),
    // so Deflate burns CPU for little to no size benefit.
    let stored_opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    let mut count = 0usize;

    // Flat folders: snapshots/*.json and references/*.json
    for folder in ["snapshots", "references"] {
        let dir = data_dir.join(folder);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if !fname_str.ends_with(".json") { continue; }
            let data = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
            zip.start_file(format!("{folder}/{fname_str}"), opts)
                .map_err(|e| e.to_string())?;
            zip.write_all(&data).map_err(|e| e.to_string())?;
            count += 1;
        }
    }

    // Nested firmware: firmware/{slug}/{version}/firmware.bin + metadata.json
    let fw_root = data_dir.join("firmware");
    if let Ok(slug_dirs) = std::fs::read_dir(&fw_root) {
        for slug_entry in slug_dirs.filter_map(|e| e.ok()) {
            if !slug_entry.path().is_dir() { continue; }
            let slug = slug_entry.file_name();
            let slug_str = slug.to_string_lossy();
            let Ok(ver_dirs) = std::fs::read_dir(slug_entry.path()) else { continue };
            for ver_entry in ver_dirs.filter_map(|e| e.ok()) {
                if !ver_entry.path().is_dir() { continue; }
                let ver = ver_entry.file_name();
                let ver_str = ver.to_string_lossy();
                for filename in ["firmware.bin", "metadata.json"] {
                    let fpath = ver_entry.path().join(filename);
                    if !fpath.exists() { continue; }
                    let data = std::fs::read(&fpath).map_err(|e| e.to_string())?;
                    let file_opts = if filename == "firmware.bin" { stored_opts } else { opts };
                    zip.start_file(
                        format!("firmware/{slug_str}/{ver_str}/{filename}"),
                        file_opts,
                    ).map_err(|e| e.to_string())?;
                    zip.write_all(&data).map_err(|e| e.to_string())?;
                    count += 1;
                }
            }
        }
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(Evt::ExportDone { dest: dest.to_path_buf(), count })
}

fn do_import(data_dir: &Path, src: &Path) -> Result<Evt, String> {
    let file = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;

    let mut snap_count = 0usize;
    let mut ref_count = 0usize;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() { continue; }

        // Content filter: only accept known safe paths.
        if !accept_zip_entry(entry.name()) {
            eprintln!("warning: skipping unexpected archive entry: {}", entry.name());
            continue;
        }

        let outpath = safe_join(data_dir, entry.name())
            .ok_or_else(|| format!("unsafe path in archive: {}", entry.name()))?;

        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut outfile = std::fs::File::create(&outpath).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;

        let name = entry.name();
        if name.starts_with("snapshots/") { snap_count += 1; }
        if name.starts_with("references/") { ref_count += 1; }
    }

    Ok(Evt::ImportDone { snapshots: snap_count, references: ref_count })
}

/// Return true if a zip archive entry should be imported.
///
/// Mirrors fleet_lib.accept_zip_entry (Python) — both must stay in sync.
/// Accepted layouts:
///   snapshots/<file>.json        — 2-part, .json only
///   references/<file>.json       — 2-part, .json only
///   firmware/<slug>/<ver>/firmware.bin   — 4-part, exact filename
///   firmware/<slug>/<ver>/metadata.json  — 4-part, exact filename
fn accept_zip_entry(name: &str) -> bool {
    // Every component must be a plain path segment — reject RootDir/Prefix/
    // ParentDir/CurDir outright instead of silently dropping them, so this
    // function is self-contained and doesn't rely on safe_join as a second
    // gate to reject e.g. absolute paths. (A prior version filter_map'd to
    // Normal components only, which meant "/snapshots/foo.json" passed this
    // check — only caught later by safe_join. Mirrors fleet_lib.py, where
    // Path(name).parts keeps the root as a literal part and so already
    // rejects it here.)
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(s) => match s.to_str() {
                Some(s) => parts.push(s),
                None => return false,
            },
            _ => return false,
        }
    }
    if parts.is_empty() { return false; }
    match parts[0] {
        "snapshots" | "references" => parts.len() == 2 && parts[1].ends_with(".json"),
        "firmware" => {
            parts.len() == 4 && (parts[3] == "firmware.bin" || parts[3] == "metadata.json")
        }
        _ => false,
    }
}

// Guard against zip-slip: walk path components and reject anything that
// could escape the base directory (.. or absolute paths).
fn safe_join(base: &Path, untrusted: &str) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for component in Path::new(untrusted).components() {
        match component {
            Component::Normal(c) => path.push(c),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(path)
}


// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 520.0])
            .with_title("Fleet — Nikon Camera Manager"),
        ..Default::default()
    };
    eframe::run_native(
        "fleet-gui",
        opts,
        Box::new(|cc| Ok(Box::new(FleetApp::new(cc)))),
    )
    .unwrap();
}

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── accept_zip_entry ───────────────────────────────────────────────────
    // Mirrors gui/test_fleet_lib.py::TestAcceptZipEntry — same cases, same
    // shape, so the two implementations can't silently drift apart.

    #[test]
    fn accept_zip_entry_snapshot_json() {
        assert!(accept_zip_entry("snapshots/foo.json"));
    }

    #[test]
    fn accept_zip_entry_reference_json() {
        assert!(accept_zip_entry("references/bar.json"));
    }

    #[test]
    fn accept_zip_entry_firmware_bin() {
        assert!(accept_zip_entry("firmware/Z_9/5.31/firmware.bin"));
    }

    #[test]
    fn accept_zip_entry_firmware_metadata_json() {
        assert!(accept_zip_entry("firmware/Z_9/5.31/metadata.json"));
    }

    #[test]
    fn accept_zip_entry_firmware_z6iii() {
        assert!(accept_zip_entry("firmware/Z6_3/2.00/firmware.bin"));
    }

    #[test]
    fn accept_zip_entry_snapshot_bin_rejected() {
        assert!(!accept_zip_entry("snapshots/foo.bin"));
    }

    #[test]
    fn accept_zip_entry_firmware_arbitrary_json_rejected() {
        assert!(!accept_zip_entry("firmware/Z_9/5.31/other.json"));
    }

    #[test]
    fn accept_zip_entry_firmware_flat_bin_rejected() {
        // Old flat layout (2-part) is no longer accepted.
        assert!(!accept_zip_entry("firmware/Z_9_0531.bin"));
    }

    #[test]
    fn accept_zip_entry_reference_bin_rejected() {
        assert!(!accept_zip_entry("references/bar.bin"));
    }

    #[test]
    fn accept_zip_entry_unknown_folder_rejected() {
        assert!(!accept_zip_entry("other/foo.json"));
    }

    #[test]
    fn accept_zip_entry_directory_entry_rejected() {
        // Trailing slash → a single "snapshots" component, not 2 parts.
        assert!(!accept_zip_entry("snapshots/"));
    }

    #[test]
    fn accept_zip_entry_too_deep_rejected() {
        assert!(!accept_zip_entry("snapshots/subdir/foo.json"));
    }

    #[test]
    fn accept_zip_entry_firmware_too_shallow_rejected() {
        // 3-part firmware path (missing version level).
        assert!(!accept_zip_entry("firmware/Z_9/firmware.bin"));
    }

    #[test]
    fn accept_zip_entry_firmware_too_deep_rejected() {
        // 5-part firmware path.
        assert!(!accept_zip_entry("firmware/Z_9/5.31/extra/firmware.bin"));
    }

    #[test]
    fn accept_zip_entry_dotdot_in_firmware_path_rejected() {
        assert!(!accept_zip_entry("firmware/../snapshots/metadata.json"));
    }

    #[test]
    fn accept_zip_entry_dotdot_in_snapshots_path_rejected() {
        assert!(!accept_zip_entry("snapshots/../etc/passwd"));
    }

    #[test]
    fn accept_zip_entry_dotdot_as_slug_component_rejected() {
        assert!(!accept_zip_entry("firmware/../Z_9/5.31/firmware.bin"));
    }

    #[test]
    fn accept_zip_entry_bare_filename_rejected() {
        assert!(!accept_zip_entry("foo.json"));
    }

    #[test]
    fn accept_zip_entry_path_traversal_rejected() {
        assert!(!accept_zip_entry("../etc/passwd"));
    }

    #[test]
    fn accept_zip_entry_absolute_path_rejected() {
        // Regression test: a prior version filter_map'd path components down
        // to Normal only, silently dropping RootDir — so this returned true
        // and only safe_join's separate check kept it from escaping the
        // data dir. accept_zip_entry must reject it on its own, matching
        // fleet_lib.py's accept_zip_entry (Path(name).parts keeps the root
        // as a literal part, so folder == "/" fails the match there).
        assert!(!accept_zip_entry("/snapshots/foo.json"));
    }

    // ── safe_join ──────────────────────────────────────────────────────────

    #[test]
    fn safe_join_normal_path_joins() {
        let base = Path::new("/data");
        assert_eq!(
            safe_join(base, "snapshots/foo.json"),
            Some(base.join("snapshots").join("foo.json"))
        );
    }

    #[test]
    fn safe_join_rejects_dotdot() {
        let base = Path::new("/data");
        assert_eq!(safe_join(base, "../etc/passwd"), None);
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let base = Path::new("/data");
        assert_eq!(safe_join(base, "/etc/passwd"), None);
    }

    #[test]
    fn safe_join_skips_curdir() {
        let base = Path::new("/data");
        assert_eq!(
            safe_join(base, "./snapshots/./foo.json"),
            Some(base.join("snapshots").join("foo.json"))
        );
    }

    // name_map_for_model's tests now live once, canonically, in
    // src/maid_layer.rs (it moved there — see MaidLayerConfig::name_map_for_model).

    // ── list_snapshots / set_reference ──────────────────────────────────────

    fn write_snapshot(dir: &Path, filename: &str, model: &str, serial: &str, captured_at: &str) {
        let mut s = Snapshot::new(
            Camera { model: model.into(), serial: serial.into(), firmware: "5.00".into() },
            Transport::Usb,
            captured_at.into(),
        );
        s.label = Some("test".into());
        s.save_to_file(dir.join(filename)).unwrap();
    }

    #[test]
    fn list_snapshots_missing_dir_returns_empty() {
        let data_dir = TempDir::new().unwrap();
        assert!(list_snapshots(data_dir.path(), "ABC123").is_empty());
    }

    #[test]
    fn list_snapshots_filters_by_serial() {
        let data_dir = TempDir::new().unwrap();
        let snap_dir = data_dir.path().join("snapshots");
        fs::create_dir_all(&snap_dir).unwrap();
        write_snapshot(&snap_dir, "a.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        write_snapshot(&snap_dir, "b.json", "Z 9", "OTHER456", "2026-01-01T00:00:00Z");

        let rows = list_snapshots(data_dir.path(), "ABC123");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "a.json");
    }

    #[test]
    fn list_snapshots_sorts_newest_first() {
        let data_dir = TempDir::new().unwrap();
        let snap_dir = data_dir.path().join("snapshots");
        fs::create_dir_all(&snap_dir).unwrap();
        write_snapshot(&snap_dir, "old.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        write_snapshot(&snap_dir, "new.json", "Z 9", "ABC123", "2026-06-01T00:00:00Z");

        let rows = list_snapshots(data_dir.path(), "ABC123");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].filename, "new.json");
        assert_eq!(rows[1].filename, "old.json");
    }

    #[test]
    fn list_snapshots_skips_unparseable_json_without_panicking() {
        let data_dir = TempDir::new().unwrap();
        let snap_dir = data_dir.path().join("snapshots");
        fs::create_dir_all(&snap_dir).unwrap();
        write_snapshot(&snap_dir, "good.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        fs::write(snap_dir.join("bad.json"), "not valid json").unwrap();

        let rows = list_snapshots(data_dir.path(), "ABC123");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "good.json");
    }

    #[test]
    fn set_reference_uses_canonical_model_underscore_filename() {
        let data_dir = TempDir::new().unwrap();
        let snap_dir = data_dir.path().join("snapshots");
        fs::create_dir_all(&snap_dir).unwrap();
        write_snapshot(&snap_dir, "orig.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");

        set_reference(data_dir.path(), "orig.json").unwrap();

        let ref_path = data_dir.path().join("references").join("Z_9_ABC123.json");
        assert!(ref_path.exists());
        assert_eq!(
            fs::read_to_string(&ref_path).unwrap(),
            fs::read_to_string(snap_dir.join("orig.json")).unwrap(),
        );
    }

    #[test]
    fn list_snapshots_marks_reference_by_captured_at() {
        // Regression test: is_reference used to compare the snapshot's own
        // filename against filenames present in references/ — but
        // set_reference copies the file under a *different* canonical name,
        // so that comparison could never match. Fixed to compare
        // captured_at (mirrors fleet_gui.py's _load_snapshots), which this
        // test pins.
        let data_dir = TempDir::new().unwrap();
        let snap_dir = data_dir.path().join("snapshots");
        fs::create_dir_all(&snap_dir).unwrap();
        write_snapshot(&snap_dir, "ref_candidate.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z");
        write_snapshot(&snap_dir, "other.json", "Z 9", "ABC123", "2026-06-01T00:00:00Z");

        set_reference(data_dir.path(), "ref_candidate.json").unwrap();

        let rows = list_snapshots(data_dir.path(), "ABC123");
        let by_name = |n: &str| rows.iter().find(|r| r.filename == n).unwrap();
        assert!(by_name("ref_candidate.json").is_reference);
        assert!(!by_name("other.json").is_reference);
    }

    // ── do_export / do_import ───────────────────────────────────────────────

    #[test]
    fn export_import_round_trip_preserves_files() {
        let src_dir = TempDir::new().unwrap();
        fs::create_dir_all(src_dir.path().join("snapshots")).unwrap();
        fs::create_dir_all(src_dir.path().join("references")).unwrap();
        write_snapshot(
            &src_dir.path().join("snapshots"), "a.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z",
        );
        write_snapshot(
            &src_dir.path().join("references"), "Z_9_ABC123.json", "Z 9", "ABC123", "2026-01-01T00:00:00Z",
        );
        let fw_dir = src_dir.path().join("firmware").join("Z_9").join("5.31");
        fs::create_dir_all(&fw_dir).unwrap();
        fs::write(fw_dir.join("firmware.bin"), b"fake-firmware-bytes").unwrap();
        fs::write(fw_dir.join("metadata.json"), b"{}").unwrap();

        let zip_path = src_dir.path().join("export.zip");
        let export = do_export(src_dir.path(), &zip_path).unwrap();
        match export {
            Evt::ExportDone { count, .. } => assert_eq!(count, 4),
            _ => panic!("expected ExportDone"),
        }

        let dst_dir = TempDir::new().unwrap();
        let import = do_import(dst_dir.path(), &zip_path).unwrap();
        match import {
            Evt::ImportDone { snapshots, references } => {
                assert_eq!(snapshots, 1);
                assert_eq!(references, 1);
            }
            _ => panic!("expected ImportDone"),
        }

        assert_eq!(
            fs::read(dst_dir.path().join("firmware/Z_9/5.31/firmware.bin")).unwrap(),
            b"fake-firmware-bytes",
        );
        assert_eq!(
            fs::read_to_string(dst_dir.path().join("snapshots/a.json")).unwrap(),
            fs::read_to_string(src_dir.path().join("snapshots/a.json")).unwrap(),
        );
    }

    #[test]
    fn import_rejects_path_traversal_entries_without_escaping_data_dir() {
        let dst_dir = TempDir::new().unwrap();
        let zip_path = dst_dir.path().join("malicious.zip");

        // Craft a zip with one legitimate entry and several traversal/absolute
        // attempts. ZipWriter doesn't validate names, so this reaches
        // do_import exactly as an attacker-controlled archive would.
        let file = fs::File::create(&zip_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        zip.start_file("snapshots/good.json", opts).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.start_file("../evil_sibling.txt", opts).unwrap();
        zip.write_all(b"escaped").unwrap();
        zip.start_file("/tmp/evil_absolute.txt", opts).unwrap();
        zip.write_all(b"escaped").unwrap();
        zip.finish().unwrap();

        // "good.json" isn't a valid Snapshot, but do_import only counts by
        // path prefix — it doesn't parse contents — so this still exercises
        // the filter without needing a real snapshot body.
        let result = do_import(dst_dir.path(), &zip_path);
        assert!(result.is_ok());

        assert!(!dst_dir.path().join("snapshots/good.json").parent().unwrap()
            .parent().unwrap().join("evil_sibling.txt").exists());
        assert!(!Path::new("/tmp/evil_absolute.txt").exists());
    }
}
