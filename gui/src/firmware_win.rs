// ── Firmware: the archive, what is newer, and getting a file ──────────────
//
// What each `nikon-fleet firmware` command does, as this window uses it (read from `src/main.rs`):
//
//   ls        reads the archive on disk. Listed here by the library call it prints (no process).
//   latest    asks Nikon's Download Center what is newest for each model (NETWORK), reads the USB
//             descriptors of connected bodies and the archive. Writes nothing.
//   browse    opens an embedded browser window on Nikon's page; the PERSON reads and accepts the
//             licence and presses Download; the file is checked against the page's version and
//             archived. Writes the local archive only.
//   fetch     --from-file: archives a file the person downloaded, under the version Nikon lists as
//             newest for the model (it cannot tell which version the file is). Without --from-file
//             it would download ungated files itself; this window never asks for that.
//   add       archives a file under the model and version the person types. Local archive only.
//   pin       records which snapshot is the canonical settings of an archived version. Local only.
//   rollback  writes a bundle directory (the archived firmware, the pinned settings, instructions)
//             into the data directory. It does NOT flash or write to any camera: the person copies
//             the firmware to an SD card and updates the body from its own menu.
//
// None of these writes to a camera, so none goes through the camera worker, and a long one (the
// person reading a licence in the browser) does not hold a camera operation up. Each runs on its
// own thread, one at a time. Those that change the archive or the data directory ask first, in a
// text naming what changes. Text that came from Nikon's page is data: it is shown as plain text,
// never as markup, and no link in it is opened by this program.

use super::*;
use nikon_fleet::firmware::{ArchiveScan, ArchiveSkipKind, RetrievalMethod};
use nikon_fleet::firmware_source::FwVersion;
use nikon_fleet::ops::firmware_latest::{LatestDownload, LatestRelation, LatestStatus};
use nikon_fleet::ops::render::printable;

/// Text from a command or a web page that may span lines, safe to show: control characters other
/// than the line break are written as escapes, and nothing in it is interpreted.
pub(crate) fn plain(text: &str) -> String {
    text.lines().map(printable).collect::<Vec<_>>().join("\n")
}

/// Looking up the newest release is a few web requests.
pub(crate) const LATEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Archiving copies and hashes a file of tens of megabytes, and `fetch` looks the release up first.
pub(crate) const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// The person reads Nikon's licence and downloads in the browser window: minutes, not seconds.
pub(crate) const BROWSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) const PIN_TIMEOUT: Duration = Duration::from_secs(60);

// ── The archive, as listed ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveRow {
    pub model: String,
    pub version: String,
    pub size_bytes: u64,
    pub archived_at: String,
    /// `firmware.bin` is there; an entry whose binary was deleted by hand is still listed.
    pub bin_present: bool,
    /// The snapshot pinned as this version's canonical settings, as the archive records it.
    pub pinned: Option<String>,
    pub how: Option<RetrievalMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveUnreadable {
    pub directory: String,
    pub kind: ArchiveSkipKind,
    pub error: String,
}

/// What `firmware ls` prints, kept whole: the entries it could not list are part of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ArchiveView {
    pub root: String,
    pub root_exists: bool,
    pub rows: Vec<ArchiveRow>,
    pub unreadable: Vec<ArchiveUnreadable>,
    pub skipped_dir_entries: usize,
}

pub(crate) fn archive_view(scan: &ArchiveScan) -> ArchiveView {
    ArchiveView {
        root: scan.root.display().to_string(),
        root_exists: scan.root_exists,
        rows: scan
            .entries
            .iter()
            .map(|e| ArchiveRow {
                model: e.model.clone(),
                version: e.version.clone(),
                size_bytes: e.meta.bin_size_bytes,
                archived_at: e.meta.archived_at.clone(),
                bin_present: e.bin_present,
                pinned: e.meta.canonical_snapshot_path.clone(),
                how: e.meta.provenance.as_ref().map(|p| p.method),
            })
            .collect(),
        unreadable: scan
            .skipped
            .iter()
            .map(|s| ArchiveUnreadable { directory: s.directory.display().to_string(), kind: s.kind, error: s.error.clone() })
            .collect(),
        skipped_dir_entries: scan.skipped_dir_entries,
    }
}

impl ArchiveView {
    /// Every entry on disk could be listed, so "not in the list" means "not archived".
    pub fn complete(&self) -> bool {
        self.unreadable.is_empty() && self.skipped_dir_entries == 0
    }

    pub fn holds(&self, model: &str, version: &str) -> bool {
        self.rows.iter().any(|r| r.model == model && r.version == version)
    }

    pub fn models(&self) -> BTreeSet<String> {
        self.rows.iter().map(|r| r.model.clone()).collect()
    }

    pub fn row(&self, model: &str, version: &str) -> Option<&ArchiveRow> {
        self.rows.iter().find(|r| r.model == model && r.version == version)
    }

    /// Words for a person about what is not in the list.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for u in &self.unreadable {
            out.push(format!("{} could not be listed ({}): {}", printable(&u.directory), skip_words(u.kind), printable(&u.error)));
        }
        if self.skipped_dir_entries > 0 {
            out.push(format!(
                "{} director{} under {} could not be read and {} not listed",
                self.skipped_dir_entries,
                if self.skipped_dir_entries == 1 { "y" } else { "ies" },
                printable(&self.root),
                if self.skipped_dir_entries == 1 { "was" } else { "were" },
            ));
        }
        out
    }
}

/// A closed `ArchiveSkipKind` in words.
fn skip_words(kind: ArchiveSkipKind) -> &'static str {
    match kind {
        ArchiveSkipKind::MetadataUnreadable => "its metadata.json cannot be read",
        ArchiveSkipKind::MetadataCorrupt => "its metadata.json is not valid",
        ArchiveSkipKind::UnsupportedVersion => "its metadata.json is a format this build does not read",
        ArchiveSkipKind::IncompleteArchive => "an interrupted archive: firmware.bin without metadata.json",
    }
}

fn how_words(how: Option<RetrievalMethod>) -> &'static str {
    match how {
        None => "added from a file",
        Some(RetrievalMethod::ManualAfterGate) => "downloaded by you, archived from the file",
        Some(RetrievalMethod::DirectDownload) => "downloaded from an ungated URL",
        Some(RetrievalMethod::BrowserAfterGate) => "downloaded by you in the browser window",
    }
}

/// Whether the version a connected body runs is in the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunningArchived {
    Archived,
    NotArchived,
    /// Cannot be said, and why (an unreadable archive entry could be that version; the body's
    /// firmware is not a version string).
    Unknown(String),
}

pub(crate) fn running_archived(view: &ArchiveView, camera: &CameraRow) -> RunningArchived {
    let running = match FwVersion::parse_canonical(&camera.firmware) {
        Ok(v) => v,
        Err(_) => return RunningArchived::Unknown(format!("its firmware {:?} is not a version number", printable(&camera.firmware))),
    };
    let same_model = || view.rows.iter().filter(|r| r.model == camera.model);
    if same_model().any(|r| FwVersion::parse_canonical(&r.version).is_ok_and(|v| v == running)) {
        return RunningArchived::Archived;
    }
    if !view.complete() {
        return RunningArchived::Unknown("some archive entries could not be listed, and one of them may be that version".to_string());
    }
    // An entry whose version is not a canonical string cannot be matched, so it could be this one.
    if same_model().any(|r| FwVersion::parse_canonical(&r.version).is_err()) {
        return RunningArchived::Unknown("an archive entry for this model has a version that is not a version number".to_string());
    }
    RunningArchived::NotArchived
}

// ── `firmware latest --json`, as the library prints it ────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestReleaseJson {
    pub version: String,
    pub released: Option<String>,
    pub page_url: String,
    pub download: LatestDownload,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestUnreadableJson {
    pub directory: String,
    pub error_kind: ArchiveSkipKind,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestUsbScanJson {
    pub complete: bool,
    pub enumeration_error: Option<String>,
    pub unreadable: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestCameraJson {
    pub serial: String,
    pub installed: String,
    pub relation: LatestRelation,
    pub relation_detail: Option<String>,
    pub running_version_archived: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestModelJson {
    pub model: String,
    pub status: LatestStatus,
    pub latest: Option<LatestReleaseJson>,
    pub reason: Option<String>,
    pub archive_holds_latest: Option<bool>,
    pub archived_versions: Vec<String>,
    pub archive_unreadable: Vec<LatestUnreadableJson>,
    pub archive_unreadable_dir_entries: usize,
    pub cameras: Vec<LatestCameraJson>,
    pub usb_scan: LatestUsbScanJson,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct LatestReportJson {
    pub models: Vec<LatestModelJson>,
}

/// The survey, with what the command said on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LatestView {
    pub report: LatestReportJson,
    pub cli_log: String,
}

/// One model's line, and how loudly. Text from Nikon's page is `printable`d and shown as it is.
pub(crate) fn latest_headline(m: &LatestModelJson) -> (Level, String) {
    match (m.status, &m.latest) {
        (LatestStatus::CouldNotDetermine, _) | (_, None) => (
            Level::Warn,
            format!(
                "{}: could not be determined ({}). This says nothing about whether newer firmware exists.",
                printable(&m.model),
                printable(m.reason.as_deref().unwrap_or("no reason given"))
            ),
        ),
        (LatestStatus::NewAvailable, Some(r)) => (
            Level::Warn,
            format!(
                "{}: {} is the newest listed{} and is NOT in your archive.",
                printable(&m.model),
                printable(&r.version),
                r.released.as_deref().map(|d| format!(" (released {})", printable(d))).unwrap_or_default()
            ),
        ),
        (LatestStatus::AlreadyArchived, Some(r)) => (
            Level::Info,
            format!(
                "{}: {} is the newest listed{} and is already in your archive.",
                printable(&m.model),
                printable(&r.version),
                r.released.as_deref().map(|d| format!(" (released {})", printable(d))).unwrap_or_default()
            ),
        ),
    }
}

/// One connected body against the newest release.
pub(crate) fn latest_camera_line(c: &LatestCameraJson) -> (Level, String) {
    let relation = match c.relation {
        LatestRelation::Behind => (Level::Warn, "behind the newest".to_string()),
        LatestRelation::Current => (Level::Info, "at the newest".to_string()),
        LatestRelation::Ahead => (Level::Warn, "AHEAD of the newest listed (the page may be out of date)".to_string()),
        LatestRelation::Unreadable => (
            Level::Warn,
            format!("its firmware is not a version number ({})", printable(c.relation_detail.as_deref().unwrap_or("no detail"))),
        ),
        LatestRelation::LatestUnknown => (Level::Warn, "the newest could not be determined".to_string()),
    };
    let archived = match c.running_version_archived {
        Some(true) => "the version it runs is archived",
        Some(false) => "the version it runs is NOT archived",
        None => "whether the version it runs is archived is unknown",
    };
    (relation.0, format!("{} runs {}: {}; {archived}.", printable(&c.serial), printable(&c.installed), relation.1))
}

/// Notes a person needs before trusting "no body attached" or "not archived".
pub(crate) fn latest_notes(m: &LatestModelJson) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(e) = &m.usb_scan.enumeration_error {
        out.push(format!("The USB bus could not be listed ({}), so connected bodies are unknown.", printable(e)));
    } else if !m.usb_scan.complete {
        out.push("The USB scan was incomplete, so a body may be attached that is not listed.".to_string());
    }
    if !m.usb_scan.unreadable.is_empty() {
        out.push(format!("{} Nikon USB device(s) could not be read.", m.usb_scan.unreadable.len()));
    }
    for u in &m.archive_unreadable {
        out.push(format!("Archive entry {} could not be listed ({}).", printable(&u.directory), skip_words(u.error_kind)));
    }
    if m.archive_unreadable_dir_entries > 0 {
        out.push(format!("{} archive director(ies) could not be read at all.", m.archive_unreadable_dir_entries));
    }
    for w in &m.warnings {
        out.push(printable(w));
    }
    out
}

// ── What can be run ───────────────────────────────────────────────────────

/// One firmware operation: exactly what is run. A confirmation is for this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FwOp {
    /// List the archive (the library call `firmware ls` prints).
    Refresh,
    /// `firmware latest --json`, for these models (none: the command's own default).
    Latest { models: Vec<String> },
    /// `firmware browse`: the person accepts the licence and downloads in a window.
    Browse { model: String },
    /// `firmware add`: a file the person downloaded, as the model and version they typed.
    Add { file: PathBuf, model: String, version: String },
    /// `firmware fetch --from-file`, as a dry run or for real.
    FetchFile { file: PathBuf, model: String, dry_run: bool },
    /// `firmware pin`: a snapshot in `snapshots/` as an archived version's canonical settings.
    Pin { snapshot: String, model: String, version: String },
    /// `firmware rollback`, into `output_dir`.
    Rollback { model: String, version: String, output_dir: PathBuf },
}

impl FwOp {
    pub fn title(&self) -> &'static str {
        match self {
            FwOp::Refresh => "List the archive",
            FwOp::Latest { .. } => "Check for newer firmware",
            FwOp::Browse { .. } => "Get firmware in the browser window",
            FwOp::Add { .. } => "Archive a file",
            FwOp::FetchFile { dry_run: true, .. } => "Look up what a file would be archived as",
            FwOp::FetchFile { dry_run: false, .. } => "Archive a file as Nikon's newest",
            FwOp::Pin { .. } => "Pin settings",
            FwOp::Rollback { .. } => "Create a rollback bundle",
        }
    }

    /// Changes the archive or the data directory (and so is asked about first, and re-lists after).
    pub fn writes_locally(&self) -> bool {
        match self {
            FwOp::Refresh | FwOp::Latest { .. } | FwOp::FetchFile { dry_run: true, .. } => false,
            FwOp::Browse { .. } | FwOp::Add { .. } | FwOp::FetchFile { dry_run: false, .. } | FwOp::Pin { .. } | FwOp::Rollback { .. } => true,
        }
    }

    pub fn timeout(&self) -> Duration {
        match self {
            FwOp::Refresh => Duration::from_secs(60),
            FwOp::Latest { .. } => LATEST_TIMEOUT,
            FwOp::Browse { .. } => BROWSE_TIMEOUT,
            FwOp::Add { .. } | FwOp::FetchFile { .. } | FwOp::Rollback { .. } => ARCHIVE_TIMEOUT,
            FwOp::Pin { .. } => PIN_TIMEOUT,
        }
    }

    /// The CLI arguments, `--flag=value` throughout so a value that starts with `-` stays attached,
    /// and `--` before a positional so a file name cannot be taken for a flag. `Refresh` has none.
    pub fn argv(&self, data_dir: &Path) -> Result<Option<Vec<String>>, String> {
        let dir = data_dir
            .to_str()
            .ok_or_else(|| format!("the data directory {} is not valid UTF-8", data_dir.display()))?;
        let path = |p: &Path| p.to_str().map(str::to_string).ok_or_else(|| format!("{} is not valid UTF-8", p.display()));
        let mut argv = vec![format!("--data-dir={dir}"), "firmware".to_string()];
        match self {
            FwOp::Refresh => return Ok(None),
            FwOp::Latest { models } => {
                argv.extend(["latest".into(), "--json".into()]);
                argv.extend(models.iter().map(|m| format!("--model={m}")));
            }
            FwOp::Browse { model } => {
                argv.extend(["browse".into(), "--".into(), model.clone()]);
            }
            FwOp::Add { file, model, version } => {
                argv.extend(["add".into(), format!("--model={model}"), format!("--version={version}"), "--".into(), path(file)?]);
            }
            FwOp::FetchFile { file, model, dry_run } => {
                argv.extend(["fetch".into(), format!("--model={model}")]);
                if *dry_run {
                    argv.push("--dry-run".into());
                }
                argv.push(format!("--from-file={}", path(file)?));
            }
            FwOp::Pin { snapshot, model, version } => {
                argv.extend(["pin".into(), format!("--model={model}"), format!("--version={version}"), "--".into(), snapshot_path_for_cli(data_dir, snapshot)?]);
            }
            FwOp::Rollback { model, version, output_dir } => {
                argv.extend(["rollback".into(), format!("--model={model}"), format!("--version={version}"), format!("--output-dir={}", path(output_dir)?)]);
            }
        }
        Ok(Some(argv))
    }
}

/// Where a rollback bundle goes: the CLI's own default place and name, spelled out, so the window
/// knows the path to show without reading it off the command's output.
pub(crate) fn rollback_dir(data_dir: &Path, model: &str, version: &str, now: &str) -> Result<PathBuf, String> {
    let slug = nikon_fleet::firmware::model_slug(model);
    // The stamp the CLI writes: RFC 3339 without `:` and `-`.
    let stamp = now.replace([':', '-'], "");
    let name = format!("{slug}_{version}_{stamp}");
    if !is_safe_path_component(&name) {
        return Err(format!("{name:?} is not a plain directory name"));
    }
    std::path::absolute(data_dir.join("rollback-bundles").join(name)).map_err(|e| format!("the bundle's path could not be made absolute: {e}"))
}

/// How a command that writes to the local archive ended. The exit status decides, never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteEnd {
    Done { stdout: String, stderr: String },
    /// Exit `EXIT_ALREADY_ARCHIVED`: that model and version is already archived; nothing changed.
    NothingChanged { message: String },
    Failed { message: String, stdout: String, stderr: String, how: FailedHow },
}

/// Whether a failed command is one the archive is known to be untouched by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailedHow {
    /// The command refused or failed on its own account (exit 1 or 2) or never started.
    Refused,
    /// It was stopped, killed or ended in a way that is not its own refusal: a partly written
    /// entry is possible. The archive listing says what is there.
    Stopped,
}

/// The exit status `firmware add` / `fetch` / `browse` end with when that model and version is
/// already archived and nothing was changed (`EXIT_ALREADY_ARCHIVED` in `src/main.rs`).
pub(crate) const EXIT_ALREADY_ARCHIVED: i32 = 3;

pub(crate) fn classify_write(op: &FwOp, result: Result<CliOutput, CliError>) -> WriteEnd {
    match result {
        Ok(out) => WriteEnd::Done { stdout: out.stdout, stderr: out.stderr },
        Err(e) => {
            let documents_3 = matches!(op, FwOp::Add { .. } | FwOp::FetchFile { .. } | FwOp::Browse { .. });
            if documents_3 && e.kind == CliFailure::Exit && e.code == Some(EXIT_ALREADY_ARCHIVED) {
                return WriteEnd::NothingChanged { message: e.message };
            }
            let how = match (e.kind, e.code) {
                (CliFailure::Spawn, _) | (CliFailure::Exit, Some(1) | Some(2)) => FailedHow::Refused,
                _ => FailedHow::Stopped,
            };
            WriteEnd::Failed { message: e.message, stdout: e.stdout, stderr: e.stderr, how }
        }
    }
}

/// An operation's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FwResult {
    Archive(ArchiveView),
    Latest(Result<LatestView, String>),
    Write(WriteEnd),
}

#[derive(Debug)]
pub(crate) struct FwAnswer {
    pub op: FwOp,
    pub result: FwResult,
}

/// Run `op`. A long call: off the UI thread.
pub(crate) fn execute(cli: &Result<PathBuf, String>, data_dir: &Path, op: &FwOp) -> FwResult {
    if matches!(op, FwOp::Refresh) {
        return FwResult::Archive(archive_view(&scan_archives(data_dir, None)));
    }
    let program = match cli {
        Ok(p) => p,
        Err(e) => {
            return match op {
                FwOp::Latest { .. } => FwResult::Latest(Err(e.clone())),
                _ => FwResult::Write(WriteEnd::Failed { message: e.clone(), stdout: String::new(), stderr: String::new(), how: FailedHow::Refused }),
            };
        }
    };
    let argv = match op.argv(data_dir) {
        Ok(Some(a)) => a,
        Ok(None) => unreachable!("Refresh returned above"),
        Err(e) => {
            return match op {
                FwOp::Latest { .. } => FwResult::Latest(Err(e)),
                _ => FwResult::Write(WriteEnd::Failed { message: e, stdout: String::new(), stderr: String::new(), how: FailedHow::Refused }),
            };
        }
    };
    let ran = run_cli(program, &argv, None, op.timeout());
    if matches!(op, FwOp::Latest { .. }) {
        // Exit 1 is also "some model could not be determined", with the survey printed first: what
        // is read is stdout, and the survey says per model what could not be determined.
        let (stdout, stderr, failure) = match ran {
            Ok(o) => (o.stdout, o.stderr, None),
            Err(e) => (e.stdout.clone(), e.stderr.clone(), Some(e)),
        };
        return FwResult::Latest(match serde_json::from_str::<LatestReportJson>(stdout.trim()) {
            Ok(report) => Ok(LatestView { report, cli_log: stderr }),
            Err(e) => Err(match failure {
                Some(f) => format!("The lookup failed: {}", f.message),
                None => format!("The lookup succeeded but its report could not be read ({e})."),
            }),
        });
    }
    FwResult::Write(classify_write(op, ran))
}

// ── The confirmations ─────────────────────────────────────────────────────

fn size_text(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MIB { format!("{:.1} MiB", bytes as f64 / MIB) } else { format!("{bytes} bytes") }
}

/// The question put to the person before a firmware command that changes the local archive or the
/// data directory. It says what is written, where, and that no camera is touched. `None` for an
/// operation that writes nothing.
pub(crate) fn confirm_text(op: &FwOp, data_dir: &Path, archive: Option<&ArchiveView>, extra: Option<&str>) -> Option<String> {
    let fw_dir = |model: &str, version: &str| nikon_fleet::firmware::archive_dir(data_dir, model, version).display().to_string();
    Some(match op {
        FwOp::Refresh | FwOp::Latest { .. } => return None,
        FwOp::FetchFile { dry_run: true, .. } => return None,
        FwOp::Add { file, model, version } => {
            let size = fs::metadata(file).map(|m| size_text(m.len())).unwrap_or_else(|e| format!("size unknown: {e}"));
            let already = archive.is_some_and(|a| a.holds(model, version));
            format!(
                "Copy {} ({size}) into the firmware archive as {model} firmware {version}?\n\
                 It is stored in {} with a record of its SHA-256. The program cannot tell what version the file really is: \
                 the version is the one you typed. No camera is touched.{}",
                file.display(),
                fw_dir(model, version),
                if already {
                    "\nThat model and version is ALREADY archived: this will be refused and nothing will change (replacing an archived entry is not offered here)."
                } else {
                    ""
                }
            )
        }
        FwOp::FetchFile { file, model, dry_run: false } => {
            let size = fs::metadata(file).map(|m| size_text(m.len())).unwrap_or_else(|e| format!("size unknown: {e}"));
            format!(
                "Copy {} ({size}) into the firmware archive as {model}'s newest firmware, under the version Nikon's page lists when the command runs?\n\
                 The program cannot tell what version the file is: only continue if it is that release. The look-up above shows which version it found. \
                 The page is saved next to the file as provenance. No camera is touched.{}",
                file.display(),
                extra.map(|e| format!("\nThe look-up said:\n{}", plain(e))).unwrap_or_default()
            )
        }
        FwOp::Browse { model } => format!(
            "Open Nikon's download page for the newest {model} firmware in a window?\n\
             You read and accept Nikon's licence and press Download yourself: this program never accepts it or presses the button. \
             The file you download is checked against the page's version and then archived under {}. No camera is touched. \
             If no window opens in this build, the result says so and you can download the file in your own browser and archive it with \"Archive a file I downloaded\".",
            nikon_fleet::firmware::firmware_root(data_dir).join(nikon_fleet::firmware::model_slug(model)).display()
        ),
        FwOp::Pin { snapshot, model, version } => {
            let current = archive
                .and_then(|a| a.row(model, version))
                .map(|r| match &r.pinned {
                    Some(p) => format!("It currently points to {}; that is replaced.", printable(p)),
                    None => "Nothing is pinned for it now.".to_string(),
                })
                .unwrap_or_else(|| "That version is not in the listing shown.".to_string());
            format!(
                "Make {snapshot} the canonical settings for {model} firmware {version}?\n{current} \
                 This changes the archive's record for that version only ({}); no snapshot is changed and no camera is touched.",
                fw_dir(model, version)
            )
        }
        FwOp::Rollback { model, version, output_dir } => {
            let size = archive.and_then(|a| a.row(model, version)).map(|r| size_text(r.size_bytes)).unwrap_or_else(|| "size unknown".into());
            format!(
                "Create a rollback bundle for {model} → firmware {version} in {}?\n\
                 It copies the archived firmware.bin ({size}, SHA-256 checked first), the pinned settings if there are any, and a step-by-step instruction file. \
                 It does NOT flash or write to any camera: to go back you copy firmware.bin to an SD card and update the body from its own menu.",
                output_dir.display()
            )
        }
    })
}

/// The warning to show beside a pin when the snapshot is of other firmware than the version it is
/// pinned to (the CLI proceeds on a mismatch and only prints it to stderr).
pub(crate) fn pin_mismatch(snapshot_firmware: &str, version: &str) -> Option<String> {
    (!snapshot_firmware.is_empty() && snapshot_firmware != version)
        .then(|| format!("This snapshot was taken on firmware {}, not {version}; pinning it records settings from other firmware as {version}'s.", printable(snapshot_firmware)))
}

// ── The add form ──────────────────────────────────────────────────────────

/// Why a file cannot be archived as typed, before anything is asked of the command.
pub(crate) fn add_refusal(file: Option<&Path>, model: &str, version: &str) -> Option<String> {
    let Some(file) = file else { return Some("Choose the firmware file first.".to_string()) };
    match fs::metadata(file) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Some(format!("{} is not a file.", file.display())),
        Err(e) => return Some(format!("{} cannot be read: {e}", file.display())),
    }
    let (model, version) = (model.trim(), version.trim());
    if model.is_empty() {
        return Some("Enter the camera model, e.g. \"Z 9\".".to_string());
    }
    if !is_safe_path_component(&nikon_fleet::firmware::model_slug(model)) {
        return Some(format!("{model:?} cannot be used as a model name: it must be one plain directory name."));
    }
    if FwVersion::parse_canonical(version).is_err() || !is_safe_path_component(version) {
        return Some(format!(
            "{version:?} is not a firmware version in the shape the camera reports (major.minor with two digits after the dot, e.g. 5.31): a differently written version would never be recognised as the one a camera runs."
        ));
    }
    None
}

/// Models offered in the pickers: the connected bodies' and the archive's.
pub(crate) fn known_models(cameras: &[CameraRow], archive: Option<&ArchiveView>) -> Vec<String> {
    let mut all: BTreeSet<String> = cameras.iter().map(|c| c.model.clone()).collect();
    if let Some(a) = archive {
        all.extend(a.models());
    }
    all.into_iter().collect()
}

// ── The window's state ────────────────────────────────────────────────────

/// An operation on its own thread.
#[derive(Debug)]
pub(crate) struct FwRun {
    pub op: FwOp,
    rx: mpsc::Receiver<FwAnswer>,
    pub started: Instant,
}

/// A question waiting for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FwPending {
    pub op: FwOp,
    pub text: String,
}

/// What the last operation came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FwNote {
    pub level: Level,
    pub headline: String,
    /// What the command printed, for a person to read; nothing branches on it.
    pub detail: String,
}

#[derive(Debug, Default)]
pub(crate) struct AddForm {
    pub file: Option<PathBuf>,
    pub model: String,
    pub version: String,
}

impl AddForm {
    /// The operation the form asks for, with the model and version as they will be stored (trimmed),
    /// or why it asks for none.
    pub fn op(&self) -> Result<FwOp, String> {
        match add_refusal(self.file.as_deref(), &self.model, &self.version) {
            Some(why) => Err(why),
            None => Ok(FwOp::Add {
                file: self.file.clone().expect("a refusal covers no file"),
                model: self.model.trim().to_string(),
                version: self.version.trim().to_string(),
            }),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct FwUi {
    pub open: bool,
    pub archive: Option<ArchiveView>,
    pub latest: Option<LatestView>,
    pub running: Option<FwRun>,
    pub pending: Option<FwPending>,
    pub note: Option<FwNote>,
    pub add: AddForm,
    /// The archived version a pin is being chosen for, and the snapshot chosen.
    pub pin_for: Option<(String, String)>,
    pub pin_snapshot: Option<String>,
    /// A dry run's output, kept for the confirmation of the real run it was for.
    pub dry_run: Option<(FwOp, String)>,
    /// The archive changed (or may have): list it again once nothing is running.
    pub relist: bool,
    pub notice: String,
}

impl FwUi {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// Run `op` on its own thread, if nothing else is running.
    pub fn start(&mut self, op: FwOp, data_dir: &Path, cli: Result<PathBuf, String>, ctx: &egui::Context) -> Result<(), String> {
        if self.running.is_some() {
            return Err("Another firmware operation is still running.".to_string());
        }
        let (tx, rx) = mpsc::channel();
        let thread_op = op.clone();
        let dir = data_dir.to_path_buf();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("firmware-op".into())
            .spawn(move || {
                let result = execute(&cli, &dir, &thread_op);
                // The receiver is gone only when the window closed; there is no one to tell.
                let _ = tx.send(FwAnswer { op: thread_op, result });
                ctx.request_repaint();
            })
            .map_err(|e| format!("could not start the operation: {e}"))?;
        self.notice.clear();
        self.running = Some(FwRun { op, rx, started: Instant::now() });
        Ok(())
    }

    /// Ask the person about `op` (which changes something), or run it if it changes nothing.
    pub fn ask(&mut self, op: FwOp, data_dir: &Path, extra: Option<&str>) -> Option<FwOp> {
        match confirm_text(&op, data_dir, self.archive.as_ref(), extra) {
            None => Some(op),
            Some(text) => {
                self.pending = Some(FwPending { op, text });
                None
            }
        }
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Take the confirmed operation out, to be started. Only what was asked is returned.
    pub fn confirm(&mut self) -> Option<FwOp> {
        if self.running.is_some() {
            return None;
        }
        self.pending.take().map(|p| p.op)
    }

    /// The data directory changed: a question about the old one, and what was listed from it, no
    /// longer applies. A running operation keeps its own directory and its answer is still taken.
    pub fn data_dir_changed(&mut self) {
        self.pending = None;
        self.archive = None;
        self.latest = None;
        self.dry_run = None;
        self.pin_for = None;
        self.pin_snapshot = None;
        self.relist = true;
    }

    /// Collect an answer if there is one. A thread that ended without answering (it panicked) is a
    /// stopped operation, said as such: a write may have been partly done.
    pub fn poll(&mut self) {
        let Some(run) = &self.running else { return };
        match run.rx.try_recv() {
            Ok(answer) => {
                self.running = None;
                self.finish(answer);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                let op = run.op.clone();
                self.running = None;
                let writes = op.writes_locally();
                self.relist |= writes;
                self.note = Some(FwNote {
                    level: if writes { Level::Error } else { Level::Warn },
                    headline: format!("{}: the operation stopped without an answer.", op.title()),
                    detail: if writes {
                        "It may have changed the archive or the data directory. The listing below is read again; an interrupted archive entry is listed as unreadable.".to_string()
                    } else {
                        "It only reads, so nothing was changed.".to_string()
                    },
                });
            }
        }
    }

    pub fn finish(&mut self, answer: FwAnswer) {
        let FwAnswer { op, result } = answer;
        match result {
            FwResult::Archive(view) => {
                self.archive = Some(view);
                self.relist = false;
            }
            FwResult::Latest(Ok(view)) => {
                let new = view.report.models.iter().filter(|m| m.status == LatestStatus::NewAvailable).count();
                self.note = Some(FwNote {
                    level: if new > 0 { Level::Warn } else { Level::Info },
                    headline: format!("Looked up {} model(s): {new} with firmware not in your archive.", view.report.models.len()),
                    detail: String::new(),
                });
                self.latest = Some(view);
            }
            FwResult::Latest(Err(e)) => {
                self.note = Some(FwNote { level: Level::Error, headline: "Checking for newer firmware failed.".to_string(), detail: e });
            }
            FwResult::Write(end) => {
                // A dry run only reports: its text is what the real run's question quotes.
                if let (FwOp::FetchFile { dry_run: true, .. }, WriteEnd::Done { stdout, stderr }) = (&op, &end) {
                    let text = if stderr.trim().is_empty() { stdout.clone() } else { format!("{stdout}\n{stderr}") };
                    self.dry_run = Some((op.clone(), text));
                    self.note = Some(FwNote { level: Level::Info, headline: "Look-up done. Nothing was written.".to_string(), detail: stdout.clone() });
                    return;
                }
                self.relist |= op.writes_locally();
                self.note = Some(write_note(&op, end));
            }
        }
    }
}

/// What a write operation came to, in words.
pub(crate) fn write_note(op: &FwOp, end: WriteEnd) -> FwNote {
    let title = op.title();
    match end {
        WriteEnd::Done { stdout, stderr } => FwNote {
            level: Level::Info,
            headline: format!("{title}: done."),
            detail: if stderr.trim().is_empty() { stdout } else { format!("{stdout}\n[stderr]\n{stderr}") },
        },
        WriteEnd::NothingChanged { message } => FwNote {
            level: Level::Warn,
            headline: format!("{title}: that model and version is already in the archive; nothing was changed."),
            detail: message,
        },
        WriteEnd::Failed { message, stdout, stderr, how } => FwNote {
            level: Level::Error,
            headline: match how {
                FailedHow::Refused => format!("{title}: failed. The archive was not changed by it as far as the command reported; the listing shows what is there."),
                FailedHow::Stopped => format!("{title}: stopped before it finished. A partly written entry is possible; the listing shows what is there (an interrupted entry is listed as unreadable)."),
            },
            detail: [message, stdout, stderr].into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n"),
        },
    }
}

// ── The window ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum FwAction {
    Refresh,
    CheckLatest,
    Browse(String),
    ChoosePin(String, String),
    Rollback(String, String),
    AskAdd,
    AskFetchDryRun,
    AskFetchReal,
    AskPin,
    PickFile,
    Confirm,
    Cancel,
    CopyText(String),
}

impl FleetApp {
    pub(crate) fn poll_firmware(&mut self, ctx: &egui::Context) {
        self.firmware.poll();
        if self.firmware.relist && !self.firmware.busy() {
            if let Some(d) = self.settings.data_dir.clone() {
                // A listing is a quick local read; a failure to start it leaves the old one.
                if let Err(e) = self.firmware.start(FwOp::Refresh, &d, locate_cli(), ctx) {
                    self.firmware.notice = e;
                }
            }
            self.firmware.relist = false;
        }
        if self.firmware.running.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    pub(crate) fn ui_firmware(&mut self, ctx: &egui::Context) {
        if !self.firmware.open {
            return;
        }
        if self.firmware.archive.is_none() && !self.firmware.busy() {
            self.firmware.relist = true;
        }
        let mut open = true;
        let mut action: Option<FwAction> = None;
        let camera_busy = self.busy();
        let data_ready = self.settings.data_dir.is_some();

        egui::Window::new("Firmware")
            .open(&mut open)
            .default_size([760.0, 680.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                let f = &mut self.firmware;
                let cameras = &self.cameras;
                let listing = self.listing.as_ref();
                egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(760.0).show(ui, |ui| {
                    draw_fw_status(ui, f);
                    if let Some(p) = &f.pending {
                        ui.separator();
                        ui.colored_label(WARN_COLOR, &p.text);
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(!f.busy(), |ui| {
                                if ui.button(format!("Confirm: {}", p.op.title())).clicked() {
                                    action = Some(FwAction::Confirm);
                                }
                            });
                            if ui.button("Cancel").clicked() {
                                action = Some(FwAction::Cancel);
                            }
                        });
                        ui.separator();
                    }
                    let can_act = !f.busy() && f.pending.is_none() && data_ready;

                    // 1. The archive
                    ui.horizontal(|ui| {
                        ui.heading("Archive");
                        ui.add_enabled_ui(!f.busy(), |ui| {
                            if ui.small_button("Refresh").clicked() {
                                action = Some(FwAction::Refresh);
                            }
                        });
                    });
                    for c in cameras {
                        let (level, text) = match f.archive.as_ref().map(|a| running_archived(a, c)) {
                            Some(RunningArchived::Archived) => (Level::Info, "this version is archived".to_string()),
                            Some(RunningArchived::NotArchived) => (Level::Warn, "this version is NOT in the archive".to_string()),
                            Some(RunningArchived::Unknown(why)) => (Level::Warn, format!("cannot tell whether this version is archived: {why}")),
                            None => (Level::Info, "archive not listed yet".to_string()),
                        };
                        colored(ui, level, format!("{} {} runs {}: {text}", c.model, c.serial, printable(&c.firmware)));
                    }
                    match &f.archive {
                        None => {
                            ui.label(egui::RichText::new("Listing the archive…").italics().weak());
                        }
                        Some(a) => {
                            for p in a.problems() {
                                ui.colored_label(WARN_COLOR, p);
                            }
                            if a.rows.is_empty() {
                                ui.label(egui::RichText::new(if a.root_exists { "No firmware is archived." } else { "No firmware is archived (there is no firmware directory yet)." }).italics().weak());
                            }
                            egui::Grid::new("fw_archive").striped(true).show(ui, |ui| {
                                for r in &a.rows {
                                    ui.label(egui::RichText::new(&r.model).strong());
                                    ui.label(&r.version);
                                    ui.label(size_text(r.size_bytes));
                                    ui.label(r.archived_at.get(..10).unwrap_or(&r.archived_at));
                                    ui.label(egui::RichText::new(how_words(r.how)).weak());
                                    ui.label(match (&r.pinned, r.bin_present) {
                                        (_, false) => egui::RichText::new("firmware.bin MISSING").color(ERROR_COLOR),
                                        (Some(_), true) => egui::RichText::new("settings pinned").weak(),
                                        (None, true) => egui::RichText::new("no settings pinned").weak(),
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_enabled_ui(can_act, |ui| {
                                            if ui.small_button("Pin settings…").clicked() {
                                                action = Some(FwAction::ChoosePin(r.model.clone(), r.version.clone()));
                                            }
                                            if ui.small_button("Rollback bundle…").clicked() {
                                                action = Some(FwAction::Rollback(r.model.clone(), r.version.clone()));
                                            }
                                        });
                                    });
                                    ui.end_row();
                                }
                            });
                        }
                    }
                    if let Some((model, version)) = f.pin_for.clone() {
                        ui.separator();
                        ui.label(egui::RichText::new(format!("Pin settings for {model} firmware {version}")).strong());
                        let rows: Vec<&SnapRow> = listing.map(|l| l.rows.iter().filter(|r| r.model == model && !r.name_lossy).collect()).unwrap_or_default();
                        if rows.is_empty() {
                            ui.label(egui::RichText::new(format!("No stored snapshot is of a {model}.")).italics().weak());
                        }
                        let shown = f
                            .pin_snapshot
                            .as_ref()
                            .and_then(|s| rows.iter().find(|r| &r.file == s))
                            .map_or("choose a snapshot…".to_string(), |r| format!("{}  fw {}", snapshot_label(r), printable(&r.firmware)));
                        egui::ComboBox::from_id_salt("fw_pin_snapshot").selected_text(shown).width(520.0).show_ui(ui, |ui| {
                            for r in &rows {
                                let picked = f.pin_snapshot.as_ref() == Some(&r.file);
                                if ui.selectable_label(picked, format!("{}  {}  fw {}", r.serial, snapshot_label(r), printable(&r.firmware))).clicked() {
                                    f.pin_snapshot = Some(r.file.clone());
                                }
                            }
                        });
                        if let Some(row) = f.pin_snapshot.as_ref().and_then(|s| rows.iter().find(|r| &r.file == s)) {
                            if let Some(w) = pin_mismatch(&row.firmware, &version) {
                                ui.colored_label(WARN_COLOR, w);
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.add_enabled_ui(can_act && f.pin_snapshot.is_some(), |ui| {
                                if ui.button("Pin…").clicked() {
                                    action = Some(FwAction::AskPin);
                                }
                            });
                            if ui.button("Close").clicked() {
                                f.pin_for = None;
                            }
                        });
                    }

                    // 2. What is newer
                    ui.add_space(8.0);
                    ui.heading("Newer firmware");
                    ui.label(egui::RichText::new("Asks Nikon's Download Center what is newest for your models. Needs the network; reads the cameras' USB identity and your archive, writes nothing.").small().weak());
                    ui.add_enabled_ui(can_act && !camera_busy, |ui| {
                        if ui.button("Check for newer firmware").clicked() {
                            action = Some(FwAction::CheckLatest);
                        }
                    });
                    if let Some(view) = &f.latest {
                        draw_latest(ui, view, can_act, &mut action);
                    }

                    // 3. Getting a file
                    ui.add_space(8.0);
                    ui.heading("Get firmware");
                    ui.label(egui::RichText::new("Nikon's firmware is licensed. This program never accepts the licence for you and never downloads a licensed file itself: you accept it on Nikon's page and download the file, here in a window or in your own browser.").small().weak());
                    let models = known_models(cameras, f.archive.as_ref());
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Open Nikon's page for:");
                        for m in &models {
                            ui.add_enabled_ui(can_act, |ui| {
                                if ui.button(format!("{m}…")).clicked() {
                                    action = Some(FwAction::Browse(m.clone()));
                                }
                            });
                        }
                        if models.is_empty() {
                            ui.label(egui::RichText::new("(connect a camera or archive something first, or type a model below)").weak());
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Archive a file I downloaded").strong());
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(can_act, |ui| {
                            if ui.button("Choose file…").clicked() {
                                action = Some(FwAction::PickFile);
                            }
                        });
                        ui.label(f.add.file.as_ref().map_or("(no file chosen)".to_string(), |p| p.display().to_string()));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Model:");
                        ui.add(egui::TextEdit::singleline(&mut f.add.model).desired_width(100.0).hint_text("Z 9"));
                        egui::ComboBox::from_id_salt("fw_add_model").selected_text("pick").show_ui(ui, |ui| {
                            for m in &models {
                                if ui.selectable_label(f.add.model == *m, m).clicked() {
                                    f.add.model = m.clone();
                                }
                            }
                        });
                        ui.label("Version:");
                        ui.add(egui::TextEdit::singleline(&mut f.add.version).desired_width(70.0).hint_text("5.31"));
                    });
                    let refusal = add_refusal(f.add.file.as_deref(), &f.add.model, &f.add.version);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_enabled_ui(can_act && refusal.is_none(), |ui| {
                            if ui.button("Archive as this version…").clicked() {
                                action = Some(FwAction::AskAdd);
                            }
                        });
                        let can_fetch = can_act && f.add.file.is_some() && !f.add.model.trim().is_empty() && !camera_busy;
                        ui.add_enabled_ui(can_fetch, |ui| {
                            if ui.button("Archive as Nikon's newest (look up first)").on_hover_text("Looks up the newest release on Nikon's page (network) and shows what the file would be archived as before you confirm").clicked() {
                                action = Some(FwAction::AskFetchDryRun);
                            }
                        });
                    });
                    if let Some(why) = &refusal {
                        if f.add.file.is_some() {
                            ui.label(egui::RichText::new(why).small().weak());
                        }
                    }
                    if let Some((FwOp::FetchFile { file, model, .. }, text)) = &f.dry_run {
                        ui.separator();
                        ui.label(egui::RichText::new(format!("Look-up for {} as {model}:", file.display())).strong());
                        ui.label(egui::RichText::new(plain(text.trim_end())).monospace());
                        ui.add_enabled_ui(can_act, |ui| {
                            if ui.button("Archive it under that version…").clicked() {
                                action = Some(FwAction::AskFetchReal);
                            }
                        });
                    }
                });
            });
        self.firmware.open = open;
        if let Some(a) = action {
            self.firmware_act(ctx, a);
        }
    }

    fn firmware_act(&mut self, ctx: &egui::Context, action: FwAction) {
        let Some(data_dir) = self.settings.data_dir.clone() else {
            self.firmware.notice = "The data directory is unknown.".to_string();
            return;
        };
        let run = |app: &mut FleetApp, op: FwOp| {
            if let Err(e) = app.firmware.start(op, &data_dir, locate_cli(), ctx) {
                app.firmware.notice = e;
            }
        };
        match action {
            FwAction::Refresh => run(self, FwOp::Refresh),
            FwAction::CheckLatest => {
                let models = known_models(&self.cameras, self.firmware.archive.as_ref());
                run(self, FwOp::Latest { models });
            }
            FwAction::Browse(model) => {
                if let Some(op) = self.firmware.ask(FwOp::Browse { model }, &data_dir, None) {
                    run(self, op);
                }
            }
            FwAction::ChoosePin(model, version) => {
                self.firmware.pin_for = Some((model, version));
                self.firmware.pin_snapshot = None;
            }
            FwAction::AskPin => {
                if let (Some((model, version)), Some(snapshot)) = (self.firmware.pin_for.clone(), self.firmware.pin_snapshot.clone()) {
                    self.firmware.ask(FwOp::Pin { snapshot, model, version }, &data_dir, None);
                }
            }
            FwAction::Rollback(model, version) => {
                let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
                match rollback_dir(&data_dir, &model, &version, &now) {
                    Ok(output_dir) => {
                        self.firmware.ask(FwOp::Rollback { model, version, output_dir }, &data_dir, None);
                    }
                    Err(e) => self.firmware.notice = e,
                }
            }
            FwAction::PickFile => {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.firmware.add.file = Some(path);
                    self.firmware.dry_run = None;
                }
            }
            FwAction::AskAdd => {
                match self.firmware.add.op() {
                    Ok(op) => {
                        self.firmware.ask(op, &data_dir, None);
                    }
                    Err(why) => self.firmware.notice = why,
                }
            }
            FwAction::AskFetchDryRun => {
                if let Some(file) = self.firmware.add.file.clone() {
                    self.firmware.dry_run = None;
                    run(self, FwOp::FetchFile { file, model: self.firmware.add.model.trim().to_string(), dry_run: true });
                }
            }
            FwAction::AskFetchReal => {
                if let Some((FwOp::FetchFile { file, model, .. }, text)) = self.firmware.dry_run.clone() {
                    self.firmware.ask(FwOp::FetchFile { file, model, dry_run: false }, &data_dir, Some(&text));
                }
            }
            FwAction::Confirm => {
                if let Some(op) = self.firmware.confirm() {
                    run(self, op);
                }
            }
            FwAction::Cancel => self.firmware.cancel(),
            FwAction::CopyText(t) => ctx.copy_text(t),
        }
    }
}

fn draw_fw_status(ui: &mut egui::Ui, f: &FwUi) {
    if let Some(run) = &f.running {
        ui.colored_label(WARN_COLOR, format!("⏳ {}… {}s", run.op.title(), run.started.elapsed().as_secs()));
        if matches!(run.op, FwOp::Browse { .. }) {
            ui.label(egui::RichText::new("A window is open on Nikon's page. Accept the licence and download there; this program is waiting.").weak());
        }
    }
    if !f.notice.is_empty() {
        ui.colored_label(WARN_COLOR, &f.notice);
    }
    if let Some(n) = &f.note {
        colored(ui, n.level, &n.headline);
        if !n.detail.trim().is_empty() {
            egui::CollapsingHeader::new("Details").id_salt("fw_note_detail").show(ui, |ui| {
                ui.label(egui::RichText::new(plain(n.detail.trim_end())).monospace());
            });
        }
    }
}

fn draw_latest(ui: &mut egui::Ui, view: &LatestView, can_act: bool, action: &mut Option<FwAction>) {
    for m in &view.report.models {
        let (level, headline) = latest_headline(m);
        ui.separator();
        colored(ui, level, headline);
        if let Some(r) = &m.latest {
            // Plain text: the address is Nikon's page text, not something this program follows.
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(format!("Page: {}", printable(&r.page_url))).monospace().small());
                if ui.small_button("Copy address").clicked() {
                    *action = Some(FwAction::CopyText(printable(&r.page_url)));
                }
            });
            match r.download {
                LatestDownload::Gated => {
                    ui.label(egui::RichText::new("The download is behind Nikon's licence: you accept it on the page.").weak());
                }
                LatestDownload::Direct => {
                    ui.label(egui::RichText::new("Nikon lists a direct download. This program still does not fetch it for you: download it yourself and archive the file.").weak());
                }
            }
            if m.status == LatestStatus::NewAvailable {
                ui.add_enabled_ui(can_act, |ui| {
                    if ui.small_button(format!("Open Nikon's page for {} in a window…", printable(&m.model))).clicked() {
                        *action = Some(FwAction::Browse(m.model.clone()));
                    }
                });
            }
        }
        for c in &m.cameras {
            let (level, line) = latest_camera_line(c);
            colored(ui, level, line);
        }
        if m.cameras.is_empty() {
            ui.label(egui::RichText::new(if m.usb_scan.complete { "No body of this model is attached." } else { "No attached body was listed (the USB scan was not complete)." }).weak());
        }
        ui.label(egui::RichText::new(format!("Archived: {}", if m.archived_versions.is_empty() { "none".to_string() } else { m.archived_versions.iter().map(|v| printable(v)).collect::<Vec<_>>().join(", ") })).weak());
        for n in latest_notes(m) {
            ui.colored_label(WARN_COLOR, n);
        }
    }
    if !view.cli_log.trim().is_empty() {
        egui::CollapsingHeader::new("CLI messages").id_salt("fw_latest_log").show(ui, |ui| {
            ui.label(egui::RichText::new(plain(view.cli_log.trim_end())).monospace());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::tests::recording_cli;
    use crate::tests::{firmware_unit, row, write_snapshot};
    use nikon_fleet::ops::firmware_latest::{
        LatestArchiveUnreadable, LatestCamera, LatestModel, LatestRelease, LatestReport, LatestUsbScan,
    };
    use tempfile::TempDir;

    fn archive_with(entries: &[(&str, &str, &str)]) -> (TempDir, ArchiveView) {
        let d = TempDir::new().unwrap();
        for (model, slug, version) in entries {
            firmware_unit(d.path(), model, slug, version, format!("bin {model} {version}").as_bytes());
        }
        let view = archive_view(&scan_archives(d.path(), None));
        (d, view)
    }

    fn camera(model: &str, firmware: &str) -> CameraRow {
        let mut c = row(model, "S1", 1);
        c.firmware = firmware.into();
        c
    }

    // ── the archive, as listed ───────────────────────────────────────────

    #[test]
    fn the_archive_is_listed_by_the_librarys_scan_with_what_it_could_not_list() {
        let (d, view) = archive_with(&[("Z 9", "Z_9", "5.31"), ("Z 9", "Z_9", "5.30"), ("Z6_3", "Z6_3", "2.00")]);
        assert_eq!(view.rows.iter().map(|r| (r.model.as_str(), r.version.as_str())).collect::<Vec<_>>(), [("Z 9", "5.30"), ("Z 9", "5.31"), ("Z6_3", "2.00")], "model, then version");
        assert!(view.complete() && view.problems().is_empty());
        assert!(view.rows.iter().all(|r| r.bin_present && r.pinned.is_none() && r.size_bytes > 0));
        assert_eq!(view.models(), BTreeSet::from(["Z 9".to_string(), "Z6_3".to_string()]));
        // a corrupt entry is listed as unreadable, and makes "not archived" uncertain
        let bad = d.path().join("firmware/Z_9/5.40");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("metadata.json"), "{ not json").unwrap();
        let view = archive_view(&scan_archives(d.path(), None));
        assert_eq!(view.rows.len(), 3);
        assert_eq!(view.unreadable.len(), 1);
        assert_eq!(view.unreadable[0].kind, ArchiveSkipKind::MetadataCorrupt);
        assert!(!view.complete());
        assert!(view.problems()[0].contains("5.40") && view.problems()[0].contains("not valid"), "{:?}", view.problems());
        // an interrupted archive (a binary with no metadata)
        let half = d.path().join("firmware/Z_9/5.50");
        fs::create_dir_all(&half).unwrap();
        fs::write(half.join("firmware.bin"), b"x").unwrap();
        let view = archive_view(&scan_archives(d.path(), None));
        assert!(view.unreadable.iter().any(|u| u.kind == ArchiveSkipKind::IncompleteArchive));
        assert!(view.problems().iter().any(|p| p.contains("interrupted archive")));
        // no firmware directory at all
        let empty = TempDir::new().unwrap();
        let none = archive_view(&scan_archives(empty.path(), None));
        assert!(!none.root_exists && none.rows.is_empty() && none.complete());
    }

    #[test]
    fn whether_a_bodys_firmware_is_archived_is_never_guessed() {
        let (d, view) = archive_with(&[("Z 9", "Z_9", "5.31"), ("Z6_3", "Z6_3", "2.00")]);
        assert_eq!(running_archived(&view, &camera("Z 9", "5.31")), RunningArchived::Archived);
        assert_eq!(running_archived(&view, &camera("Z 9", "5.30")), RunningArchived::NotArchived, "one version below");
        assert_eq!(running_archived(&view, &camera("Z 9", "5.32")), RunningArchived::NotArchived, "one version above");
        assert_eq!(running_archived(&view, &camera("Z6_3", "5.31")), RunningArchived::NotArchived, "another model's entry does not count");
        // a firmware string that is not a version cannot be matched
        for odd in ["", "5.3", "5", "05.31", "five"] {
            assert!(matches!(running_archived(&view, &camera("Z 9", odd)), RunningArchived::Unknown(_)), "{odd:?}");
        }
        // an entry that could not be listed could be that version
        let bad = d.path().join("firmware/Z_9/5.40");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("metadata.json"), "{").unwrap();
        let incomplete = archive_view(&scan_archives(d.path(), None));
        assert_eq!(running_archived(&incomplete, &camera("Z 9", "5.31")), RunningArchived::Archived, "found is found");
        assert!(matches!(running_archived(&incomplete, &camera("Z 9", "5.30")), RunningArchived::Unknown(_)), "not found is only 'not found in what could be read'");
        // an entry of this model whose version is not a version number could be it
        let (_d2, odd) = archive_with(&[("Z 9", "Z_9", "5.3")]);
        assert!(matches!(running_archived(&odd, &camera("Z 9", "5.30")), RunningArchived::Unknown(_)));
        assert_eq!(running_archived(&odd, &camera("Z6_3", "2.00")), RunningArchived::NotArchived, "...but not for another model");
    }

    #[test]
    fn the_listing_of_snapshots_carries_the_firmware_each_was_taken_on() {
        let d = TempDir::new().unwrap();
        write_snapshot(&d.path().join("snapshots"), "a.json", "Z 9", "1", "2026-10-27T10:00:00Z");
        let listing = list_all(d.path());
        assert_eq!(listing.rows.len(), 1);
        assert_eq!(listing.rows[0].firmware, "5.00");
    }

    #[test]
    fn the_models_offered_are_the_connected_ones_and_the_archives_without_repeats() {
        let (_d, view) = archive_with(&[("Z 9", "Z_9", "5.31"), ("Z6_3", "Z6_3", "2.00")]);
        assert_eq!(known_models(&[camera("Z 9", "5.31"), camera("Z 8", "1.00")], Some(&view)), ["Z 8", "Z 9", "Z6_3"]);
        assert!(known_models(&[], None).is_empty());
    }

    // ── `firmware latest --json`, read ───────────────────────────────────

    fn latest_model(status: LatestStatus, cameras: Vec<LatestCamera>) -> LatestModel {
        LatestModel {
            model: "Z 9".into(),
            status,
            latest: (status != LatestStatus::CouldNotDetermine).then(|| LatestRelease {
                version: "5.40".into(),
                released: Some("2026-10-01".into()),
                page_url: "https://example.test/z9".into(),
                download: LatestDownload::Gated,
            }),
            reason: (status == LatestStatus::CouldNotDetermine).then(|| "the page layout changed".to_string()),
            archive_holds_latest: (status != LatestStatus::CouldNotDetermine).then_some(status == LatestStatus::AlreadyArchived),
            archived_versions: vec!["5.31".into()],
            archive_unreadable: vec![],
            archive_unreadable_dir_entries: 0,
            cameras,
            usb_scan: LatestUsbScan { complete: true, enumeration_error: None, unreadable: vec![] },
            warnings: vec![],
        }
    }

    fn camera_json(relation: LatestRelation, archived: Option<bool>) -> LatestCamera {
        LatestCamera { serial: "S1".into(), installed: "5.31".into(), relation, relation_detail: None, running_version_archived: archived }
    }

    fn model_json(m: &LatestModel) -> LatestModelJson {
        // LatestModel is Serialize only: round-trip through the value the command prints.
        let value = serde_json::to_value(&LatestReport { models: vec![] }).unwrap();
        assert_eq!(value, serde_json::json!({"models": []}));
        let value = serde_json::json!({"models": [serde_json::to_value(m).unwrap()]});
        serde_json::from_value::<LatestReportJson>(value).expect("the library's survey parses").models.remove(0)
    }

    #[test]
    fn every_status_relation_and_download_the_library_can_print_is_read_and_worded() {
        for status in [LatestStatus::NewAvailable, LatestStatus::AlreadyArchived, LatestStatus::CouldNotDetermine] {
            let m = model_json(&latest_model(status, vec![]));
            assert_eq!(m.status, status);
            let (level, text) = latest_headline(&m);
            match status {
                LatestStatus::NewAvailable => assert!(level == Level::Warn && text.contains("5.40") && text.contains("NOT in your archive"), "{text}"),
                LatestStatus::AlreadyArchived => assert!(level == Level::Info && text.contains("already in your archive"), "{text}"),
                LatestStatus::CouldNotDetermine => {
                    assert!(level == Level::Warn && text.contains("could not be determined") && text.contains("says nothing about whether newer firmware exists"), "{text}");
                    assert!(text.contains("the page layout changed"), "{text}");
                }
            }
        }
        let mut seen = BTreeSet::new();
        for rel in [LatestRelation::Behind, LatestRelation::Current, LatestRelation::Ahead, LatestRelation::Unreadable, LatestRelation::LatestUnknown] {
            let m = model_json(&latest_model(LatestStatus::NewAvailable, vec![camera_json(rel, Some(true))]));
            let (level, line) = latest_camera_line(&m.cameras[0]);
            assert_eq!(level == Level::Info, rel == LatestRelation::Current, "only 'at the newest' is plain: {line}");
            seen.insert(line);
        }
        assert_eq!(seen.len(), 5, "five different sentences");
        let junk = serde_json::from_value::<LatestReportJson>(serde_json::json!({"models": [{"model": "Z 9", "status": "a_new_status"}]}));
        assert!(junk.is_err(), "a verdict the GUI does not know fails loudly");
    }

    #[test]
    fn whether_the_running_version_is_archived_is_yes_no_or_unknown() {
        let line = |a| {
            let m = model_json(&latest_model(LatestStatus::AlreadyArchived, vec![camera_json(LatestRelation::Behind, a)]));
            latest_camera_line(&m.cameras[0]).1
        };
        assert!(line(Some(true)).contains("is archived"), "{}", line(Some(true)));
        assert!(line(Some(false)).contains("NOT archived"));
        assert!(line(None).contains("unknown"));
    }

    #[test]
    fn what_could_not_be_scanned_or_listed_is_said_before_a_negative_is_believed() {
        let mut m = latest_model(LatestStatus::NewAvailable, vec![]);
        assert!(latest_notes(&model_json(&m)).is_empty());
        m.usb_scan = LatestUsbScan { complete: false, enumeration_error: Some("libusb: no access".into()), unreadable: vec![] };
        m.archive_unreadable = vec![LatestArchiveUnreadable { directory: "/d/firmware/Z_9/5.5".into(), error_kind: ArchiveSkipKind::MetadataCorrupt, error: "bad".into() }];
        m.archive_unreadable_dir_entries = 2;
        m.warnings = vec!["the page lists two releases".into()];
        let notes = latest_notes(&model_json(&m));
        for needle in ["USB bus could not be listed", "5.5", "not valid", "2 archive director", "two releases"] {
            assert!(notes.iter().any(|n| n.contains(needle)), "{needle:?} in {notes:?}");
        }
        m.usb_scan = LatestUsbScan { complete: false, enumeration_error: None, unreadable: vec![nikon_fleet::sdk::UsbUnreadable { bus: 3, address: 7, stage: nikon_fleet::sdk::UsbStage::Open, error: "Access denied".into() }] };
        let notes = latest_notes(&model_json(&m));
        assert!(notes.iter().any(|n| n.contains("incomplete")) && notes.iter().any(|n| n.contains("1 Nikon USB device")), "{notes:?}");
    }

    #[test]
    fn text_from_nikons_page_is_shown_as_text_and_cannot_forge_a_line_or_a_terminal_sequence() {
        let mut m = latest_model(LatestStatus::CouldNotDetermine, vec![]);
        m.reason = Some("oops\n\u{1b}[31m[click](https://evil.test) and <b>bold</b>\u{7}".into());
        let (_, text) = latest_headline(&model_json(&m));
        assert!(!text.contains('\u{1b}') && !text.contains('\n') && !text.contains('\u{7}'), "{text:?}");
        assert!(text.contains("[click](https://evil.test)") && text.contains("<b>bold</b>"), "markup is kept as the characters it is: {text}");
        assert_eq!(plain("a\nb\u{1b}c"), "a\nb\\u{1b}c", "lines stay lines; control characters are escaped");
        let mut with_url = latest_model(LatestStatus::NewAvailable, vec![]);
        with_url.warnings = vec!["see\u{1b}]8;;http://evil.test\u{7}x".into()];
        assert!(!latest_notes(&model_json(&with_url))[0].contains('\u{1b}'));
    }

    // ── what is run ──────────────────────────────────────────────────────

    #[test]
    fn each_operation_is_one_command_whose_arguments_cannot_be_taken_for_flags() {
        let d = Path::new("/data");
        let a = |op: FwOp| op.argv(d).unwrap().unwrap();
        assert_eq!(FwOp::Refresh.argv(d).unwrap(), None, "listing the archive is not a process");
        assert_eq!(a(FwOp::Latest { models: vec![] }), ["--data-dir=/data", "firmware", "latest", "--json"], "no model: the command's own default");
        assert_eq!(
            a(FwOp::Latest { models: vec!["Z 9".into(), "Z6_3".into()] }),
            ["--data-dir=/data", "firmware", "latest", "--json", "--model=Z 9", "--model=Z6_3"]
        );
        assert_eq!(a(FwOp::Browse { model: "-Z".into() }), ["--data-dir=/data", "firmware", "browse", "--", "-Z"]);
        let add = a(FwOp::Add { file: PathBuf::from("-fw.bin"), model: "Z 9".into(), version: "5.31".into() });
        assert_eq!(add, ["--data-dir=/data", "firmware", "add", "--model=Z 9", "--version=5.31", "--", "-fw.bin"]);
        let dry = a(FwOp::FetchFile { file: PathBuf::from("/f/fw.bin"), model: "Z 9".into(), dry_run: true });
        let real = a(FwOp::FetchFile { file: PathBuf::from("/f/fw.bin"), model: "Z 9".into(), dry_run: false });
        assert!(dry.contains(&"--dry-run".to_string()) && !real.contains(&"--dry-run".to_string()));
        for fetch in [&dry, &real] {
            assert!(fetch.contains(&"--from-file=/f/fw.bin".to_string()), "a fetch always names the file: this window never lets the command download: {fetch:?}");
            assert!(fetch.contains(&"--model=Z 9".to_string()));
        }
        let pin = a(FwOp::Pin { snapshot: "snap.json".into(), model: "Z 9".into(), version: "5.31".into() });
        assert!(Path::new(pin.last().unwrap()).is_absolute() && pin.last().unwrap().ends_with("snap.json"), "{pin:?}");
        assert_eq!(pin[pin.len() - 2], "--");
        let rb = a(FwOp::Rollback { model: "Z 9".into(), version: "5.31".into(), output_dir: PathBuf::from("/data/rollback-bundles/x") });
        assert!(rb.contains(&"--output-dir=/data/rollback-bundles/x".to_string()));
        assert!(!rb.iter().any(|x| x.starts_with("--serial")), "a bundle needs no body: nothing here reaches a camera");
        for argv in [&add, &dry, &real, &pin, &rb] {
            assert!(!argv.iter().any(|x| x.contains("force")), "replacing an archived entry is not offered: {argv:?}");
        }
        let bad = FwOp::Pin { snapshot: "../x.json".into(), model: "Z 9".into(), version: "5.31".into() };
        assert!(bad.argv(d).is_err(), "a path is not a snapshot name");
    }

    #[test]
    fn only_the_operations_that_change_something_ask_first() {
        let all = [
            (FwOp::Refresh, false),
            (FwOp::Latest { models: vec![] }, false),
            (FwOp::FetchFile { file: "f".into(), model: "Z 9".into(), dry_run: true }, false),
            (FwOp::FetchFile { file: "f".into(), model: "Z 9".into(), dry_run: false }, true),
            (FwOp::Browse { model: "Z 9".into() }, true),
            (FwOp::Add { file: "f".into(), model: "Z 9".into(), version: "5.31".into() }, true),
            (FwOp::Pin { snapshot: "s.json".into(), model: "Z 9".into(), version: "5.31".into() }, true),
            (FwOp::Rollback { model: "Z 9".into(), version: "5.31".into(), output_dir: "/o".into() }, true),
        ];
        for (op, writes) in all {
            assert_eq!(op.writes_locally(), writes, "{op:?}");
            assert_eq!(confirm_text(&op, Path::new("/data"), None, None).is_some(), writes, "{op:?}");
        }
        assert!(FwOp::Browse { model: "Z 9".into() }.timeout() >= Duration::from_secs(15 * 60), "a person reading a licence takes minutes");
        assert!(FwOp::Latest { models: vec![] }.timeout() < FwOp::Browse { model: "Z 9".into() }.timeout());
    }

    #[test]
    fn a_rollback_bundle_goes_to_an_absolute_plain_directory_and_a_hostile_model_or_version_is_refused() {
        let dir = rollback_dir(Path::new("/data"), "Z 9", "5.31", "2026-10-29T10:15:00.5Z").unwrap();
        assert!(dir.is_absolute() && dir.ends_with("rollback-bundles/Z_9_5.31_20261029T101500.5Z".replace('/', std::path::MAIN_SEPARATOR_STR)), "{dir:?}");
        assert!(rollback_dir(Path::new("/data"), "../Z", "5.31", "t").is_err());
        assert!(rollback_dir(Path::new("/data"), "Z 9", "../5.31", "t").is_err());
        assert!(rollback_dir(Path::new("/data"), "Z 9", "5.31", "a/b").is_err());
    }

    // ── how a write ended ────────────────────────────────────────────────

    fn failure(kind: CliFailure, code: Option<i32>) -> Result<CliOutput, CliError> {
        Err(CliError { kind, message: "m".into(), stdout: "out".into(), stderr: "err".into(), code })
    }

    #[test]
    fn already_archived_is_the_clis_status_three_for_the_commands_that_document_it_and_nothing_else() {
        let add = FwOp::Add { file: "f".into(), model: "Z 9".into(), version: "5.31".into() };
        let fetch = FwOp::FetchFile { file: "f".into(), model: "Z 9".into(), dry_run: false };
        let browse = FwOp::Browse { model: "Z 9".into() };
        for op in [&add, &fetch, &browse] {
            assert!(matches!(classify_write(op, failure(CliFailure::Exit, Some(3))), WriteEnd::NothingChanged { .. }), "{op:?}");
        }
        for op in [FwOp::Pin { snapshot: "s".into(), model: "Z 9".into(), version: "5.31".into() }, FwOp::Rollback { model: "Z 9".into(), version: "5.31".into(), output_dir: "/o".into() }] {
            assert!(matches!(classify_write(&op, failure(CliFailure::Exit, Some(3))), WriteEnd::Failed { .. }), "exit 3 means nothing for {op:?}");
        }
        // a message that says "already archived" does not make it so
        let mut e = CliError { kind: CliFailure::Exit, message: "already archived".into(), stdout: String::new(), stderr: String::new(), code: Some(1) };
        assert!(matches!(classify_write(&add, Err(e.clone())), WriteEnd::Failed { how: FailedHow::Refused, .. }));
        e.code = Some(3);
        assert!(matches!(classify_write(&add, Err(e)), WriteEnd::NothingChanged { .. }));
        assert!(matches!(classify_write(&add, Ok(CliOutput { stdout: "s".into(), stderr: String::new() })), WriteEnd::Done { .. }));
    }

    #[test]
    fn a_failed_write_is_refused_only_by_the_clis_own_statuses_and_anything_else_may_be_half_done() {
        let add = FwOp::Add { file: "f".into(), model: "Z 9".into(), version: "5.31".into() };
        let how = |r| match classify_write(&add, r) {
            WriteEnd::Failed { how, .. } => how,
            other => panic!("{other:?}"),
        };
        for r in [failure(CliFailure::Spawn, None), failure(CliFailure::Exit, Some(1)), failure(CliFailure::Exit, Some(2))] {
            assert_eq!(how(r), FailedHow::Refused);
        }
        for r in [
            failure(CliFailure::TimedOut, None),
            failure(CliFailure::Wait, None),
            failure(CliFailure::StdinWrite, None),
            failure(CliFailure::Exit, None),
            failure(CliFailure::Exit, Some(101)),
            failure(CliFailure::Exit, Some(4)),
            failure(CliFailure::Exit, Some(0)),
        ] {
            assert_eq!(how(r), FailedHow::Stopped);
        }
        let stopped = write_note(&add, WriteEnd::Failed { message: "m".into(), stdout: String::new(), stderr: String::new(), how: FailedHow::Stopped });
        assert_eq!(stopped.level, Level::Error);
        assert!(stopped.headline.contains("partly written entry is possible"), "{}", stopped.headline);
        let refused = write_note(&add, WriteEnd::Failed { message: "m".into(), stdout: String::new(), stderr: String::new(), how: FailedHow::Refused });
        assert!(!refused.headline.contains("partly written"), "{}", refused.headline);
    }

    // ── executing, with a stand-in for the CLI ───────────────────────────

    #[cfg(unix)]
    #[test]
    fn a_lookup_that_printed_its_survey_before_failing_is_shown_and_one_that_did_not_is_an_error() {
        let dir = TempDir::new().unwrap();
        let survey = serde_json::json!({"models": [serde_json::to_value(latest_model(LatestStatus::CouldNotDetermine, vec![])).unwrap()]});
        fs::write(dir.path().join("s.json"), survey.to_string()).unwrap();
        let op = FwOp::Latest { models: vec!["Z 9".into()] };
        let cli = Ok(recording_cli(dir.path(), &format!("cat '{}/s.json'\necho 'warn' >&2\nexit 1", dir.path().display())));
        let FwResult::Latest(Ok(view)) = execute(&cli, Path::new("/data"), &op) else { panic!("a survey") };
        assert_eq!(view.report.models[0].status, LatestStatus::CouldNotDetermine, "the model that could not be determined says so itself");
        assert_eq!(view.cli_log, "warn");
        let cli = Ok(recording_cli(dir.path(), "echo 'no network' >&2\nexit 1"));
        let FwResult::Latest(Err(e)) = execute(&cli, Path::new("/data"), &op) else { panic!("an error") };
        assert!(e.contains("no network"), "{e}");
        let cli = Ok(recording_cli(dir.path(), "echo '{}'"));
        assert!(matches!(execute(&cli, Path::new("/data"), &op), FwResult::Latest(Err(_))));
        let missing: Result<PathBuf, String> = Err("the CLI was not found".into());
        let FwResult::Latest(Err(e)) = execute(&missing, Path::new("/data"), &op) else { panic!() };
        assert!(e.contains("not found"));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_runs_the_command_with_the_arguments_that_were_confirmed() {
        let dir = TempDir::new().unwrap();
        let cli = Ok(recording_cli(dir.path(), "echo 'Archived Z 9 firmware 5.31'"));
        let op = FwOp::Add { file: PathBuf::from("/f/fw.bin"), model: "Z 9".into(), version: "5.31".into() };
        let FwResult::Write(WriteEnd::Done { stdout, .. }) = execute(&cli, Path::new("/data"), &op) else { panic!() };
        assert!(stdout.contains("Archived"));
        assert_eq!(crate::tests::argv_of(dir.path()), op.argv(Path::new("/data")).unwrap().unwrap());
        let nothing = Ok(recording_cli(dir.path(), "echo 'already archived' >&2\nexit 3"));
        assert!(matches!(execute(&nothing, Path::new("/data"), &op), FwResult::Write(WriteEnd::NothingChanged { .. })));
    }

    // ── what the person is asked ─────────────────────────────────────────

    #[test]
    fn each_question_says_what_is_written_where_and_that_no_camera_is_touched() {
        let (d, view) = archive_with(&[("Z 9", "Z_9", "5.31")]);
        let file = d.path().join("fw.bin");
        fs::write(&file, vec![0u8; 3 * 1024 * 1024]).unwrap();
        let add = confirm_text(&FwOp::Add { file: file.clone(), model: "Z 9".into(), version: "5.40".into() }, d.path(), Some(&view), None).unwrap();
        for needle in ["fw.bin", "3.0 MiB", "Z 9 firmware 5.40", "SHA-256", "the version is the one you typed", "No camera is touched"] {
            assert!(add.contains(needle), "{needle:?} missing from:\n{add}");
        }
        assert!(add.contains(&d.path().join("firmware").join("Z_9").join("5.40").display().to_string()), "{add}");
        assert!(!add.contains("ALREADY archived"));
        let again = confirm_text(&FwOp::Add { file, model: "Z 9".into(), version: "5.31".into() }, d.path(), Some(&view), None).unwrap();
        assert!(again.contains("ALREADY archived") && again.contains("nothing will change"), "{again}");

        let rb = confirm_text(&FwOp::Rollback { model: "Z 9".into(), version: "5.31".into(), output_dir: d.path().join("rollback-bundles/x") }, d.path(), Some(&view), None).unwrap();
        assert!(rb.contains("does NOT flash or write to any camera") && rb.contains("SD card") && rb.contains("rollback-bundles"), "{rb}");

        let browse = confirm_text(&FwOp::Browse { model: "Z 9".into() }, d.path(), None, None).unwrap();
        assert!(browse.contains("accept Nikon's licence and press Download yourself") && browse.contains("never accepts it"), "{browse}");

        let fetch = confirm_text(&FwOp::FetchFile { file: "fw.bin".into(), model: "Z 9".into(), dry_run: false }, d.path(), None, Some("would archive as Z 9 5.40\u{1b}[0m")).unwrap();
        assert!(fetch.contains("cannot tell what version the file is") && fetch.contains("would archive as Z 9 5.40"), "{fetch}");
        assert!(!fetch.contains('\u{1b}'));
    }

    #[test]
    fn a_pin_question_says_what_it_replaces_and_a_snapshot_of_other_firmware_is_warned_about() {
        let (d, mut view) = archive_with(&[("Z 9", "Z_9", "5.31")]);
        let op = FwOp::Pin { snapshot: "snap.json".into(), model: "Z 9".into(), version: "5.31".into() };
        let first = confirm_text(&op, d.path(), Some(&view), None).unwrap();
        assert!(first.contains("Nothing is pinned for it now") && first.contains("no camera is touched"), "{first}");
        view.rows[0].pinned = Some("snapshots/old.json".into());
        let second = confirm_text(&op, d.path(), Some(&view), None).unwrap();
        assert!(second.contains("currently points to snapshots/old.json") && second.contains("replaced"), "{second}");
        assert!(confirm_text(&op, d.path(), None, None).unwrap().contains("not in the listing shown"));
        assert_eq!(pin_mismatch("5.31", "5.31"), None);
        assert_eq!(pin_mismatch("", "5.31"), None, "a snapshot with no firmware recorded says nothing either way");
        assert!(pin_mismatch("5.30", "5.31").unwrap().contains("5.30"));
    }

    #[test]
    fn a_file_is_archived_only_as_a_real_file_a_plain_model_and_a_version_in_the_shape_a_camera_reports() {
        let d = TempDir::new().unwrap();
        let file = d.path().join("fw.bin");
        fs::write(&file, b"x").unwrap();
        let ok = |model: &str, version: &str| add_refusal(Some(&file), model, version);
        assert_eq!(ok("Z 9", "5.31"), None);
        assert_eq!(ok(" Z 9 ", " 5.31 "), None, "surrounding spaces are not part of either");
        assert_eq!(ok("Z 9", "10.00"), None);
        assert!(add_refusal(None, "Z 9", "5.31").unwrap().contains("Choose"));
        assert!(add_refusal(Some(d.path()), "Z 9", "5.31").unwrap().contains("not a file"));
        assert!(add_refusal(Some(&d.path().join("missing.bin")), "Z 9", "5.31").unwrap().contains("cannot be read"));
        assert!(ok("", "5.31").unwrap().contains("model"));
        assert!(ok("  ", "5.31").unwrap().contains("model"));
        assert!(ok("../Z", "5.31").unwrap().contains("plain directory"));
        for version in ["", "5", "5.3", "5.301", "05.31", "5.31.1", "v5.31", "../5.31", "5,31"] {
            assert!(ok("Z 9", version).unwrap().contains("not a firmware version"), "{version:?}");
        }
    }

    #[test]
    fn the_add_form_asks_for_the_model_and_version_as_they_will_be_stored() {
        let d = TempDir::new().unwrap();
        let file = d.path().join("fw.bin");
        fs::write(&file, b"x").unwrap();
        let form = AddForm { file: Some(file.clone()), model: " Z 9 ".into(), version: " 5.31\t".into() };
        assert_eq!(form.op(), Ok(FwOp::Add { file: file.clone(), model: "Z 9".into(), version: "5.31".into() }));
        let bad = AddForm { file: Some(file), model: "Z 9".into(), version: "5.3".into() };
        assert!(bad.op().unwrap_err().contains("not a firmware version"));
        assert!(AddForm::default().op().is_err());
    }

    #[test]
    fn a_directory_that_could_not_be_read_makes_the_archive_incomplete_whatever_was_listed() {
        let mut view = ArchiveView { root: "/d/firmware".into(), root_exists: true, ..ArchiveView::default() };
        assert!(view.complete() && view.problems().is_empty());
        view.skipped_dir_entries = 1;
        assert!(!view.complete());
        assert!(view.problems()[0].contains("1 directory under /d/firmware could not be read and was not listed"), "{:?}", view.problems());
        view.skipped_dir_entries = 2;
        assert!(view.problems()[0].contains("2 directories") && view.problems()[0].contains("were not listed"), "{:?}", view.problems());
        assert!(matches!(running_archived(&view, &camera("Z 9", "5.31")), RunningArchived::Unknown(_)), "nothing listed, but something may be unlisted");
    }

    // ── the window's state ───────────────────────────────────────────────

    fn answer(op: FwOp, result: FwResult) -> FwAnswer {
        FwAnswer { op, result }
    }

    fn pin_op() -> FwOp {
        FwOp::Pin { snapshot: "s.json".into(), model: "Z 9".into(), version: "5.31".into() }
    }

    #[test]
    fn an_operation_that_changes_something_waits_for_a_yes_and_runs_exactly_what_was_asked() {
        let mut f = FwUi::default();
        let d = Path::new("/data");
        assert_eq!(f.ask(FwOp::Refresh, d, None), Some(FwOp::Refresh), "a read needs no question");
        assert!(f.ask(pin_op(), d, None).is_none() && f.pending.as_ref().unwrap().op == pin_op());
        assert!(f.pending.as_ref().unwrap().text.contains("5.31"));
        f.cancel();
        assert!(f.pending.is_none() && f.confirm().is_none(), "cancelled: nothing to confirm");
        f.ask(pin_op(), d, None);
        assert_eq!(f.confirm(), Some(pin_op()));
        assert!(f.confirm().is_none(), "a yes is used once");
        // asking again replaces the earlier question, never adds to it
        f.ask(pin_op(), d, None);
        f.ask(FwOp::Browse { model: "Z 8".into() }, d, None);
        assert_eq!(f.confirm(), Some(FwOp::Browse { model: "Z 8".into() }));
    }

    #[cfg(unix)]
    #[test]
    fn nothing_starts_while_an_operation_runs_and_a_pending_yes_waits_for_it() {
        let ctx = egui::Context::default();
        let dir = TempDir::new().unwrap();
        let mut f = FwUi::default();
        let slow = recording_cli(dir.path(), "sleep 2");
        f.start(FwOp::Latest { models: vec![] }, dir.path(), Ok(slow), &ctx).unwrap();
        assert!(f.busy());
        assert!(f.start(FwOp::Refresh, dir.path(), Err("x".into()), &ctx).unwrap_err().contains("still running"));
        f.ask(pin_op(), dir.path(), None);
        assert!(f.confirm().is_none(), "no second operation while one runs");
        assert!(f.pending.is_some(), "and the question is still there");
    }

    #[cfg(unix)]
    #[test]
    fn an_operation_runs_on_its_own_thread_and_its_answer_is_collected() {
        let ctx = egui::Context::default();
        let dir = TempDir::new().unwrap();
        let mut f = FwUi::default();
        let cli = recording_cli(dir.path(), "echo 'Pinned Z 9 5.31'");
        f.start(pin_op(), dir.path(), Ok(cli), &ctx).unwrap();
        let t0 = Instant::now();
        while f.busy() && t0.elapsed() < Duration::from_secs(20) {
            f.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!f.busy(), "the answer arrived");
        let note = f.note.as_ref().expect("a note");
        assert_eq!(note.level, Level::Info);
        assert!(note.detail.contains("Pinned"));
        assert!(f.relist, "the archive changed: it is listed again");
    }

    #[test]
    fn a_thread_that_died_without_answering_is_a_stopped_write_and_a_stopped_read_is_harmless() {
        for (op, level, relist) in [(pin_op(), Level::Error, true), (FwOp::Latest { models: vec![] }, Level::Warn, false)] {
            let mut f = FwUi::default();
            let (tx, rx) = mpsc::channel::<FwAnswer>();
            f.running = Some(FwRun { op, rx, started: Instant::now() });
            f.poll();
            assert!(f.running.is_some(), "no answer yet, but the thread is alive");
            drop(tx);
            f.poll();
            assert!(f.running.is_none(), "never stuck 'running'");
            let note = f.note.as_ref().unwrap();
            assert_eq!((note.level, f.relist), (level, relist));
            assert!(note.headline.contains("stopped without an answer"));
            assert_eq!(note.detail.contains("may have changed"), relist, "{}", note.detail);
        }
    }

    #[test]
    fn a_write_that_finished_lists_the_archive_again_and_a_look_up_does_not() {
        let mut f = FwUi::default();
        f.finish(answer(pin_op(), FwResult::Write(WriteEnd::Done { stdout: "ok".into(), stderr: String::new() })));
        assert!(f.relist && f.note.as_ref().unwrap().level == Level::Info);
        let mut f = FwUi::default();
        f.finish(answer(FwOp::Latest { models: vec![] }, FwResult::Latest(Err("no network".into()))));
        assert!(!f.relist);
        assert_eq!(f.note.as_ref().unwrap().level, Level::Error);
        // a failed write relists too: what is on disk is the truth, whatever the command said
        let mut f = FwUi::default();
        f.finish(answer(pin_op(), FwResult::Write(WriteEnd::Failed { message: "m".into(), stdout: String::new(), stderr: String::new(), how: FailedHow::Stopped })));
        assert!(f.relist);
        // a listing answers a relist
        f.finish(answer(FwOp::Refresh, FwResult::Archive(ArchiveView::default())));
        assert!(!f.relist && f.archive.is_some());
    }

    #[test]
    fn a_dry_run_is_kept_for_the_question_about_the_real_run_and_writes_nothing() {
        let mut f = FwUi::default();
        let dry = FwOp::FetchFile { file: "fw.bin".into(), model: "Z 9".into(), dry_run: true };
        f.finish(answer(dry.clone(), FwResult::Write(WriteEnd::Done { stdout: "dry-run: would archive fw.bin as Z 9 5.40".into(), stderr: String::new() })));
        assert!(!f.relist, "a dry run changed nothing");
        let (op, text) = f.dry_run.clone().expect("kept");
        assert_eq!(op, dry);
        assert!(text.contains("5.40"));
        let q = f.ask(FwOp::FetchFile { file: "fw.bin".into(), model: "Z 9".into(), dry_run: false }, Path::new("/data"), Some(&text));
        assert!(q.is_none() && f.pending.as_ref().unwrap().text.contains("5.40"));
    }

    #[test]
    fn a_change_of_data_directory_withdraws_the_question_and_what_was_listed_from_the_old_one() {
        let mut f = FwUi::default();
        f.archive = Some(ArchiveView::default());
        f.latest = Some(LatestView { report: LatestReportJson { models: vec![] }, cli_log: String::new() });
        f.pin_for = Some(("Z 9".into(), "5.31".into()));
        f.ask(pin_op(), Path::new("/old"), None);
        f.data_dir_changed();
        assert!(f.pending.is_none() && f.archive.is_none() && f.latest.is_none() && f.pin_for.is_none());
        assert!(f.relist);
        assert!(f.confirm().is_none());
    }

    #[test]
    fn the_app_lists_the_archive_again_when_it_is_told_the_archive_changed_and_only_when_idle() {
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let ctx = egui::Context::default();
        firmware_unit(dir.path(), "Z 9", "Z_9", "5.31", b"bin");
        app.firmware.relist = true;
        // something else is running: the listing waits
        let (_tx, rx) = mpsc::channel::<FwAnswer>();
        app.firmware.running = Some(FwRun { op: FwOp::Latest { models: vec![] }, rx, started: Instant::now() });
        app.poll_firmware(&ctx);
        assert!(app.firmware.relist && app.firmware.archive.is_none(), "not while another operation runs");
        app.firmware.running = None;
        app.poll_firmware(&ctx);
        assert!(!app.firmware.relist);
        let t0 = Instant::now();
        while app.firmware.archive.is_none() && t0.elapsed() < Duration::from_secs(20) {
            app.poll_firmware(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        let listed = app.firmware.archive.as_ref().expect("the archive was listed");
        assert!(listed.holds("Z 9", "5.31"));
        assert!(!app.firmware.busy());
    }

    // ── drawn ────────────────────────────────────────────────────────────

    #[test]
    fn every_state_of_the_window_draws() {
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let ctx = egui::Context::default();
        let (_d, view) = archive_with(&[("Z 9", "Z_9", "5.31")]);
        let mut draw = |app: &mut FleetApp| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.ui_firmware(ctx));
        };
        app.firmware.open = true;
        draw(&mut app); // listing not read yet
        app.firmware.archive = Some(view.clone());
        draw(&mut app);
        let mut broken = view;
        broken.unreadable.push(ArchiveUnreadable { directory: "d".into(), kind: ArchiveSkipKind::MetadataCorrupt, error: "e".into() });
        broken.rows[0].bin_present = false;
        app.firmware.archive = Some(broken);
        app.firmware.latest = Some(LatestView {
            report: LatestReportJson {
                models: [LatestStatus::NewAvailable, LatestStatus::AlreadyArchived, LatestStatus::CouldNotDetermine]
                    .into_iter()
                    .map(|s| model_json(&latest_model(s, vec![camera_json(LatestRelation::Behind, None)])))
                    .collect(),
            },
            cli_log: "log".into(),
        });
        app.firmware.pin_for = Some(("Z 9".into(), "5.31".into()));
        app.firmware.note = Some(FwNote { level: Level::Warn, headline: "h".into(), detail: "d\nd".into() });
        app.firmware.dry_run = Some((FwOp::FetchFile { file: "f".into(), model: "Z 9".into(), dry_run: true }, "text".into()));
        app.firmware.add.file = Some(dir.path().join("fw.bin"));
        draw(&mut app);
        app.firmware.ask(pin_op(), dir.path(), None);
        draw(&mut app);
        let (_tx, rx) = mpsc::channel();
        app.firmware.running = Some(FwRun { op: FwOp::Browse { model: "Z 9".into() }, rx, started: Instant::now() });
        draw(&mut app);
    }
}
