//! The shape of `firmware latest --json`: one object per watched model, with
//! every verdict a closed snake_case enum. A contract with scripts, the GUI and
//! (planned) the MCP, so it lives in the library rather than in `main.rs`.

use serde::{Deserialize, Serialize};

use std::path::Path;

use crate::firmware::{ArchiveSkipKind, scan_archives};
use crate::ops::cameras::describe_scan;
use crate::firmware_source::{
    CameraRelation, DownloadAccess, FirmwareSource, FwVersion, HttpGet, ModelStatus, Release, SourceError, assess,
    camera_relation,
};
use crate::sdk::{UsbScan, UsbUnreadable, usb_camera_scan};

/// Everything we know about one model before deciding what to do.
pub struct ModelSurvey {
    pub model: String,
    pub release: Result<Release, SourceError>,
    pub archived: Vec<FwVersion>,
    /// Archive entries whose version string isn't canonical and so can't be matched.
    pub archive_notes: Vec<String>,
    /// (serial, reported firmware) for connected bodies of this model.
    pub cameras: Vec<(String, String)>,
    /// Archive directories for this model that could not be listed as entries, and why. Their
    /// versions are unknown, so "not archived" cannot be concluded while any exist.
    pub archive_unreadable: Vec<LatestArchiveUnreadable>,
    /// Directories under `firmware/` the OS failed to read; they may hold any model's entry.
    pub archive_unreadable_dir_entries: usize,
    /// How the USB pass that produced `cameras` went.
    pub usb_scan: LatestUsbScan,
    pub status: ModelStatus,
}

// ── `firmware latest --json` ────────────────────────────────────────────────
// The shape is a contract with scripts and the GUI; `latest_json_shape_*`
// tests pin it. Every verdict is a closed enum serialised as snake_case; the
// free-text `reason`/`detail` fields only explain a verdict, never carry it.

/// Source verdict for one model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LatestStatus {
    /// Published and not in the archive.
    NewAvailable,
    /// Published and already in the archive.
    AlreadyArchived,
    /// The source could not be read; says nothing about whether anything is new.
    CouldNotDetermine,
}

/// How one connected camera relates to the latest published version.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LatestRelation {
    Behind,
    Current,
    Ahead,
    /// The camera's reported firmware isn't a canonical version.
    Unreadable,
    /// There is no latest version to compare with (the lookup failed).
    LatestUnknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LatestDownload {
    Gated,
    Direct,
}

#[derive(Serialize, Debug)]
pub struct LatestRelease {
    pub version: String,
    pub released: Option<String>,
    pub page_url: String,
    pub download: LatestDownload,
}

/// An archive directory for the model that is not listed as an entry. A closed `error_kind`, the
/// same as `fleet firmware ls --json` reports for it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LatestArchiveUnreadable {
    pub directory: String,
    pub error_kind: ArchiveSkipKind,
    pub error: String,
}

/// What one pass over the USB bus found, in the fields `list_cameras` uses. `cameras: []` with
/// `complete: true` means no body is attached; with `complete: false` it means nothing.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LatestUsbScan {
    /// No device was dropped and the bus could be enumerated.
    pub complete: bool,
    /// The bus could not be enumerated at all.
    pub enumeration_error: Option<String>,
    /// Nikon devices attached whose identity could not be read (any model).
    pub unreadable: Vec<UsbUnreadable>,
}

impl LatestUsbScan {
    pub fn from_scan(scan: &UsbScan) -> Self {
        let list = describe_scan(scan);
        LatestUsbScan { complete: list.complete, enumeration_error: list.enumeration_error, unreadable: list.unreadable }
    }
}

#[derive(Serialize, Debug)]
pub struct LatestCamera {
    pub serial: String,
    /// Firmware the camera reports over USB, verbatim.
    pub installed: String,
    pub relation: LatestRelation,
    /// Why the relation is `unreadable`; null otherwise.
    pub relation_detail: Option<String>,
    /// Whether the archive holds the version this camera is running.
    /// Null when `installed` isn't a canonical version, or when it is not in the archive but the
    /// archive could not be fully read (`archive_unreadable`).
    pub running_version_archived: Option<bool>,
}

#[derive(Serialize, Debug)]
pub struct LatestModel {
    pub model: String,
    pub status: LatestStatus,
    /// Null exactly when `status` is `could_not_determine`.
    pub latest: Option<LatestRelease>,
    /// Why the lookup failed; null unless `status` is `could_not_determine`.
    pub reason: Option<String>,
    /// Whether the archive holds `latest.version`. Null when `latest` is, and when it was not
    /// found but the archive could not be fully read (`archive_unreadable`): unknown, not false.
    pub archive_holds_latest: Option<bool>,
    /// All readable archived versions of this model, oldest first.
    pub archived_versions: Vec<String>,
    /// Archive directories for this model that could not be listed (corrupt or unreadable
    /// `metadata.json`, an unsupported `format_version`, a binary with no `metadata.json`). Empty
    /// when the archive was read cleanly.
    pub archive_unreadable: Vec<LatestArchiveUnreadable>,
    /// Directories under `firmware/` that could not be read at all (any model's entry may be
    /// among them); 0 when the walk was clean.
    pub archive_unreadable_dir_entries: usize,
    /// Connected bodies of this model (empty if none are connected, or if `usb_scan` is
    /// incomplete: then it may be partial).
    pub cameras: Vec<LatestCamera>,
    /// How the USB scan went. Read this before concluding that no body is attached.
    pub usb_scan: LatestUsbScan,
    /// Non-fatal oddities from the source and the archive scan.
    pub warnings: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct LatestReport {
    pub models: Vec<LatestModel>,
}

pub fn survey_json(s: &ModelSurvey) -> LatestModel {
    let (status, reason) = match &s.status {
        ModelStatus::NewAvailable { .. } => (LatestStatus::NewAvailable, None),
        ModelStatus::AlreadyArchived { .. } => (LatestStatus::AlreadyArchived, None),
        ModelStatus::CouldNotDetermine { reason } => (LatestStatus::CouldNotDetermine, Some(reason.clone())),
    };
    let latest = s.release.as_ref().ok().map(|r| LatestRelease {
        version: r.version.to_string(),
        released: r.released.clone(),
        page_url: r.page_url.clone(),
        download: match r.download {
            DownloadAccess::Gated { .. } => LatestDownload::Gated,
            DownloadAccess::Direct { .. } => LatestDownload::Direct,
        },
    });
    let mut archived = s.archived.clone();
    archived.sort();
    let mut warnings: Vec<String> = s.release.as_ref().map(|r| r.warnings.clone()).unwrap_or_default();
    warnings.extend(s.archive_notes.iter().cloned());
    // Not found is "no" only when every entry could be read; otherwise the missing version may be
    // in one that could not.
    let archive_certain = s.archive_unreadable.is_empty() && s.archive_unreadable_dir_entries == 0;
    let holds = |v: &FwVersion| match s.archived.contains(v) {
        true => Some(true),
        false => archive_certain.then_some(false),
    };
    let cameras = s.cameras.iter().map(|(serial, fw)| {
        let (relation, relation_detail) = match &s.release {
            Ok(r) => match camera_relation(fw, r.version) {
                CameraRelation::Behind => (LatestRelation::Behind, None),
                CameraRelation::Current => (LatestRelation::Current, None),
                CameraRelation::Ahead => (LatestRelation::Ahead, None),
                CameraRelation::Unreadable(why) => (LatestRelation::Unreadable, Some(why)),
            },
            Err(_) => (LatestRelation::LatestUnknown, None),
        };
        LatestCamera {
            serial: serial.clone(),
            installed: fw.clone(),
            relation,
            relation_detail,
            running_version_archived: FwVersion::parse_canonical(fw).ok().and_then(|v| holds(&v)),
        }
    }).collect();
    LatestModel {
        model: s.model.clone(),
        status,
        archive_holds_latest: s.release.as_ref().ok().and_then(|r| holds(&r.version)),
        latest,
        reason,
        archived_versions: archived.iter().map(|v| v.to_string()).collect(),
        archive_unreadable: s.archive_unreadable.clone(),
        archive_unreadable_dir_entries: s.archive_unreadable_dir_entries,
        cameras,
        usb_scan: s.usb_scan.clone(),
        warnings,
    }
}

/// The models watched when none are named.
pub const DEFAULT_WATCH_MODELS: &[&str] = &["Z 9", "Z6_3"];

/// The models to survey: those named, or the default watch list.
pub fn watch_models(requested: &[String]) -> Vec<String> {
    if requested.is_empty() {
        DEFAULT_WATCH_MODELS.iter().map(|m| m.to_string()).collect()
    } else {
        requested.to_vec()
    }
}

pub fn survey_models(
    data_dir: &Path,
    models: &[String],
    source: &dyn FirmwareSource,
    http: &dyn HttpGet,
) -> Vec<ModelSurvey> {
    survey_models_with_scan(data_dir, models, source, http, &usb_camera_scan())
}

/// [`survey_models`] with the USB pass given, so it can be tested without a bus.
pub fn survey_models_with_scan(
    data_dir: &Path,
    models: &[String],
    source: &dyn FirmwareSource,
    http: &dyn HttpGet,
    usb: &UsbScan,
) -> Vec<ModelSurvey> {
    let usb_scan = LatestUsbScan::from_scan(usb);
    models.iter().map(|model| {
        let release = source.latest(model, http);
        let mut archived = Vec::new();
        let mut archive_notes = Vec::new();
        let scan = scan_archives(data_dir, Some(model));
        for e in &scan.entries {
            match FwVersion::parse_canonical(&e.version) {
                Ok(v) => archived.push(v),
                Err(err) => archive_notes.push(format!("archive entry {:?} ignored: {err}", e.version)),
            }
        }
        let archive_unreadable: Vec<LatestArchiveUnreadable> = scan.skipped.iter().map(|k| LatestArchiveUnreadable {
            directory: k.directory.display().to_string(),
            error_kind: k.kind,
            error: k.error.clone(),
        }).collect();
        // The typed fields carry the verdict; these only say it where a reader sees `warnings`.
        archive_notes.extend(archive_unreadable.iter().map(|u| format!("archive entry {} not listed: {}", u.directory, u.error)));
        if scan.skipped_dir_entries > 0 {
            archive_notes.push(format!(
                "{} director{} under {} could not be read, so the archive may hold more than is listed",
                scan.skipped_dir_entries,
                if scan.skipped_dir_entries == 1 { "y" } else { "ies" },
                scan.root.display(),
            ));
        }
        let status = assess(&release, &archived);
        let cams = usb.cameras.iter().filter(|c| &c.model == model)
            .map(|c| (c.serial.clone(), c.firmware.clone())).collect();
        ModelSurvey {
            model: model.clone(),
            release,
            archived,
            archive_notes,
            cameras: cams,
            archive_unreadable,
            archive_unreadable_dir_entries: scan.skipped_dir_entries,
            usb_scan: usb_scan.clone(),
            status,
        }
    }).collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware_source::assess;
    use crate::sdk::{UsbCameraInfo, UsbStage};
    use serde_json::json;
    use std::path::PathBuf;

    fn fwv(major: u32, minor: u32) -> FwVersion {
        FwVersion { major, minor }
    }

    fn test_release(version: FwVersion, download: DownloadAccess) -> Release {
        Release {
            model: "Z 9".into(),
            version,
            released: Some("2026-10-20".into()),
            page_url: "https://dl.test/en/download/fw/607.html".into(),
            download,
            warnings: vec!["row looked odd".into()],
            raw_page: String::new(),
        }
    }

    fn gated() -> DownloadAccess {
        DownloadAccess::Gated { notice: "click".into() }
    }

    fn test_survey(release: Result<Release, SourceError>, archived: Vec<FwVersion>, cameras: &[(&str, &str)]) -> ModelSurvey {
        let status = assess(&release, &archived);
        ModelSurvey {
            model: "Z 9".into(),
            release,
            archived,
            archive_notes: vec!["archive entry \"x\" ignored: bad".into()],
            cameras: cameras.iter().map(|(s, f)| (s.to_string(), f.to_string())).collect(),
            archive_unreadable: Vec::new(),
            archive_unreadable_dir_entries: 0,
            usb_scan: LatestUsbScan { complete: true, enumeration_error: None, unreadable: Vec::new() },
            status,
        }
    }

    /// Pins the whole object for a new release with cameras in every relation.
    #[test]
    fn latest_json_shape_new_available() {
        let s = test_survey(
            Ok(test_release(fwv(5, 33), gated())),
            vec![fwv(5, 31), fwv(5, 9), fwv(5, 10)],
            &[("A1", "5.31"), ("A2", "5.33"), ("A3", "5.34"), ("A4", "garbage")],
        );
        assert_eq!(
            serde_json::to_value(survey_json(&s)).unwrap(),
            json!({
                "model": "Z 9",
                "status": "new_available",
                "latest": {
                    "version": "5.33",
                    "released": "2026-10-20",
                    "page_url": "https://dl.test/en/download/fw/607.html",
                    "download": "gated"
                },
                "reason": null,
                "archive_holds_latest": false,
                // Numeric order (minor 9 prints as "5.09"): 5.09 < 5.10 < 5.31.
                "archived_versions": ["5.09", "5.10", "5.31"],
                "archive_unreadable": [],
                "archive_unreadable_dir_entries": 0,
                "usb_scan": {"complete": true, "enumeration_error": null, "unreadable": []},
                "cameras": [
                    {"serial": "A1", "installed": "5.31", "relation": "behind",
                     "relation_detail": null, "running_version_archived": true},
                    {"serial": "A2", "installed": "5.33", "relation": "current",
                     "relation_detail": null, "running_version_archived": false},
                    {"serial": "A3", "installed": "5.34", "relation": "ahead",
                     "relation_detail": null, "running_version_archived": false},
                    {"serial": "A4", "installed": "garbage", "relation": "unreadable",
                     "relation_detail": FwVersion::parse_canonical("garbage").unwrap_err().to_string(),
                     "running_version_archived": null}
                ],
                "warnings": ["row looked odd", "archive entry \"x\" ignored: bad"]
            })
        );
    }

    #[test]
    fn latest_json_shape_already_archived_and_direct() {
        let direct = DownloadAccess::Direct { url: "https://x.test/f".into(), expected_size: None, expected_sha256: None };
        let s = test_survey(Ok(test_release(fwv(5, 33), direct)), vec![fwv(5, 33)], &[]);
        let v = serde_json::to_value(survey_json(&s)).unwrap();
        assert_eq!(v["status"], "already_archived");
        assert_eq!(v["archive_holds_latest"], true);
        assert_eq!(v["latest"]["download"], "direct");
        assert_eq!(v["cameras"], json!([]), "no connected camera -> empty list, not null");
    }

    /// A failed lookup is a typed status with null latest, never prose in a
    /// "nothing new" field.
    #[test]
    fn latest_json_shape_could_not_determine() {
        let err = SourceError::Parse { url: "https://dl.test/p".into(), reason: "layout changed".into() };
        let s = test_survey(Err(err), vec![fwv(5, 31)], &[("A1", "5.31")]);
        let v = serde_json::to_value(survey_json(&s)).unwrap();
        assert_eq!(v["status"], "could_not_determine");
        assert!(v["latest"].is_null());
        assert!(v["archive_holds_latest"].is_null(), "unknown, not false");
        assert!(v["reason"].as_str().unwrap().contains("layout changed"));
        assert_eq!(v["cameras"][0]["relation"], "latest_unknown");
        assert_eq!(v["cameras"][0]["running_version_archived"], true);
        assert_eq!(v["archived_versions"], json!(["5.31"]));
    }

    #[test]
    fn latest_json_top_level_is_models_array() {
        let s = test_survey(Ok(test_release(fwv(5, 33), gated())), vec![], &[]);
        let report = LatestReport { models: vec![survey_json(&s)] };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["models"]);
        assert_eq!(v["models"].as_array().unwrap().len(), 1);
    }

    // ── the archive and the USB pass, as the survey reports them ─────────

    struct FixedSource(FwVersion);
    impl FirmwareSource for FixedSource {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn latest(&self, model: &str, _http: &dyn HttpGet) -> Result<Release, SourceError> {
            Ok(Release { model: model.into(), ..test_release(self.0, gated()) })
        }
    }
    struct NoHttp;
    impl HttpGet for NoHttp {
        fn get_text(&self, url: &str) -> Result<String, SourceError> {
            panic!("no network in this test: {url}")
        }
        fn get_to_file(&self, url: &str, _dest: &Path) -> Result<u64, SourceError> {
            panic!("no network in this test: {url}")
        }
    }

    /// `<data>/firmware/Z_9/<version>/` with the given files.
    fn archive_dir(data: &Path, version: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = data.join("firmware").join("Z_9").join(version);
        std::fs::create_dir_all(&d).unwrap();
        for (name, body) in files {
            std::fs::write(d.join(name), body).unwrap();
        }
        d
    }

    fn good_meta(version: &str) -> String {
        json!({
            "format_version": 1, "model": "Z 9", "firmware_version": version,
            "archived_at": "2026-10-20T00:00:00Z", "archived_by": "test", "bin_sha256": "00",
            "bin_size_bytes": 1, "source_path": "/x", "canonical_snapshot_path": null, "notes": null
        })
        .to_string()
    }

    fn usb_body(serial: &str, fw: &str) -> UsbCameraInfo {
        UsbCameraInfo { product_id: 1, serial: serial.into(), firmware: fw.into(), model: "Z 9".into() }
    }

    /// One survey of "Z 9" (latest 5.33) over `data`, with `usb` as the bus.
    fn survey(data: &Path, usb: &UsbScan) -> serde_json::Value {
        let models = vec!["Z 9".to_string()];
        let s = survey_models_with_scan(data, &models, &FixedSource(fwv(5, 33)), &NoHttp, usb);
        assert_eq!(s.len(), 1);
        serde_json::to_value(survey_json(&s[0])).unwrap()
    }

    #[test]
    fn a_clean_archive_reports_nothing_unreadable_and_a_missing_latest_is_false() {
        let d = tempfile::tempdir().unwrap();
        archive_dir(d.path(), "5.31", &[("metadata.json", &good_meta("5.31")), ("firmware.bin", "x")]);
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable"], json!([]));
        assert_eq!(v["archive_unreadable_dir_entries"], 0);
        assert_eq!(v["archived_versions"], json!(["5.31"]));
        assert_eq!(v["archive_holds_latest"], false, "everything was read and 5.33 is not there");
    }

    #[test]
    fn an_unreadable_entry_is_reported_as_such_and_not_archived_becomes_unknown() {
        let d = tempfile::tempdir().unwrap();
        archive_dir(d.path(), "5.31", &[("metadata.json", &good_meta("5.31")), ("firmware.bin", "x")]);
        let bad = archive_dir(d.path(), "5.33", &[("metadata.json", "{ not json"), ("firmware.bin", "x")]);
        let usb = UsbScan { cameras: vec![usb_body("A1", "5.33"), usb_body("A2", "5.31")], ..UsbScan::default() };
        let v = survey(d.path(), &usb);
        assert_eq!(v["archive_unreadable"].as_array().unwrap().len(), 1);
        assert_eq!(v["archive_unreadable"][0]["directory"], bad.display().to_string());
        assert_eq!(v["archive_unreadable"][0]["error_kind"], "metadata_corrupt");
        assert!(v["archive_unreadable"][0]["error"].as_str().unwrap().contains("corrupt metadata"));
        assert_eq!(v["archived_versions"], json!(["5.31"]), "the readable entry is still listed");
        assert!(v["archive_holds_latest"].is_null(), "5.33 may be the unreadable one: unknown, not false");
        // A camera on 5.33 cannot be called unarchived; one on 5.31 is archived whatever else is broken.
        assert!(v["cameras"][0]["running_version_archived"].is_null());
        assert_eq!(v["cameras"][1]["running_version_archived"], true);
        assert!(v["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("not listed")));
    }

    #[test]
    fn an_unreadable_entry_does_not_hide_a_latest_that_is_held() {
        // Found is found: the unreadable entry cannot make a held version unknown.
        let d = tempfile::tempdir().unwrap();
        archive_dir(d.path(), "5.33", &[("metadata.json", &good_meta("5.33")), ("firmware.bin", "x")]);
        archive_dir(d.path(), "5.31", &[("metadata.json", "{ not json")]);
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_holds_latest"], true);
        assert_eq!(v["archive_unreadable"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_binary_without_metadata_is_an_incomplete_archive_not_an_absent_one() {
        let d = tempfile::tempdir().unwrap();
        archive_dir(d.path(), "5.33", &[("firmware.bin", "x")]);
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable"][0]["error_kind"], "incomplete_archive");
        assert_eq!(v["archived_versions"], json!([]));
        assert!(v["archive_holds_latest"].is_null());
    }

    #[test]
    fn an_unsupported_metadata_version_is_unreadable_too() {
        let d = tempfile::tempdir().unwrap();
        archive_dir(d.path(), "5.33", &[("metadata.json", &good_meta("5.33").replace("\"format_version\":1", "\"format_version\":2"))]);
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable"][0]["error_kind"], "unsupported_version");
    }

    #[test]
    fn another_models_broken_entry_is_not_this_models() {
        let d = tempfile::tempdir().unwrap();
        let other = d.path().join("firmware").join("Z6_3").join("1.00");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("metadata.json"), "{ not json").unwrap();
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable"], json!([]));
        assert_eq!(v["archive_holds_latest"], false);
    }

    #[test]
    fn no_archive_directory_at_all_is_a_clean_empty_archive() {
        let d = tempfile::tempdir().unwrap();
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable"], json!([]));
        assert_eq!(v["archive_holds_latest"], false);
    }

    #[test]
    fn a_complete_scan_with_no_body_says_so() {
        let d = tempfile::tempdir().unwrap();
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["cameras"], json!([]));
        assert_eq!(v["usb_scan"], json!({"complete": true, "enumeration_error": null, "unreadable": []}));
    }

    #[test]
    fn a_failed_enumeration_is_not_reported_as_no_body_attached() {
        let d = tempfile::tempdir().unwrap();
        let usb = UsbScan { enumeration_error: Some("libusb: no backend".into()), ..UsbScan::default() };
        let v = survey(d.path(), &usb);
        assert_eq!(v["cameras"], json!([]));
        assert_eq!(v["usb_scan"]["complete"], false);
        assert_eq!(v["usb_scan"]["enumeration_error"], "libusb: no backend");
    }

    #[test]
    fn a_body_that_could_not_be_read_makes_the_scan_incomplete_and_is_listed() {
        let d = tempfile::tempdir().unwrap();
        let usb = UsbScan {
            cameras: vec![usb_body("A1", "5.33")],
            unreadable: vec![UsbUnreadable { bus: 3, address: 7, stage: UsbStage::Open, error: "access denied".into() }],
            enumeration_error: None,
        };
        let v = survey(d.path(), &usb);
        assert_eq!(v["cameras"][0]["serial"], "A1", "the readable body is still reported");
        assert_eq!(v["usb_scan"]["complete"], false);
        assert_eq!(v["usb_scan"]["enumeration_error"], serde_json::Value::Null);
        assert_eq!(v["usb_scan"]["unreadable"][0]["bus"], 3);
        assert_eq!(v["usb_scan"]["unreadable"][0]["stage"], "open");
        assert_eq!(v["usb_scan"]["unreadable"][0]["error"], "access denied");
    }

    #[test]
    fn the_usb_scan_is_the_same_object_for_every_model_surveyed() {
        let d = tempfile::tempdir().unwrap();
        let usb = UsbScan { enumeration_error: Some("boom".into()), ..UsbScan::default() };
        let models = vec!["Z 9".to_string(), "Z6_3".to_string()];
        let s = survey_models_with_scan(d.path(), &models, &FixedSource(fwv(5, 33)), &NoHttp, &usb);
        assert_eq!(s[0].usb_scan, s[1].usb_scan);
        assert!(!s[1].usb_scan.complete);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_read_makes_the_archive_uncertain() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let d = tempfile::tempdir().unwrap();
        let locked = d.path().join("firmware").join("Z_9");
        archive_dir(d.path(), "5.33", &[("metadata.json", &good_meta("5.33")), ("firmware.bin", "x")]);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = Restore(locked.clone());
        if std::fs::read_dir(&locked).is_ok() {
            return; // running as root: nothing is unreadable
        }
        let v = survey(d.path(), &UsbScan::default());
        assert_eq!(v["archive_unreadable_dir_entries"], 1);
        assert_eq!(v["archive_unreadable"], json!([]), "no entry was identified, only a directory that could not be read");
        assert!(v["archive_holds_latest"].is_null(), "5.33 may be in the directory that could not be read");
        assert!(v["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("could not be read")));
    }
}
