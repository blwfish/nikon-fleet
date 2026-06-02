use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui;
use nikon_fleet::maid_layer::MaidLayerConfig;
use nikon_fleet::sdk::{Sdk, OP_GET, pair_devices, usb_camera_list};
use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

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

// ── App data directory ────────────────────────────────────────────────────

fn app_data_dir() -> PathBuf {
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
}

enum Evt {
    Cameras(Vec<CameraRow>),
    SnapshotDone(String),
    Snapshots(Vec<SnapRow>),
    ReferenceDone,
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
}

impl FleetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || worker(cmd_rx, evt_tx, ctx));
        Self {
            cmd_tx,
            evt_rx,
            cameras: Vec::new(),
            selected: None,
            snapshots: Vec::new(),
            label: String::new(),
            status: "Click Discover to find cameras.".into(),
            busy: false,
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
                Evt::Err(msg) => {
                    self.status = format!("Error: {msg}");
                }
            }
        }
    }
}

impl eframe::App for FleetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    if ui.button("⟳ Discover").clicked() {
                        self.send(Cmd::Discover);
                    }
                });
                ui.separator();
                if self.busy {
                    ui.spinner();
                    ui.label("Working…");
                } else {
                    ui.label(&self.status);
                }
            });
        });

        // Collect camera selection intent — applied after the closure
        // so we can mutably borrow self.
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

        // Collect actions from main panel — applied after the closure.
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

            // Snapshot list — clone to avoid borrow conflict with self.send below.
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

fn worker(rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Evt>, ctx: egui::Context) {
    let data_dir = app_data_dir();
    for cmd in rx {
        let evt = match cmd {
            Cmd::Discover => do_discover(),
            Cmd::Snapshot { serial, label } => do_snapshot(&data_dir, &serial, &label),
            Cmd::ListSnapshots { serial } => Ok(Evt::Snapshots(list_snapshots(&data_dir, &serial))),
            Cmd::SetReference { filename } => set_reference(&data_dir, &filename),
        };
        let _ = tx.send(evt.unwrap_or_else(Evt::Err));
        ctx.request_repaint();
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
    sdk.initialize().map_err(|e| e.to_string())?;
    let devices = sdk.devices().map_err(|e| e.to_string())?;
    let usb = usb_camera_list();
    let rows = pair_devices(devices, &usb)
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

fn do_snapshot(data_dir: &Path, serial: &str, label: &str) -> Result<Evt, String> {
    let sdk_path = Path::new(SDK_PATH);
    let schema = MaidLayerConfig::parse_file(Path::new(SCHEMA_PATH))
        .map_err(|e| e.to_string())?;

    let mut sdk = Sdk::open(sdk_path).map_err(|e| e.to_string())?;
    sdk.initialize().map_err(|e| e.to_string())?;
    let devices = sdk.devices().map_err(|e| e.to_string())?;
    let usb = usb_camera_list();

    let (dev, usb_opt) = pair_devices(devices, &usb)
        .into_iter()
        .find(|(_, u)| u.as_ref().map(|u| u.serial.as_str()) == Some(serial))
        .ok_or_else(|| format!("camera {serial} not found after re-discover"))?;

    let name_map = name_map_for_model(&schema, &dev.name);
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

    let refs: HashSet<String> = std::fs::read_dir(&ref_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();

    let Ok(entries) = std::fs::read_dir(&snap_dir) else {
        return Vec::new();
    };

    let mut rows: Vec<SnapRow> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let fname = e.file_name().into_string().ok()?;
            if !fname.ends_with(".json") {
                return None;
            }
            let snap = Snapshot::load_from_file(&snap_dir.join(&fname)).ok()?;
            if snap.camera.serial != serial {
                return None;
            }
            Some(SnapRow {
                is_reference: refs.contains(&fname),
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
    let ref_dir = data_dir.join("references");
    std::fs::create_dir_all(&ref_dir).map_err(|e| e.to_string())?;
    std::fs::copy(&src, ref_dir.join(filename)).map_err(|e| e.to_string())?;
    Ok(Evt::ReferenceDone)
}

fn name_map_for_model(schema: &MaidLayerConfig, model: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for section in schema.sections_for_model(model) {
        for cap in &section.capabilities {
            map.entry(cap.code).or_insert_with(|| cap.name.clone());
        }
    }
    map
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
