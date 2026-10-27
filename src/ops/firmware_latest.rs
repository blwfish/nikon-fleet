//! The shape of `firmware latest --json`: one object per watched model, with
//! every verdict a closed snake_case enum. A contract with scripts, the GUI and
//! (planned) the MCP, so it lives in the library rather than in `main.rs`.

use serde::Serialize;

use crate::firmware_source::{
    CameraRelation, DownloadAccess, FwVersion, ModelStatus, Release, SourceError, camera_relation,
};

/// Everything we know about one model before deciding what to do.
pub struct ModelSurvey {
    pub model: String,
    pub release: Result<Release, SourceError>,
    pub archived: Vec<FwVersion>,
    /// Archive entries whose version string isn't canonical and so can't be matched.
    pub archive_notes: Vec<String>,
    /// (serial, reported firmware) for connected bodies of this model.
    pub cameras: Vec<(String, String)>,
    pub status: ModelStatus,
}

// ── `firmware latest --json` ────────────────────────────────────────────────
// The shape is a contract with scripts and the GUI; `latest_json_shape_*`
// tests pin it. Every verdict is a closed enum serialised as snake_case; the
// free-text `reason`/`detail` fields only explain a verdict, never carry it.

/// Source verdict for one model.
#[derive(Serialize, Debug, PartialEq, Eq)]
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
#[derive(Serialize, Debug, PartialEq, Eq)]
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

#[derive(Serialize, Debug, PartialEq, Eq)]
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

#[derive(Serialize, Debug)]
pub struct LatestCamera {
    pub serial: String,
    /// Firmware the camera reports over USB, verbatim.
    pub installed: String,
    pub relation: LatestRelation,
    /// Why the relation is `unreadable`; null otherwise.
    pub relation_detail: Option<String>,
    /// Whether the archive holds the version this camera is running.
    /// Null when `installed` isn't a canonical version.
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
    /// Whether the archive holds `latest.version`. Null when `latest` is.
    pub archive_holds_latest: Option<bool>,
    /// All archived versions of this model, oldest first.
    pub archived_versions: Vec<String>,
    /// Connected bodies of this model (empty if none are connected).
    pub cameras: Vec<LatestCamera>,
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
            running_version_archived: FwVersion::parse_canonical(fw).ok().map(|v| s.archived.contains(&v)),
        }
    }).collect();
    LatestModel {
        model: s.model.clone(),
        status,
        archive_holds_latest: s.release.as_ref().ok().map(|r| s.archived.contains(&r.version)),
        latest,
        reason,
        archived_versions: archived.iter().map(|v| v.to_string()).collect(),
        cameras,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware_source::assess;
    use serde_json::json;

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
}
