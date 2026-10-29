// ── Check a body against its reference ────────────────────────────────────
//
// `nikon-fleet check --json`: read the connected body, compare it with the reference snapshot set
// for that body, list what differs. Read-only: nothing is written to the camera or to the data
// directory. Run on the worker like every camera operation, so it never overlaps a capture or an
// apply.
//
// A comparison is only as good as the capture: a body that did not return a capability the model
// is known to have would show it as removed, which is a lie about the camera. The CLI therefore
// refuses an incomplete capture unless it is told to compare anyway (`--allow-partial`). This
// window never passes that on its own: it is a checkbox, off, with the consequence beside it, and
// the result says when the comparison was made that way.

use super::*;

/// What a check is asked for. A result is good for this request only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckRequest {
    pub target: CameraRow,
    /// Compare even if the capture is missing capabilities the model's baseline reads.
    pub allow_partial: bool,
}

// The CLI's JSON, as `ops::check::CheckReport` serialises it.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct FailedReadJson {
    pub(crate) code: u32,
    pub(crate) name: String,
    pub(crate) error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct CheckStatsJson {
    pub(crate) read_ok: usize,
    pub(crate) read_err: usize,
    pub(crate) skipped_no_get: usize,
    pub(crate) unknown_schema: usize,
    pub(crate) failed: Vec<FailedReadJson>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ChangeJson {
    pub(crate) name: String,
    pub(crate) code: u32,
    /// The reference's value.
    pub(crate) value_a: serde_json::Value,
    /// The live value.
    pub(crate) value_b: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct DiffJson {
    pub(crate) changed: Vec<ChangeJson>,
    /// In the reference, not readable on the body now.
    pub(crate) only_in_a: Vec<String>,
    /// Readable on the body, not in the reference.
    pub(crate) only_in_b: Vec<String>,
    /// Present when the two were taken on different firmware: which differences the schema explains.
    pub(crate) firmware_annotation: Option<FirmwareAnnotationJson>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct FirmwareAnnotationJson {
    pub(crate) firmware_a: String,
    pub(crate) firmware_b: String,
    pub(crate) schema_only_in_a: Vec<String>,
    pub(crate) schema_only_in_b: Vec<String>,
    pub(crate) schema_changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct CheckJson {
    pub(crate) camera: BodyJson,
    pub(crate) reference_label: Option<String>,
    pub(crate) reference_captured_at: String,
    pub(crate) gate: GateJson,
    pub(crate) stats: CheckStatsJson,
    pub(crate) diff: DiffJson,
    pub(crate) differences: usize,
}

/// One property that differs, with the names and values a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRow {
    pub(crate) name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) code: u32,
    pub(crate) reference: String,
    pub(crate) live: String,
    /// Either side is missing or malformed rather than a value.
    pub(crate) problem: bool,
}

impl DiffRow {
    pub(crate) fn short_name(&self) -> &str {
        short_name(&self.name)
    }

    /// The display name when it says something the name does not.
    pub(crate) fn extra_display_name(&self) -> Option<&str> {
        self.display_name.as_deref().filter(|d| *d != self.short_name() && *d != self.name)
    }
}

/// A check result, ready to draw: the report and its differences rendered with the SDK's names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckView {
    pub(crate) report: CheckJson,
    pub(crate) rows: Vec<DiffRow>,
    /// Why some names or values are shown raw (no `sdk-runtime/`, an unreadable labels file).
    pub(crate) labels_note: Option<String>,
    /// The comparison was asked for with `--allow-partial`.
    pub(crate) allow_partial: bool,
}

/// Render the report's changed properties with `labels`. Pure.
pub(crate) fn check_view(report: CheckJson, labels: &Labels, allow_partial: bool) -> CheckView {
    let opts = render::RenderOptions::viewer(labels);
    let rows = report
        .diff
        .changed
        .iter()
        .map(|c| {
            let reference = render::render_value_opts(&opts, c.code, Some(&c.name), &c.value_a);
            let live = render::render_value_opts(&opts, c.code, Some(&c.name), &c.value_b);
            DiffRow {
                name: c.name.clone(),
                display_name: labels.capability_display_name(c.code).map(str::to_string),
                code: c.code,
                problem: reference.is_problem() || live.is_problem(),
                reference: reference.display,
                live: live.display,
            }
        })
        .collect();
    CheckView { rows, labels_note: labels.note(), allow_partial, report }
}

/// What the check came to, as one line with a level. Exhaustive over the gate's closed vocabulary.
pub(crate) fn check_headline(view: &CheckView) -> (Level, String) {
    let r = &view.report;
    let n = r.differences;
    let things = |n: usize| if n == 1 { "1 difference".to_string() } else { format!("{n} differences") };
    match (r.gate.outcome, r.gate.reason) {
        (GateKind::Failed, _) => {
            let missing = r.gate.missing.as_ref().map_or(0, Vec::len);
            (
                Level::Warn,
                format!(
                    "INCOMPLETE capture: {missing} capabilit{} the model should return {} not read, so {} below. \
                     {} listed as \"in the reference, not readable now\" {} NOT evidence that a setting changed.",
                    if missing == 1 { "y" } else { "ies" },
                    if missing == 1 { "was" } else { "were" },
                    things(n),
                    if missing == 1 { "That one is" } else { "Those are" },
                    if missing == 1 { "is" } else { "are" },
                ),
            )
        }
        (GateKind::NoBaseline, reason) => {
            let why = match reason {
                Some(NoBaselineKind::UnknownModel) => format!("there is no baseline for {}", r.camera.model),
                Some(NoBaselineKind::FirmwareChanged) => format!(
                    "firmware {} differs from the baseline's {}",
                    r.gate.capture.as_deref().unwrap_or("?"),
                    r.gate.baseline.as_deref().unwrap_or("?")
                ),
                None => "no reason was given".to_string(),
            };
            let verdict = if n == 0 { "No differences found".to_string() } else { format!("{} from the reference", things(n)) };
            (Level::Warn, format!("{verdict}, but the capture could not be judged complete ({why}): a property the body did not return would be missing from the comparison."))
        }
        (GateKind::Passed, _) if n == 0 => (Level::Info, "Matches the reference: no differences.".to_string()),
        (GateKind::Passed, _) => (Level::Warn, format!("{} from the reference.", things(n))),
    }
}

/// How the capture went, in words that do not mistake an expected gap for a fault.
pub(crate) fn check_stats_line(s: &CheckStatsJson) -> String {
    format!(
        "{} properties read; {} could not be read (a healthy capture has some: the completeness check above, not this count, is the verdict); {} without a read operation; {} not in the schema.",
        s.read_ok, s.read_err, s.skipped_no_get, s.unknown_schema
    )
}

/// The CLI arguments. `--allow-partial` only when the request says so.
pub(crate) fn check_argv(data_dir: &Path, request: &CheckRequest) -> Result<Vec<String>, String> {
    let dir = data_dir
        .to_str()
        .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
    let mut argv = vec![format!("--data-dir={dir}"), "check".into(), "--json".into(), format!("--serial={}", request.target.serial)];
    if request.allow_partial {
        argv.push("--allow-partial".into());
    }
    Ok(argv)
}

pub(crate) fn do_check(data_dir: &Path, request: &CheckRequest) -> Result<CheckView, String> {
    let sdk_runtime = resolve_sdk_runtime(std::env::var_os("NIKON_FLEET_SDK_RUNTIME"));
    let firmware = Some(request.target.firmware.as_str()).filter(|f| !f.is_empty());
    let labels = Labels::load_for(&sdk_runtime, &request.target.model, firmware);
    check_via(&locate_cli()?, data_dir, request, &list_cameras, &labels, CAMERA_CLI_TIMEOUT)
}

/// Run the check. Nothing here writes, so a run that is stopped or cut off is a failed check and
/// the camera is as it was.
pub(crate) fn check_via(
    cli: &Path,
    data_dir: &Path,
    request: &CheckRequest,
    scan: &dyn Fn() -> CameraList,
    labels: &Labels,
    timeout: Duration,
) -> Result<CheckView, String> {
    if let Some(refusal) = snapshot_refusal(&request.target, &scan()) {
        return Err(format!("Check refused, nothing was read: {}", refusal.reason()));
    }
    let argv = check_argv(data_dir, request)?;
    let out = run_cli(cli, &argv, None, timeout).map_err(|e| format!("Check failed: {}", e.message))?;
    let report: CheckJson = serde_json::from_str(out.stdout.trim())
        .map_err(|e| format!("the check command succeeded but its report could not be read ({e})"))?;
    // The comparison is of the body that was asked for, from the reference that body has.
    if report.camera.serial != request.target.serial || report.camera.model != request.target.model {
        return Err(format!(
            "the check is of {} {}, not of the {} {} that was asked for",
            report.camera.model, report.camera.serial, request.target.model, request.target.serial
        ));
    }
    // Every difference the CLI counted is one the report lists: a count that does not match is a
    // report this program does not understand, and it is not shown as a clean or a short list.
    let listed = report.diff.changed.len() + report.diff.only_in_a.len() + report.diff.only_in_b.len();
    if listed != report.differences {
        return Err(format!(
            "the check reports {} differences but lists {listed}; it cannot be shown reliably",
            report.differences
        ));
    }
    Ok(check_view(report, labels, request.allow_partial))
}

/// Where the window is.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum CheckState {
    #[default]
    Idle,
    /// Asked for this request, not answered.
    Running(CheckRequest),
    Done(Box<CheckDone>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckDone {
    pub(crate) request: CheckRequest,
    pub(crate) result: Result<CheckView, String>,
}

#[derive(Debug, Default)]
pub(crate) struct CheckUi {
    pub open: bool,
    pub target_key: Option<(String, String)>,
    /// Off unless the person turns it on, and off again whenever the body changes.
    allow_partial: bool,
    allow_partial_for: Option<CameraRow>,
    state: CheckState,
    pub notice: String,
}

impl CheckUi {
    pub fn state(&self) -> &CheckState {
        &self.state
    }

    pub fn allow_partial(&self) -> bool {
        self.allow_partial
    }

    pub fn set_allow_partial(&mut self, on: bool) {
        self.allow_partial = on;
    }

    pub fn target_camera<'a>(&self, cameras: &'a [CameraRow]) -> Option<&'a CameraRow> {
        let (model, serial) = self.target_key.as_ref()?;
        cameras.iter().find(|c| &c.model == model && &c.serial == serial)
    }

    pub fn request(&self, cameras: &[CameraRow]) -> Option<CheckRequest> {
        Some(CheckRequest { target: self.target_camera(cameras)?.clone(), allow_partial: self.allow_partial })
    }

    /// Run every frame: the opt-in belongs to the body it was ticked for, and a result is shown only
    /// for the request it answers.
    pub fn sync(&mut self, cameras: &[CameraRow]) {
        let body = self.target_camera(cameras).cloned();
        if self.allow_partial_for != body {
            self.allow_partial = false;
            self.allow_partial_for = body;
        }
        let held = match &self.state {
            CheckState::Running(r) => Some(r),
            CheckState::Done(d) => Some(&d.request),
            CheckState::Idle => None,
        };
        if let Some(held) = held {
            if self.request(cameras).as_ref() != Some(held) {
                self.state = CheckState::Idle;
                self.notice = "The result was withdrawn: the camera or the comparison option changed.".to_string();
            }
        }
    }

    pub fn begin(&mut self, cameras: &[CameraRow]) -> Result<Cmd, String> {
        if matches!(self.state, CheckState::Running(_)) {
            return Err("A check is already running.".to_string());
        }
        let request = self.request(cameras).ok_or("Choose a connected body first.")?;
        self.notice.clear();
        self.state = CheckState::Running(request.clone());
        Ok(Cmd::Check { request })
    }

    /// The answer. Dropped unless it is for the check that is running.
    pub fn arrived(&mut self, request: CheckRequest, result: Result<CheckView, String>) {
        if self.state != CheckState::Running(request.clone()) {
            return;
        }
        self.state = CheckState::Done(Box::new(CheckDone { request, result }));
    }

    pub fn discard(&mut self) {
        if matches!(self.state, CheckState::Done(_)) {
            self.state = CheckState::Idle;
        }
    }

    /// The reference lives in the data directory: a result from another is not this one's.
    pub fn data_dir_changed(&mut self) {
        if !matches!(self.state, CheckState::Idle) {
            self.state = CheckState::Idle;
            self.notice = "The result was withdrawn: the data directory changed.".to_string();
        }
    }

    /// The worker is gone: a running check will not be answered. It only read, so nothing changed.
    pub fn worker_lost(&mut self) {
        if matches!(self.state, CheckState::Running(_)) {
            self.state = CheckState::Idle;
            self.notice = "The worker thread stopped while the camera was being read. Nothing was written.".to_string();
        }
    }
}

/// What the listing says about the body's reference, for a note beside the button. The listing marks
/// a snapshot as the reference by byte identity, so "none marked" is not "no reference file".
pub(crate) fn reference_note(listing: Option<&SnapListing>, camera: &CameraRow) -> Option<String> {
    let listing = listing?;
    match listing.rows_for(&camera.model, &camera.serial).find(|r| r.is_reference) {
        Some(r) => Some(format!("Reference: {}", snapshot_label(r))),
        None => Some(
            "No stored snapshot of this body is marked as its reference. The check needs a reference file; set one from the snapshot list (Set ref).".to_string(),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckAction {
    Run,
    Discard,
}

fn draw_check_result(ui: &mut egui::Ui, d: &CheckDone) {
    let t = &d.request.target;
    ui.label(egui::RichText::new(format!("{} {}  fw {}", t.model, t.serial, t.firmware)).weak());
    let view = match &d.result {
        Ok(v) => v,
        Err(e) => {
            colored(ui, Level::Error, "The check did not produce a comparison. The camera was not changed (a check only reads).");
            ui.label(e);
            return;
        }
    };
    let r = &view.report;
    let (level, headline) = check_headline(view);
    ui.heading(if r.differences == 0 && level == Level::Info { "No drift" } else { "Check result" });
    colored(ui, level, headline);
    if view.allow_partial {
        colored(ui, Level::Warn, "Compared with \"compare even if incomplete\" on, as you asked.");
    }
    ui.label(egui::RichText::new(format!(
        "Reference: {}  ({}){}",
        r.reference_label.as_deref().unwrap_or("(no label)"),
        r.reference_captured_at.get(..19).unwrap_or(&r.reference_captured_at),
        match &r.diff.firmware_annotation {
            Some(a) => format!(", taken on firmware {}; the camera runs {}", a.firmware_a, a.firmware_b),
            None => String::new(),
        }
    )).weak());
    ui.label(egui::RichText::new(check_stats_line(&r.stats)).small().weak());
    if let Some(note) = &view.labels_note {
        ui.label(egui::RichText::new(format!("Names: {note}")).small().color(WARN_COLOR));
    }
    ui.separator();
    if !view.rows.is_empty() {
        egui::CollapsingHeader::new(format!("Differs from the reference ({})", view.rows.len())).id_salt("ck_changed").default_open(true).show(ui, |ui| {
            egui::Grid::new("ck_grid").striped(true).show(ui, |ui| {
                ui.label(egui::RichText::new("Property").strong());
                ui.label(egui::RichText::new("Reference").strong());
                ui.label(egui::RichText::new("Camera now").strong());
                ui.end_row();
                for row in &view.rows {
                    let name = match row.extra_display_name() {
                        Some(d) => format!("{}  ({d})", row.short_name()),
                        None => row.short_name().to_string(),
                    };
                    ui.label(egui::RichText::new(name).monospace());
                    let text = |s: &str| {
                        let t = egui::RichText::new(s).monospace();
                        if row.problem { t.color(WARN_COLOR) } else { t }
                    };
                    ui.label(text(&row.reference));
                    ui.label(text(&row.live));
                    ui.end_row();
                }
            });
        });
    }
    if !r.diff.only_in_a.is_empty() {
        egui::CollapsingHeader::new(format!("In the reference, not readable on the camera now ({})", r.diff.only_in_a.len()))
            .id_salt("ck_only_a")
            .default_open(r.gate.outcome == GateKind::Failed)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("A capability the body did not return is not the same as a setting that was removed.").weak());
                ui.label(names_list(&r.diff.only_in_a));
            });
    }
    if !r.diff.only_in_b.is_empty() {
        egui::CollapsingHeader::new(format!("Readable on the camera, not in the reference ({})", r.diff.only_in_b.len()))
            .id_salt("ck_only_b")
            .show(ui, |ui| ui.label(names_list(&r.diff.only_in_b)));
    }
    if let Some(a) = &r.diff.firmware_annotation {
        egui::CollapsingHeader::new(format!("What the schema says about firmware {} → {}", a.firmware_a, a.firmware_b)).id_salt("ck_fw").show(ui, |ui| {
            if !a.schema_only_in_a.is_empty() {
                ui.label(format!("Removed in the newer firmware: {}", names_list(&a.schema_only_in_a)));
            }
            if !a.schema_only_in_b.is_empty() {
                ui.label(format!("Added in the newer firmware: {}", names_list(&a.schema_only_in_b)));
            }
            if !a.schema_changed.is_empty() {
                ui.label(format!("Allowed operations differ: {}", names_list(&a.schema_changed)));
            }
        });
    }
    if !r.stats.failed.is_empty() {
        egui::CollapsingHeader::new(format!("Could not be read ({})", r.stats.failed.len())).id_salt("ck_failed").show(ui, |ui| {
            for f in &r.stats.failed {
                ui.label(format!("{} (0x{:04x}): {}", short_name(&f.name), f.code, f.error));
            }
        });
    }
}

impl FleetApp {
    pub(crate) fn ui_check(&mut self, ctx: &egui::Context) {
        if !self.check.open {
            return;
        }
        let mut open = true;
        let mut action: Option<CheckAction> = None;
        let busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::Window::new("Check a camera against its reference")
            .open(&mut open)
            .default_size([720.0, 560.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let c = &mut self.check;
                let cameras = &self.cameras;
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(720.0).show(ui, |ui| {
                    ui.label(egui::RichText::new("Read the camera and compare it with its reference snapshot. Nothing is written.").weak());
                    if cameras.is_empty() {
                        ui.label(egui::RichText::new("No camera is connected: use Discover first.").italics().weak());
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        let text = c.target_camera(cameras).map_or("choose a body…".to_string(), camera_label);
                        egui::ComboBox::from_id_salt("ck_target").selected_text(text).width(420.0).show_ui(ui, |ui| {
                            for cam in cameras {
                                let picked = c.target_key.as_ref() == Some(&(cam.model.clone(), cam.serial.clone()));
                                if ui.selectable_label(picked, camera_label(cam)).clicked() {
                                    c.target_key = Some((cam.model.clone(), cam.serial.clone()));
                                }
                            }
                        });
                    });
                    if c.target_key.is_some() && c.target_camera(cameras).is_none() {
                        ui.colored_label(WARN_COLOR, "That body is no longer connected.");
                    }
                    let camera = c.target_camera(cameras);
                    if let Some(cam) = camera {
                        if let Some(note) = reference_note(self.listing.as_ref(), cam) {
                            ui.label(egui::RichText::new(note).small().weak());
                        }
                    }
                    let mut partial = c.allow_partial();
                    ui.add_enabled_ui(!busy && camera.is_some(), |ui| {
                        ui.checkbox(&mut partial, "Compare even if the capture is incomplete");
                    });
                    c.set_allow_partial(partial);
                    ui.label(
                        egui::RichText::new(
                            "Off: an incomplete capture is refused, because a property the body did not return would look like a removed setting. On: it is compared anyway and those properties are listed as \"not readable now\".",
                        )
                        .small()
                        .weak(),
                    );
                    let can_run = !busy && data_ready && c.request(cameras).is_some() && !matches!(c.state(), CheckState::Running(_));
                    ui.add_enabled_ui(can_run, |ui| {
                        if ui.button("Check (reads the camera, writes nothing)").clicked() {
                            action = Some(CheckAction::Run);
                        }
                    });
                    if !c.notice.is_empty() {
                        ui.colored_label(WARN_COLOR, &c.notice);
                    }
                    ui.separator();
                    match c.state() {
                        CheckState::Idle => {
                            ui.label(egui::RichText::new("Choose a body and run the check.").weak());
                        }
                        CheckState::Running(r) => {
                            ui.label(format!("Reading {} {}… this takes up to a minute.", r.target.model, r.target.serial));
                        }
                        CheckState::Done(d) => {
                            draw_check_result(ui, d);
                            ui.separator();
                            if ui.button("Close this result").clicked() {
                                action = Some(CheckAction::Discard);
                            }
                        }
                    }
                });
            });
        self.check.open = open;
        match action {
            Some(CheckAction::Run) => match self.check.begin(&self.cameras) {
                Ok(cmd) => self.send_check(cmd),
                Err(e) => self.check.notice = e,
            },
            Some(CheckAction::Discard) => self.check.discard(),
            None => {}
        }
    }

    /// Hand a check to the worker. If the worker is gone the command was not delivered.
    pub(crate) fn send_check(&mut self, cmd: Cmd) {
        if !self.send(cmd) {
            self.check.worker_lost();
        }
    }

    /// Open the window on the camera selected in the sidebar.
    pub(crate) fn start_check_for_selected(&mut self) {
        self.check.open = true;
        if let Some(c) = self.selected_camera() {
            self.check.target_key = Some((c.model.clone(), c.serial.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{argv_of, recording_cli, row, scan_of, test_app, usb};
    use nikon_fleet::diff::{Change, Diff, FirmwareAnnotation};
    use nikon_fleet::ops::baseline::{BaselineCapability, GateOutcome, NoBaselineReason};
    use nikon_fleet::ops::capture::{CaptureStats, FailedRead};
    use nikon_fleet::ops::check::CheckReport;
    use nikon_fleet::ops::labels::Source;
    use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
    use tempfile::TempDir;

    const T: Duration = Duration::from_secs(30);

    fn live() -> Camera {
        Camera { model: "Z 9".into(), serial: "T1".into(), firmware: "5.31".into() }
    }

    fn target() -> CameraRow {
        row("Z 9", "T1", 1)
    }

    fn request(allow_partial: bool) -> CheckRequest {
        CheckRequest { target: target(), allow_partial }
    }

    fn changed(name: &str, code: u32, a: serde_json::Value, b: serde_json::Value) -> Change {
        Change { name: format!("kNkMAIDCapability_{name}"), code, value_a: a, value_b: b }
    }

    fn diff(n_changed: usize, only_a: &[&str], only_b: &[&str]) -> Diff {
        Diff {
            changed: (0..n_changed).map(|i| changed(&format!("P{i}"), 100 + i as u32, serde_json::json!(1), serde_json::json!(2))).collect(),
            only_in_a: only_a.iter().map(|n| format!("kNkMAIDCapability_{n}")).collect(),
            only_in_b: only_b.iter().map(|n| format!("kNkMAIDCapability_{n}")).collect(),
            firmware_annotation: None,
        }
    }

    /// The library's own report, as `check --json` prints it.
    fn report_text(gate: GateOutcome, diff: Diff, stats: CaptureStats) -> String {
        let mut reference = Snapshot::new(live(), Transport::Usb, "2026-10-27T10:00:00Z".into());
        reference.label = Some("studio".into());
        serde_json::to_string_pretty(&CheckReport::new(live(), &reference, gate, stats, diff)).unwrap()
    }

    fn stats() -> CaptureStats {
        CaptureStats {
            read_ok: 196,
            read_err: 1,
            skipped_no_get: 2,
            unknown_schema: 0,
            failed: vec![FailedRead { code: 7, name: "kNkMAIDCapability_Unreadable".into(), error: "-126".into() }],
        }
    }

    fn view_of(gate: GateOutcome, diff: Diff, allow_partial: bool) -> CheckView {
        let report: CheckJson = serde_json::from_str(&report_text(gate, diff, stats())).expect("the library's report parses");
        check_view(report, &Labels::none(), allow_partial)
    }

    fn missing(n: usize) -> GateOutcome {
        GateOutcome::Failed { missing: (0..n).map(|i| BaselineCapability { code: 900 + i as u32, name: format!("kNkMAIDCapability_M{i}") }).collect() }
    }

    // ── the library's report, read ───────────────────────────────────────

    #[test]
    fn every_gate_the_library_can_report_is_read_with_the_differences_it_counted() {
        let gates = [
            GateOutcome::Passed,
            GateOutcome::NoBaseline(NoBaselineReason::UnknownModel),
            GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "5.00".into(), capture: "5.31".into() }),
            missing(2),
        ];
        let mut seen = BTreeSet::new();
        for gate in gates {
            let v = view_of(gate.clone(), diff(2, &["A"], &["B", "C"]), false);
            assert_eq!(v.report.differences, 5, "{gate:?}");
            assert_eq!((v.report.diff.changed.len(), v.report.diff.only_in_a.len(), v.report.diff.only_in_b.len()), (2, 1, 2));
            assert_eq!(v.rows.len(), 2);
            seen.insert(format!("{:?}/{:?}", v.report.gate.outcome, v.report.gate.reason));
        }
        assert_eq!(seen.len(), 4, "four different gates were told apart: {seen:?}");
        let v = view_of(GateOutcome::Passed, diff(0, &[], &[]), false);
        assert_eq!(v.report.stats.failed.len(), 1, "the unreadable properties are carried, not dropped");
        assert_eq!(v.report.reference_label.as_deref(), Some("studio"));
    }

    #[test]
    fn a_firmware_annotation_is_carried() {
        let mut d = diff(1, &[], &[]);
        d.firmware_annotation = Some(FirmwareAnnotation {
            firmware_a: "5.00".into(),
            firmware_b: "5.31".into(),
            schema_only_in_a: vec!["kNkMAIDCapability_Old".into()],
            schema_only_in_b: vec![],
            schema_changed: vec!["kNkMAIDCapability_P0".into()],
        });
        let v = view_of(GateOutcome::Passed, d, false);
        let a = v.report.diff.firmware_annotation.expect("annotation");
        assert_eq!((a.firmware_a.as_str(), a.firmware_b.as_str()), ("5.00", "5.31"));
        assert_eq!((a.schema_only_in_a.len(), a.schema_only_in_b.len(), a.schema_changed.len()), (1, 0, 1));
    }

    // ── what the person is told ──────────────────────────────────────────

    #[test]
    fn the_headline_is_clean_only_when_the_capture_was_judged_complete_and_nothing_differs() {
        let h = |gate, n| check_headline(&view_of(gate, diff(n, &[], &[]), false));
        let (level, text) = h(GateOutcome::Passed, 0);
        assert_eq!(level, Level::Info);
        assert!(text.contains("no differences"), "{text}");
        // one difference is already drift, with the singular
        let (level, text) = h(GateOutcome::Passed, 1);
        assert_eq!(level, Level::Warn);
        assert!(text.starts_with("1 difference "), "{text}");
        assert!(h(GateOutcome::Passed, 2).1.starts_with("2 differences "));
        // "no differences" without a completeness verdict is not "matches the reference"
        for reason in [NoBaselineReason::UnknownModel, NoBaselineReason::FirmwareChanged { baseline: "5.00".into(), capture: "5.31".into() }] {
            let (level, text) = h(GateOutcome::NoBaseline(reason), 0);
            assert_eq!(level, Level::Warn);
            assert!(text.contains("could not be judged complete") && !text.starts_with("Matches"), "{text}");
        }
        let (_, unknown) = h(GateOutcome::NoBaseline(NoBaselineReason::UnknownModel), 0);
        assert!(unknown.contains("no baseline for Z 9"), "{unknown}");
        let (_, changed) = h(GateOutcome::NoBaseline(NoBaselineReason::FirmwareChanged { baseline: "5.00".into(), capture: "5.31".into() }), 0);
        assert!(changed.contains("firmware 5.31 differs from the baseline's 5.00"), "{changed}");
    }

    #[test]
    fn an_incomplete_capture_is_said_to_be_incomplete_and_its_gaps_are_not_called_removals() {
        let (level, one) = check_headline(&view_of(missing(1), diff(0, &["A"], &[]), true));
        assert_eq!(level, Level::Warn);
        assert!(one.starts_with("INCOMPLETE capture: 1 capability the model should return was not read"), "{one}");
        assert!(one.contains("NOT evidence that a setting changed") && one.contains("That one is"), "{one}");
        let (_, three) = check_headline(&view_of(missing(3), diff(0, &["A", "B", "C"], &[]), true));
        assert!(three.contains("3 capabilities") && three.contains("were not read") && three.contains("Those are"), "{three}");
    }

    #[test]
    fn the_stats_line_does_not_call_an_expected_gap_a_fault() {
        let line = check_stats_line(&view_of(GateOutcome::Passed, diff(0, &[], &[]), false).report.stats);
        assert!(line.contains("196 properties read") && line.contains("1 could not be read"), "{line}");
        assert!(line.contains("a healthy capture has some"), "{line}");
    }

    #[test]
    fn differences_are_shown_with_the_names_and_values_the_snapshot_viewer_uses() {
        let schema = "<ProfileVersion>9.0.3000</ProfileVersion>\n<model:Z 9>\n<version>common</version>\n<caplist>\n\
                      <capability:kNkMAIDCapability_P0-100>\n<description>0,0,\"Mode Zero\"</description>\n\
                      <allowedoperation:14></allowedoperation>\n<DeviceCommand>1,0,0,0,0</DeviceCommand>\n</capability>\n\
                      </caplist>\n</model>\n";
        let labels = Labels::from_sources(Some(Source { path: "schema", text: schema }), None, "Z 9", None);
        let report: CheckJson = serde_json::from_str(&report_text(GateOutcome::Passed, diff(2, &[], &[]), stats())).unwrap();
        let v = check_view(report.clone(), &labels, false);
        assert_eq!(v.rows[0].extra_display_name(), Some("Mode Zero"));
        assert_eq!(v.rows[0].short_name(), "P0");
        assert_eq!((v.rows[0].reference.as_str(), v.rows[0].live.as_str()), ("1", "2"));
        assert_eq!(v.rows[1].extra_display_name(), None, "a code the schema does not name shows its property name only");
        // without names the values are the same plain strings
        let raw = check_view(report, &Labels::none(), false);
        assert_eq!(raw.rows[0].extra_display_name(), None);
        assert_eq!(raw.rows[0].reference, "1");
        assert!(raw.labels_note.is_none() || !raw.labels_note.as_ref().unwrap().is_empty());
    }

    #[test]
    fn a_value_that_is_missing_or_malformed_is_flagged_not_shown_as_a_plain_value() {
        let mut d = diff(0, &[], &[]);
        d.changed = vec![
            changed("Odd", 5, serde_json::json!(1), serde_json::json!({"value_index": "x", "values": 3})),
            changed("Plain", 6, serde_json::json!(1), serde_json::json!(2)),
        ];
        let v = view_of(GateOutcome::Passed, d, false);
        assert!(v.rows[0].problem, "{:?}", v.rows[0]);
        assert!(!v.rows[1].problem);
    }

    // ── what is asked of the CLI ─────────────────────────────────────────

    #[test]
    fn allow_partial_is_passed_only_when_it_was_asked_for() {
        let d = Path::new("/data");
        let plain = check_argv(d, &request(false)).unwrap();
        assert!(!plain.iter().any(|a| a.contains("allow-partial")), "{plain:?}");
        assert!(plain.contains(&"--json".to_string()) && plain.contains(&"--serial=T1".to_string()), "{plain:?}");
        assert!(check_argv(d, &request(true)).unwrap().contains(&"--allow-partial".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_check_runs_the_cli_and_returns_its_comparison() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("r.json"), report_text(GateOutcome::Passed, diff(1, &[], &[]), stats())).unwrap();
        let cli = recording_cli(dir.path(), &format!("cat '{}/r.json'", dir.path().display()));
        let v = check_via(&cli, Path::new("/data"), &request(false), &|| scan_of(vec![usb("Z 9", "T1")]), &Labels::none(), T).unwrap();
        assert_eq!(v.report.differences, 1);
        assert!(!v.allow_partial);
        assert!(!argv_of(dir.path()).iter().any(|a| a.contains("allow-partial")));
    }

    #[cfg(unix)]
    #[test]
    fn a_check_that_failed_says_why_and_an_incomplete_capture_is_the_clis_refusal_shown_whole() {
        let dir = TempDir::new().unwrap();
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let refused = recording_cli(dir.path(), "echo 'the live capture is incomplete (2 capabilities in the baseline could not be read)' >&2\nexit 1");
        let e = check_via(&refused, Path::new("/d"), &request(false), &scan, &Labels::none(), T).unwrap_err();
        assert!(e.starts_with("Check failed") && e.contains("incomplete"), "{e}");
        let junk = recording_cli(dir.path(), "echo 'nope'");
        assert!(check_via(&junk, Path::new("/d"), &request(false), &scan, &Labels::none(), T).unwrap_err().contains("could not be read"));
        // a hung run is a failed check, said as such
        let hung = recording_cli(dir.path(), "sleep 5");
        let e = check_via(&hung, Path::new("/d"), &request(false), &scan, &Labels::none(), Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("did not finish"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_report_of_another_body_or_with_a_miscounted_list_is_not_shown() {
        let dir = TempDir::new().unwrap();
        let scan = || scan_of(vec![usb("Z 9", "T1")]);
        let mut other = request(false);
        other.target.serial = "OTHER".into();
        fs::write(dir.path().join("r.json"), report_text(GateOutcome::Passed, diff(1, &[], &[]), stats())).unwrap();
        let cli = recording_cli(dir.path(), &format!("cat '{}/r.json'", dir.path().display()));
        let e = check_via(&cli, Path::new("/d"), &other, &|| scan_of(vec![usb("Z 9", "OTHER")]), &Labels::none(), T).unwrap_err();
        assert!(e.contains("not of the Z 9 OTHER"), "{e}");
        // 2 differences counted, 1 listed
        let mut v: serde_json::Value = serde_json::from_str(&report_text(GateOutcome::Passed, diff(1, &[], &[]), stats())).unwrap();
        v["differences"] = serde_json::json!(2);
        fs::write(dir.path().join("r.json"), v.to_string()).unwrap();
        let e = check_via(&cli, Path::new("/d"), &request(false), &scan, &Labels::none(), T).unwrap_err();
        assert!(e.contains("lists 1"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_body_whose_pairing_cannot_be_trusted_is_not_read() {
        let dir = TempDir::new().unwrap();
        let cli = recording_cli(dir.path(), "exit 0");
        let two = || scan_of(vec![usb("Z 9", "T1"), usb("Z 9", "T2")]);
        assert!(check_via(&cli, Path::new("/d"), &request(false), &two, &Labels::none(), T).unwrap_err().starts_with("Check refused"));
        assert!(!dir.path().join("argv.txt").exists(), "the CLI was never run");
    }

    // ── the window ───────────────────────────────────────────────────────

    fn ui_ready() -> (CheckUi, Vec<CameraRow>) {
        (CheckUi { open: true, target_key: Some(("Z 9".into(), "T1".into())), ..CheckUi::default() }, vec![target()])
    }

    fn done_ui() -> (CheckUi, Vec<CameraRow>) {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Check { request } = ui.begin(&cameras).unwrap() else { panic!() };
        ui.arrived(request, Ok(view_of(GateOutcome::Passed, diff(1, &[], &[]), false)));
        assert!(matches!(ui.state(), CheckState::Done(_)));
        (ui, cameras)
    }

    #[test]
    fn nothing_is_asked_until_a_connected_body_is_chosen_and_only_one_check_runs_at_a_time() {
        let mut ui = CheckUi::default();
        assert!(ui.begin(&[target()]).is_err());
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        assert!(ui.begin(&[]).is_err(), "not connected");
        assert_eq!(*ui.state(), CheckState::Idle);
        assert!(matches!(ui.begin(&[target()]), Ok(Cmd::Check { .. })));
        assert!(ui.begin(&[target()]).is_err(), "one at a time");
    }

    #[test]
    fn incomplete_comparison_is_off_by_default_and_goes_off_when_the_body_changes() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        assert!(!ui.allow_partial());
        ui.set_allow_partial(true);
        ui.sync(&cameras);
        assert!(ui.allow_partial(), "kept for the body it was ticked for");
        let Cmd::Check { request } = ui.begin(&cameras).unwrap() else { panic!() };
        assert!(request.allow_partial);
        let mut other = cameras.clone();
        other[0].firmware = "5.40".into();
        ui.sync(&other);
        assert!(!ui.allow_partial(), "another body, another decision");
        assert_eq!(*ui.state(), CheckState::Idle, "and the check for the old request is withdrawn");
        let two = vec![target(), row("Z 6III", "S", 1)];
        ui.target_key = Some(("Z 6III".into(), "S".into()));
        ui.set_allow_partial(true);
        ui.sync(&two);
        ui.target_key = Some(("Z 9".into(), "T1".into()));
        ui.sync(&two);
        assert!(!ui.allow_partial(), "switching bodies clears it");
    }

    #[test]
    fn a_result_is_shown_only_for_the_request_it_answers() {
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        let Cmd::Check { request } = ui.begin(&cameras).unwrap() else { panic!() };
        let mut other = request.clone();
        other.allow_partial = true;
        ui.arrived(other, Ok(view_of(GateOutcome::Passed, diff(0, &[], &[]), true)));
        assert!(matches!(ui.state(), CheckState::Running(_)), "an answer to another question is ignored");
        ui.arrived(request.clone(), Err("Check failed: camera busy".into()));
        let CheckState::Done(d) = ui.state() else { panic!() };
        assert_eq!(d.result.as_ref().unwrap_err(), "Check failed: camera busy");
        // a stray answer when nothing is running changes nothing
        ui.discard();
        ui.arrived(request, Ok(view_of(GateOutcome::Passed, diff(0, &[], &[]), false)));
        assert_eq!(*ui.state(), CheckState::Idle);
    }

    #[test]
    fn a_changed_camera_data_directory_or_dead_worker_ends_the_result() {
        let (mut ui, mut cameras) = done_ui();
        cameras[0].firmware = "5.40".into();
        ui.sync(&cameras);
        assert_eq!(*ui.state(), CheckState::Idle);
        assert!(ui.notice.contains("withdrawn"));
        let (mut ui, _) = done_ui();
        ui.data_dir_changed();
        assert_eq!(*ui.state(), CheckState::Idle);
        let (mut ui, cameras) = ui_ready();
        ui.sync(&cameras);
        ui.begin(&cameras).unwrap();
        ui.worker_lost();
        assert_eq!(*ui.state(), CheckState::Idle);
        assert!(ui.notice.contains("Nothing was written"));
        // nothing running: a dead worker leaves a shown result alone
        let (mut ui, _) = done_ui();
        ui.worker_lost();
        assert!(matches!(ui.state(), CheckState::Done(_)));
    }

    #[test]
    fn the_reference_note_says_when_no_snapshot_is_marked_and_never_blocks() {
        let snap = |file: &str, is_reference: bool| SnapRow {
            file: file.into(),
            model: "Z 9".into(),
            serial: "T1".into(),
            label: Some("studio".into()),
            captured_at: "2026-10-27T10:00:00Z".into(),
            is_reference,
            name_lossy: false,
        };
        assert_eq!(reference_note(None, &target()), None, "no listing, nothing to say");
        let none = SnapListing { rows: vec![snap("a.json", false)], ..SnapListing::default() };
        assert!(reference_note(Some(&none), &target()).unwrap().contains("No stored snapshot of this body is marked"));
        let marked = SnapListing { rows: vec![snap("a.json", false), snap("b.json", true)], ..SnapListing::default() };
        assert!(reference_note(Some(&marked), &target()).unwrap().starts_with("Reference: "));
        let mut other_body = snap("c.json", true);
        other_body.serial = "T2".into();
        let elsewhere = SnapListing { rows: vec![other_body], ..SnapListing::default() };
        assert!(reference_note(Some(&elsewhere), &target()).unwrap().contains("No stored snapshot"), "another body's reference is not this one's");
    }

    #[test]
    fn the_app_runs_the_check_on_the_worker_and_shows_the_answer() {
        let (mut app, cmd_rx, evt_tx, _dir) = test_app();
        app.check.target_key = Some(("Z 9".into(), "T1".into()));
        app.check.sync(&app.cameras);
        let cmd = app.check.begin(&app.cameras).unwrap();
        app.send_check(cmd);
        let Ok(Cmd::Check { request }) = cmd_rx.try_recv() else { panic!("a check") };
        assert!(app.busy());
        evt_tx.send(Evt::Checked { request: request.clone(), result: Ok(view_of(GateOutcome::Passed, diff(2, &[], &[]), false)) }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Warn);
        assert!(app.status.text.contains("2 differences from the reference"), "{}", app.status.text);
        assert!(matches!(app.check.state(), CheckState::Done(_)));
        assert!(cmd_rx.try_recv().is_err(), "a check writes nothing, so nothing is re-listed");
        // a clean check is plain
        app.check.discard();
        let cmd = app.check.begin(&app.cameras).unwrap();
        app.send_check(cmd);
        let Ok(Cmd::Check { request }) = cmd_rx.try_recv() else { panic!() };
        evt_tx.send(Evt::Checked { request, result: Ok(view_of(GateOutcome::Passed, diff(0, &[], &[]), false)) }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Info);
        // a failed check is an error, and says nothing about drift
        app.check.discard();
        let cmd = app.check.begin(&app.cameras).unwrap();
        app.send_check(cmd);
        let Ok(Cmd::Check { request }) = cmd_rx.try_recv() else { panic!() };
        evt_tx.send(Evt::Checked { request, result: Err("Check failed: camera busy".into()) }).unwrap();
        app.poll();
        assert_eq!(app.status.level, Level::Error);
        assert!(!app.status.text.contains("difference"));
    }
}
