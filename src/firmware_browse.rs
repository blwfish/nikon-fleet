//! Human-in-the-loop firmware acquisition: `fleet firmware browse <model>`.
//!
//! Nikon gates every firmware download behind a licence click-through (see
//! docs/firmware-source-findings.md §3). This module lets the *human* do that
//! click-through in an embedded browser window while the tool only decides
//! where the resulting file lands and then archives it.
//!
//! Hard constraints, from the findings doc and the user:
//!   * never inject script to tick "I agree" or press Download,
//!   * never read the site's scripts to reach the file without the click,
//!   * never accept the licence on the user's behalf.
//! Nothing here touches the page. The window shell (`firmware_webview`,
//! cargo feature `webview`) wires three browser callbacks — navigation,
//! download-started, download-completed — to [`DownloadTracker`], which is
//! plain state with no UI in it and is what the tests exercise.
//!
//! Layout: the window is behind [`BrowserWindow`], so everything except the
//! window itself (staging dir, checks, ingest, provenance) runs, and is
//! tested, without a display.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use thiserror::Error;
use url::Url;

use crate::firmware::{FirmwareError, IngestOutcome, Provenance, RetrievalMethod, ingest_bundle};
use crate::firmware_source::{
    DownloadAccess, FirmwareSource, FwVersion, HttpGet, Release, SourceError,
};

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Why a download was denied inside the window. Closed vocabulary; the CLI
/// prints these, nothing branches on their text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The last firmware page the user was on is not the release we are
    /// archiving as (e.g. they wandered to another product's page).
    WrongReleasePage { seen: String, expected: String },
    /// A download had already been captured; the window is about to close.
    AlreadyCaptured,
}

fn describe_refusals(r: &[Refusal]) -> String {
    if r.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = r
        .iter()
        .map(|r| match r {
            Refusal::WrongReleasePage { seen, expected } => format!(
                "a download was refused because the page you were on ({seen}) is not the one for the release being archived ({expected})"
            ),
            Refusal::AlreadyCaptured => "a second download was refused".to_owned(),
        })
        .collect();
    format!(" ({})", parts.join("; "))
}

#[derive(Debug, Error)]
pub enum BrowseError {
    /// Built without cargo feature `webview`. The CLI falls back to printing
    /// the page URL and the `--from-file` instructions.
    #[error("this build has no embedded browser (rebuild with `cargo build --features webview`)")]
    NotCompiledIn,
    /// The OS web view could not be created (on Windows: the WebView2 runtime
    /// is missing or too old; on Linux: WebKitGTK/display problems). Same
    /// fallback as `NotCompiledIn`.
    #[error("could not open the embedded browser: {0}")]
    WebViewUnavailable(String),
    #[error("the source offers this release without a licence gate; use `fleet firmware fetch` instead")]
    NotGated,
    #[error("the window was closed before a download completed{}", describe_refusals(.refusals))]
    ClosedWithoutDownload { refusals: Vec<Refusal> },
    #[error("{failed} download(s) started but none completed successfully")]
    DownloadFailed { failed: u32 },
    #[error(
        "the downloaded file name {file:?} names firmware {found}, but this page is for {expected}; \
         not archiving. The download is kept at {kept}"
    )]
    VersionContradiction { file: String, expected: FwVersion, found: String, kept: PathBuf },
    #[error("archiving the download failed: {error}. The download is kept at {kept}")]
    Ingest { kept: PathBuf, error: FirmwareError },
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("staging directory: {0}")]
    Io(#[from] std::io::Error),
}

impl BrowseError {
    /// True when the embedded browser cannot be used at all, so the caller
    /// should fall back to the manual download instructions.
    pub fn browser_unavailable(&self) -> bool {
        matches!(self, Self::NotCompiledIn | Self::WebViewUnavailable(_))
    }
}

// ─── What the file name says about the version ───────────────────────────────

/// Whether a downloaded file's *name* agrees with the expected version.
/// A heuristic on a name, not on contents: `Unknown` is "no evidence", never
/// "fine". The file name formats are from memory of Nikon's installers and
/// have not been checked against a real download (see the findings doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionEvidence {
    /// The name carries the expected version.
    Confirms,
    /// The name carries only other version(s).
    Contradicts(Vec<FwVersion>),
    /// No recognisable version in the name.
    Unknown,
}

/// Look for Nikon's `V<MMmm>` token (`F-Z9-V0533W.exe` → 5.33) and, for
/// extracted bundles, a `_<MMmm>` stem on a `.bin` (`Z_9_0533.bin`). Only runs
/// of exactly four digits count, so `V05330` or a date such as `_20261020`
/// yield nothing.
pub fn file_version_evidence(file_name: &str, expected: FwVersion) -> VersionEvidence {
    let bytes = file_name.as_bytes();
    let is_bin = file_name.to_ascii_lowercase().ends_with(".bin");
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let run = &file_name[start..i];
        let prev = if start == 0 { None } else { Some(bytes[start - 1]) };
        let qualifies = run.len() == 4
            && match prev {
                Some(b'V') | Some(b'v') => true,
                Some(b'_') => is_bin,
                _ => false,
            };
        if qualifies {
            // Cannot fail: four ASCII digits.
            let major: u32 = run[..2].parse().unwrap_or(0);
            let minor: u32 = run[2..].parse().unwrap_or(0);
            found.push(FwVersion { major, minor });
        }
    }
    if found.is_empty() {
        VersionEvidence::Unknown
    } else if found.contains(&expected) {
        VersionEvidence::Confirms
    } else {
        VersionEvidence::Contradicts(found)
    }
}

// ─── Download tracking (pure state) ──────────────────────────────────────────

/// A completed, successful download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// URL the file came from.
    pub url: String,
    /// Where the bytes are, inside the staging directory.
    pub path: PathBuf,
    /// The name the site suggested, unsanitised (recorded in provenance).
    pub original_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    /// This was the download we wanted; the window can close.
    Captured,
    /// A started download ended unsuccessfully.
    Failed,
    /// Not one of ours (never started through the tracker).
    Unrecognised,
}

struct InFlight {
    url: String,
    path: PathBuf,
    original_name: String,
}

/// State behind the three browser callbacks. It never navigates, clicks or
/// injects anything; it only answers "may this load / where does this file go
/// / is this the file".
pub struct DownloadTracker {
    expected_key: String,
    expected_display: String,
    is_release_page: fn(&Url) -> bool,
    last_release_page: String,
    staging: PathBuf,
    seq: u32,
    in_flight: Vec<InFlight>,
    captured: Option<Captured>,
    failed: u32,
    refusals: Vec<Refusal>,
}

/// scheme + host + path, without query, fragment or trailing slash — so
/// `#agree` or `?lang=en` on the same page still counts as the same page.
fn page_key(u: &Url) -> String {
    format!(
        "{}://{}{}",
        u.scheme(),
        u.host_str().unwrap_or("").to_ascii_lowercase(),
        u.path().trim_end_matches('/')
    )
}

/// Nikon Download Center firmware pages live under `/<lang>/download/fw/`.
/// Another source needs its own predicate.
pub fn nikon_is_release_page(u: &Url) -> bool {
    u.path().contains("/download/fw/")
}

/// Keep only the final path component and characters that are safe in a file
/// name on every OS; never empty, never hidden, never a path.
pub fn sanitize_download_name(suggested: &Path) -> String {
    let raw = suggested
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(100)
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_owned();
    if cleaned.is_empty() { "download.bin".to_owned() } else { cleaned }
}

impl DownloadTracker {
    pub fn new(
        expected_page: &Url,
        staging: &Path,
        is_release_page: fn(&Url) -> bool,
    ) -> Self {
        Self {
            expected_key: page_key(expected_page),
            expected_display: expected_page.to_string(),
            is_release_page,
            // The window is opened on the expected page.
            last_release_page: page_key(expected_page),
            staging: staging.to_owned(),
            seq: 0,
            in_flight: Vec::new(),
            captured: None,
            failed: 0,
            refusals: Vec::new(),
        }
    }

    /// Navigation callback: may the window load `url`? Only web pages; no
    /// `file:`, custom schemes or `javascript:` navigations.
    pub fn on_navigate(&mut self, url: &str) -> bool {
        if url == "about:blank" {
            return true;
        }
        let Ok(u) = Url::parse(url) else { return false };
        if !matches!(u.scheme(), "http" | "https") {
            return false;
        }
        if (self.is_release_page)(&u) {
            self.last_release_page = page_key(&u);
        }
        true
    }

    /// Download-started callback: `Some(path)` to allow and save there,
    /// `None` to deny. `suggested` is the OS's default destination; only its
    /// file name is used.
    pub fn on_download_started(&mut self, url: &str, suggested: &Path) -> Option<PathBuf> {
        if self.captured.is_some() {
            self.refusals.push(Refusal::AlreadyCaptured);
            return None;
        }
        if self.last_release_page != self.expected_key {
            self.refusals.push(Refusal::WrongReleasePage {
                seen: self.last_release_page.clone(),
                expected: self.expected_display.clone(),
            });
            return None;
        }
        self.seq += 1;
        let original_name = suggested
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = self
            .staging
            .join(format!("{:02}-{}", self.seq, sanitize_download_name(suggested)));
        self.in_flight.push(InFlight { url: url.to_owned(), path: path.clone(), original_name });
        Some(path)
    }

    /// Download-completed callback.
    pub fn on_download_finished(&mut self, url: &str, path: Option<&Path>, ok: bool) -> Finish {
        let idx = match path {
            Some(p) => self.in_flight.iter().position(|f| f.path == p),
            None => self.in_flight.iter().position(|f| f.url == url),
        };
        let Some(idx) = idx else { return Finish::Unrecognised };
        let f = self.in_flight.remove(idx);
        if ok && self.captured.is_none() {
            self.captured = Some(Captured { url: f.url, path: f.path, original_name: f.original_name });
            Finish::Captured
        } else {
            self.failed += 1;
            Finish::Failed
        }
    }

    pub fn captured(&self) -> Option<&Captured> {
        self.captured.as_ref()
    }

    pub fn failed_downloads(&self) -> u32 {
        self.failed
    }

    pub fn refusals(&self) -> &[Refusal] {
        &self.refusals
    }
}

// ─── Window abstraction and orchestration ────────────────────────────────────

/// A browser window the human uses. `run` opens `page_url`, feeds the three
/// callbacks into `tracker`, and returns when the window closes (the shell
/// closes it itself once `tracker.captured()` is set).
pub trait BrowserWindow {
    fn run(
        &self,
        page_url: &str,
        staging: &Path,
        tracker: Rc<RefCell<DownloadTracker>>,
    ) -> Result<(), BrowseError>;
}

#[derive(Debug)]
pub struct BrowseReport {
    pub release: Release,
    pub outcome: IngestOutcome,
    pub evidence: VersionEvidence,
    pub captured: Captured,
}

/// Open the release's download page for the human, capture what they
/// download into a fresh staging directory, check it against the expected
/// version, and archive it with provenance (never overwriting existing
/// different bytes — that is `ingest_bundle`'s rule).
///
/// On any failure after a file was captured, the staging directory is kept
/// and named in the error, so a licence click-through is never wasted.
pub fn browse_firmware(
    data_dir: &Path,
    source: &dyn FirmwareSource,
    http: &dyn HttpGet,
    model: &str,
    window: &dyn BrowserWindow,
    is_release_page: fn(&Url) -> bool,
    now_rfc3339: &str,
) -> Result<BrowseReport, BrowseError> {
    let release = source.latest(model, http)?;
    if !matches!(release.download, DownloadAccess::Gated { .. }) {
        return Err(BrowseError::NotGated);
    }
    let page = Url::parse(&release.page_url)
        .map_err(|e| SourceError::Parse { url: release.page_url.clone(), reason: format!("page URL: {e}") })?;

    let staging = tempfile::Builder::new().prefix("nikon-fleet-fw-").tempdir()?;
    let tracker = Rc::new(RefCell::new(DownloadTracker::new(&page, staging.path(), is_release_page)));
    window.run(&release.page_url, staging.path(), tracker.clone())?;

    let (captured, failed, refusals) = {
        let t = tracker.borrow();
        (t.captured().cloned(), t.failed_downloads(), t.refusals().to_vec())
    };
    let Some(captured) = captured else {
        return Err(if failed > 0 {
            BrowseError::DownloadFailed { failed }
        } else {
            BrowseError::ClosedWithoutDownload { refusals }
        });
    };

    let evidence = file_version_evidence(&captured.original_name, release.version);
    if let VersionEvidence::Contradicts(found) = &evidence {
        let found = found.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ");
        return Err(BrowseError::VersionContradiction {
            file: captured.original_name.clone(),
            expected: release.version,
            found,
            kept: staging.keep(),
        });
    }

    let prov = Provenance {
        source_id: source.id().to_owned(),
        source_url: captured.url.clone(),
        page_url: release.page_url.clone(),
        released: release.released.clone(),
        retrieved_at: now_rfc3339.to_owned(),
        method: RetrievalMethod::BrowserAfterGate,
        published_size: None,
        published_sha256: None,
        original_filename: Some(captured.original_name.clone()).filter(|n| !n.is_empty()),
    };
    match ingest_bundle(
        data_dir, &release.model, &release.version.to_string(), &captured.path, &prov, Some(&release.raw_page),
    ) {
        Ok(outcome) => Ok(BrowseReport { release, outcome, evidence, captured }),
        Err(error) => Err(BrowseError::Ingest { kept: staging.keep(), error }),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::{archive_exists, load_meta};
    use crate::firmware_source::{Release, SourceError};
    use std::fs;

    const PAGE: &str = "https://downloadcenter.nikonimglib.com/en/download/fw/607.html";
    const OTHER: &str = "https://downloadcenter.nikonimglib.com/en/download/fw/574.html";

    fn v(major: u32, minor: u32) -> FwVersion {
        FwVersion { major, minor }
    }

    fn tracker(staging: &Path) -> DownloadTracker {
        DownloadTracker::new(&Url::parse(PAGE).unwrap(), staging, nikon_is_release_page)
    }

    // ── file_version_evidence ────────────────────────────────────────────────

    #[test]
    fn evidence_nikon_installer_names() {
        assert_eq!(file_version_evidence("F-Z9-V0533W.exe", v(5, 33)), VersionEvidence::Confirms);
        assert_eq!(file_version_evidence("F-Z9-V0533M.dmg", v(5, 33)), VersionEvidence::Confirms);
        assert_eq!(file_version_evidence("F-Z6III-V0201W.exe", v(2, 1)), VersionEvidence::Confirms);
        assert_eq!(file_version_evidence("f-z9-v0533w.exe", v(5, 33)), VersionEvidence::Confirms);
    }

    #[test]
    fn evidence_contradiction_is_reported_with_the_found_versions() {
        assert_eq!(
            file_version_evidence("F-Z9-V0531W.exe", v(5, 33)),
            VersionEvidence::Contradicts(vec![v(5, 31)])
        );
        // Neighbouring version, both directions, and 5.9 vs 5.10 style pairs.
        assert_eq!(
            file_version_evidence("F-Z9-V0510W.exe", v(5, 9)),
            VersionEvidence::Contradicts(vec![v(5, 10)])
        );
        assert_eq!(
            file_version_evidence("F-Z9-V0509W.exe", v(5, 10)),
            VersionEvidence::Contradicts(vec![v(5, 9)])
        );
    }

    #[test]
    fn evidence_bin_stem_form_only_on_bin_files() {
        assert_eq!(file_version_evidence("Z_9_0533.bin", v(5, 33)), VersionEvidence::Confirms);
        assert_eq!(file_version_evidence("Z_9_0533.BIN", v(5, 33)), VersionEvidence::Confirms);
        // `_0533` on a non-.bin name is not a recognised token.
        assert_eq!(file_version_evidence("Z_9_0533.zip", v(5, 33)), VersionEvidence::Unknown);
        assert_eq!(file_version_evidence("firmware_2026.zip", v(5, 33)), VersionEvidence::Unknown);
    }

    #[test]
    fn evidence_requires_exactly_four_digits() {
        assert_eq!(file_version_evidence("F-Z9-V053W.exe", v(5, 33)), VersionEvidence::Unknown);
        assert_eq!(file_version_evidence("F-Z9-V05330W.exe", v(5, 33)), VersionEvidence::Unknown);
        assert_eq!(file_version_evidence("x_20261020.bin", v(5, 33)), VersionEvidence::Unknown);
        assert_eq!(file_version_evidence("", v(5, 33)), VersionEvidence::Unknown);
        assert_eq!(file_version_evidence("download", v(5, 33)), VersionEvidence::Unknown);
    }

    #[test]
    fn evidence_confirms_if_any_token_matches() {
        // Two tokens: one matches, so not a contradiction.
        assert_eq!(
            file_version_evidence("F-Z9-V0531W_to_V0533W.exe", v(5, 33)),
            VersionEvidence::Confirms
        );
    }

    // ── sanitize_download_name ───────────────────────────────────────────────

    #[test]
    fn sanitize_strips_paths_and_odd_characters() {
        assert_eq!(sanitize_download_name(Path::new("/Users/x/Downloads/F-Z9-V0533W.exe")), "F-Z9-V0533W.exe");
        assert_eq!(sanitize_download_name(Path::new("a b$c.bin")), "a_b_c.bin");
        assert_eq!(sanitize_download_name(Path::new("../../etc/passwd")), "passwd");
        assert_eq!(sanitize_download_name(Path::new(".hidden")), "hidden");
        assert_eq!(sanitize_download_name(Path::new("...")), "download.bin");
        assert_eq!(sanitize_download_name(Path::new("")), "download");
    }

    #[test]
    fn sanitize_caps_length_at_100_chars() {
        let at = "a".repeat(100);
        let over = "a".repeat(101);
        assert_eq!(sanitize_download_name(Path::new(&at)).len(), 100);
        assert_eq!(sanitize_download_name(Path::new(&over)).len(), 100);
    }

    // ── DownloadTracker ──────────────────────────────────────────────────────

    #[test]
    fn navigation_allows_only_web_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        assert!(t.on_navigate(PAGE));
        assert!(t.on_navigate("http://example.test/x"));
        assert!(t.on_navigate("about:blank"));
        assert!(!t.on_navigate("about:srcdoc"));
        assert!(!t.on_navigate("file:///etc/passwd"));
        assert!(!t.on_navigate("javascript:alert(1)"));
        assert!(!t.on_navigate("ftp://example.test/x"));
        assert!(!t.on_navigate("not a url"));
        assert!(!t.on_navigate(""));
    }

    #[test]
    fn download_lands_in_staging_under_a_sanitised_unique_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        let p1 = t.on_download_started("https://x.test/a", Path::new("/home/u/Downloads/F-Z9-V0533W.exe")).unwrap();
        assert_eq!(p1, dir.path().join("01-F-Z9-V0533W.exe"));
        assert!(p1.is_absolute());
        // After a failed first attempt the retry gets a different path.
        assert_eq!(t.on_download_finished("https://x.test/a", Some(&p1), false), Finish::Failed);
        let p2 = t.on_download_started("https://x.test/a", Path::new("F-Z9-V0533W.exe")).unwrap();
        assert_eq!(p2, dir.path().join("02-F-Z9-V0533W.exe"));
        assert_ne!(p1, p2);
        assert_eq!(t.failed_downloads(), 1);
    }

    #[test]
    fn successful_finish_captures_with_original_name_and_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        let p = t.on_download_started("https://cdn.test/f?token=1", Path::new("F Z9 V0533W.exe")).unwrap();
        assert_eq!(t.on_download_finished("https://cdn.test/f?token=1", Some(&p), true), Finish::Captured);
        let c = t.captured().unwrap();
        assert_eq!(c.url, "https://cdn.test/f?token=1");
        assert_eq!(c.path, p);
        // The unsanitised name is what provenance records.
        assert_eq!(c.original_name, "F Z9 V0533W.exe");
        assert_eq!(t.failed_downloads(), 0);
    }

    #[test]
    fn finish_without_a_path_matches_by_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        t.on_download_started("https://x.test/a", Path::new("a.exe")).unwrap();
        assert_eq!(t.on_download_finished("https://x.test/a", None, true), Finish::Captured);
    }

    #[test]
    fn finish_for_an_unknown_download_is_not_captured() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        assert_eq!(
            t.on_download_finished("https://x.test/a", Some(Path::new("/elsewhere/a")), true),
            Finish::Unrecognised
        );
        assert!(t.captured().is_none());
        assert_eq!(t.failed_downloads(), 0);
    }

    #[test]
    fn second_download_after_capture_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        let p = t.on_download_started("https://x.test/a", Path::new("a.exe")).unwrap();
        t.on_download_finished("https://x.test/a", Some(&p), true);
        assert!(t.on_download_started("https://x.test/b", Path::new("b.exe")).is_none());
        assert_eq!(t.refusals(), &[Refusal::AlreadyCaptured]);
    }

    #[test]
    fn download_from_a_different_firmware_page_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        // The user wandered to the Z6III page and clicked there.
        assert!(t.on_navigate(OTHER));
        assert!(t.on_download_started("https://x.test/a", Path::new("a.exe")).is_none());
        assert_eq!(
            t.refusals(),
            &[Refusal::WrongReleasePage { seen: OTHER.to_owned(), expected: PAGE.to_owned() }]
        );
        // Coming back to the right page re-enables downloads.
        assert!(t.on_navigate(PAGE));
        assert!(t.on_download_started("https://x.test/a", Path::new("a.exe")).is_some());
    }

    #[test]
    fn query_fragment_and_trailing_slash_do_not_change_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        assert!(t.on_navigate(&format!("{PAGE}?x=1#agree")));
        assert!(t.on_download_started("https://x.test/a", Path::new("a.exe")).is_some());
        let mut t2 = tracker(dir.path());
        assert!(t2.on_navigate(&format!("{PAGE}/")));
        assert!(t2.on_download_started("https://x.test/a", Path::new("a.exe")).is_some());
    }

    #[test]
    fn navigating_to_non_release_pages_does_not_count_as_leaving() {
        // The JS-built file link navigates to a non-release URL; that must
        // not disarm the download that follows it.
        let dir = tempfile::tempdir().unwrap();
        let mut t = tracker(dir.path());
        assert!(t.on_navigate("https://cdn.test/files/F-Z9-V0533W.exe"));
        assert!(t.on_download_started("https://cdn.test/files/F-Z9-V0533W.exe", Path::new("a.exe")).is_some());
    }

    // ── browse_firmware ──────────────────────────────────────────────────────

    struct FixedSource(Result<Release, String>);
    impl FirmwareSource for FixedSource {
        fn id(&self) -> &'static str {
            "test-source"
        }
        fn latest(&self, model: &str, _http: &dyn HttpGet) -> Result<Release, SourceError> {
            self.0.clone().map_err(|reason| SourceError::Parse { url: model.to_owned(), reason })
        }
    }
    struct NoHttp;
    impl HttpGet for NoHttp {
        fn get_text(&self, url: &str) -> Result<String, SourceError> {
            panic!("unexpected GET {url}")
        }
        fn get_to_file(&self, url: &str, _dest: &Path) -> Result<u64, SourceError> {
            panic!("unexpected GET {url}")
        }
    }

    fn release(download: DownloadAccess) -> Release {
        Release {
            model: "Z 9".into(),
            version: v(5, 33),
            released: Some("2026-10-20".into()),
            page_url: PAGE.into(),
            download,
            warnings: vec![],
            raw_page: "<html>page</html>".into(),
        }
    }
    fn gated() -> DownloadAccess {
        DownloadAccess::Gated { notice: "click".into() }
    }

    /// A window that plays back a scripted human session.
    enum Script {
        /// Download `name` with these bytes from `url`, completing OK.
        Download { url: &'static str, name: &'static str, bytes: &'static [u8], ok: bool },
        /// Navigate to another firmware page, then try to download there.
        WrongPageThenDownload,
        /// Close without doing anything.
        Close,
        /// The OS web view can't start.
        Unavailable,
    }
    struct FakeWindow(Script);
    impl BrowserWindow for FakeWindow {
        fn run(&self, page_url: &str, staging: &Path, t: Rc<RefCell<DownloadTracker>>) -> Result<(), BrowseError> {
            assert_eq!(page_url, PAGE);
            assert!(staging.is_dir());
            match &self.0 {
                Script::Download { url, name, bytes, ok } => {
                    let dest = t.borrow_mut().on_download_started(url, Path::new(name)).expect("allowed");
                    if *ok {
                        fs::write(&dest, bytes).unwrap();
                    }
                    t.borrow_mut().on_download_finished(url, Some(&dest), *ok);
                    Ok(())
                }
                Script::WrongPageThenDownload => {
                    t.borrow_mut().on_navigate(OTHER);
                    assert!(t.borrow_mut().on_download_started("https://x.test/a", Path::new("a.exe")).is_none());
                    Ok(())
                }
                Script::Close => Ok(()),
                Script::Unavailable => Err(BrowseError::WebViewUnavailable("WebView2 runtime not found".into())),
            }
        }
    }

    fn run(data: &Path, rel: Result<Release, String>, script: Script) -> Result<BrowseReport, BrowseError> {
        browse_firmware(
            data, &FixedSource(rel), &NoHttp, "Z 9", &FakeWindow(script),
            nikon_is_release_page, "2026-10-27T12:00:00Z",
        )
    }

    #[test]
    fn happy_path_archives_with_provenance_and_original_name() {
        let data = tempfile::tempdir().unwrap();
        let rep = run(
            data.path(), Ok(release(gated())),
            Script::Download { url: "https://cdn.test/f.exe", name: "F-Z9-V0533W.exe", bytes: b"BUNDLE", ok: true },
        ).unwrap();
        assert!(matches!(rep.outcome, IngestOutcome::Archived(_)));
        assert_eq!(rep.evidence, VersionEvidence::Confirms);
        assert!(archive_exists(data.path(), "Z 9", "5.33"));
        let meta = load_meta(data.path(), "Z 9", "5.33").unwrap();
        let p = meta.provenance.unwrap();
        assert_eq!(p.method, RetrievalMethod::BrowserAfterGate);
        assert_eq!(p.source_url, "https://cdn.test/f.exe");
        assert_eq!(p.page_url, PAGE);
        assert_eq!(p.original_filename.as_deref(), Some("F-Z9-V0533W.exe"));
        assert_eq!(p.released.as_deref(), Some("2026-10-20"));
        assert_eq!(p.retrieved_at, "2026-10-27T12:00:00Z");
        assert_eq!(p.source_id, "test-source");
    }

    #[test]
    fn unrecognised_name_still_archives_but_is_flagged_unknown() {
        let data = tempfile::tempdir().unwrap();
        let rep = run(
            data.path(), Ok(release(gated())),
            Script::Download { url: "https://cdn.test/f", name: "download", bytes: b"X", ok: true },
        ).unwrap();
        assert_eq!(rep.evidence, VersionEvidence::Unknown);
        assert!(archive_exists(data.path(), "Z 9", "5.33"));
    }

    #[test]
    fn contradicting_name_is_refused_and_staging_is_kept() {
        let data = tempfile::tempdir().unwrap();
        let err = run(
            data.path(), Ok(release(gated())),
            Script::Download { url: "https://cdn.test/f", name: "F-Z9-V0531W.exe", bytes: b"OLD", ok: true },
        ).unwrap_err();
        let BrowseError::VersionContradiction { found, kept, expected, .. } = &err else { panic!("{err}") };
        assert_eq!(found, "5.31");
        assert_eq!(*expected, v(5, 33));
        assert!(kept.join("01-F-Z9-V0531W.exe").is_file(), "download must survive for the user");
        assert!(!archive_exists(data.path(), "Z 9", "5.33"), "nothing archived on contradiction");
        fs::remove_dir_all(kept).unwrap();
    }

    #[test]
    fn different_bytes_for_an_archived_version_are_refused_and_kept() {
        let data = tempfile::tempdir().unwrap();
        let s = |bytes| Script::Download { url: "https://cdn.test/f", name: "F-Z9-V0533W.exe", bytes, ok: true };
        run(data.path(), Ok(release(gated())), s(b"FIRST")).unwrap();
        // Identical bytes again: no-op.
        let again = run(data.path(), Ok(release(gated())), s(b"FIRST")).unwrap();
        assert!(matches!(again.outcome, IngestOutcome::AlreadyPresent(_)));
        // Different bytes: refused; original survives; the new download is kept.
        let err = run(data.path(), Ok(release(gated())), s(b"SECOND")).unwrap_err();
        let BrowseError::Ingest { kept, error } = &err else { panic!("{err}") };
        assert!(matches!(error, FirmwareError::ContentMismatch { .. }), "{error}");
        assert!(kept.join("01-F-Z9-V0533W.exe").is_file());
        let archived = fs::read(data.path().join("firmware/Z_9/5.33/firmware.bin")).unwrap();
        assert_eq!(archived, b"FIRST");
        fs::remove_dir_all(kept).unwrap();
    }

    #[test]
    fn empty_download_is_not_archived() {
        let data = tempfile::tempdir().unwrap();
        let err = run(
            data.path(), Ok(release(gated())),
            Script::Download { url: "https://cdn.test/f", name: "F-Z9-V0533W.exe", bytes: b"", ok: true },
        ).unwrap_err();
        let BrowseError::Ingest { kept, .. } = &err else { panic!("{err}") };
        assert!(!archive_exists(data.path(), "Z 9", "5.33"));
        fs::remove_dir_all(kept).unwrap();
    }

    #[test]
    fn window_closed_without_download_says_so() {
        let data = tempfile::tempdir().unwrap();
        let err = run(data.path(), Ok(release(gated())), Script::Close).unwrap_err();
        assert!(matches!(err, BrowseError::ClosedWithoutDownload { ref refusals } if refusals.is_empty()), "{err}");
        assert!(!err.browser_unavailable());
    }

    #[test]
    fn refused_download_is_explained_when_the_window_closes() {
        let data = tempfile::tempdir().unwrap();
        // The scripted refusal happens in a different tracker clone's view, so
        // check the message path directly.
        let err = BrowseError::ClosedWithoutDownload {
            refusals: vec![Refusal::WrongReleasePage { seen: OTHER.into(), expected: PAGE.into() }],
        };
        let msg = err.to_string();
        assert!(msg.contains(OTHER) && msg.contains(PAGE), "{msg}");
        let r = run(data.path(), Ok(release(gated())), Script::WrongPageThenDownload).unwrap_err();
        let BrowseError::ClosedWithoutDownload { refusals } = r else { panic!() };
        assert_eq!(refusals.len(), 1);
    }

    #[test]
    fn failed_download_is_reported_as_failed_not_as_closed() {
        let data = tempfile::tempdir().unwrap();
        let err = run(
            data.path(), Ok(release(gated())),
            Script::Download { url: "https://cdn.test/f", name: "a.exe", bytes: b"", ok: false },
        ).unwrap_err();
        assert!(matches!(err, BrowseError::DownloadFailed { failed: 1 }), "{err}");
    }

    #[test]
    fn unavailable_window_is_flagged_for_fallback() {
        let data = tempfile::tempdir().unwrap();
        let err = run(data.path(), Ok(release(gated())), Script::Unavailable).unwrap_err();
        assert!(err.browser_unavailable(), "{err}");
        assert!(BrowseError::NotCompiledIn.browser_unavailable());
    }

    #[test]
    fn ungated_release_is_pointed_at_fetch() {
        let data = tempfile::tempdir().unwrap();
        let direct = DownloadAccess::Direct { url: "https://x.test/f".into(), expected_size: None, expected_sha256: None };
        let err = run(data.path(), Ok(release(direct)), Script::Close).unwrap_err();
        assert!(matches!(err, BrowseError::NotGated));
    }

    #[test]
    fn source_failure_opens_no_window() {
        let data = tempfile::tempdir().unwrap();
        // `Script::Download` would panic on `assert_eq!(page_url…)` if run were reached with no release.
        let err = run(data.path(), Err("layout changed".into()), Script::Close).unwrap_err();
        assert!(matches!(err, BrowseError::Source(_)), "{err}");
    }
}
