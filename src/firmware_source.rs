//! Firmware *sources*: where to look for the latest published firmware for a
//! model, and how to read the answer. Everything past `FirmwareSource::latest`
//! (comparison with the archive and the cameras, download, archiving) is
//! source-independent and lives in `assess`, `fetch_direct` and
//! `firmware::ingest_bundle`.
//!
//! The one implementation, `NikonDownloadCenter`, targets Nikon's Download
//! Center (`downloadcenter.nikonimglib.com`, the site nikonusa links to). What
//! we know about its structure, and why `DownloadAccess::Gated` is the only
//! thing it returns, is in docs/firmware-source-findings.md.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use scraper::{CaseSensitivity, Html, Selector};
use thiserror::Error;

use crate::firmware::{
    FirmwareError, IngestOutcome, Provenance, RetrievalMethod, ingest_bundle, sha256_file,
};

// ─── Version ─────────────────────────────────────────────────────────────────

/// A firmware version, compared numerically per component — `5.9 < 5.10`,
/// never as strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FwVersion {
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("empty version string")]
    Empty,
    #[error("{0:?} is not \"<major>.<minor>\" with decimal digits")]
    Malformed(String),
    #[error("{0:?} is not in canonical \"<major>.<minor:02>\" form (e.g. \"5.04\", not \"5.4\"); the minor is ambiguous")]
    NotCanonical(String),
}

impl FwVersion {
    /// Lenient: any `<digits>.<digits>`; `"5.4"` parses as 5.4 (i.e. minor 4).
    pub fn parse(s: &str) -> Result<Self, VersionError> {
        let t = s.trim();
        if t.is_empty() {
            return Err(VersionError::Empty);
        }
        let bad = || VersionError::Malformed(s.to_owned());
        let (maj, min) = t.split_once('.').ok_or_else(bad)?;
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if !digits(maj) || !digits(min) {
            return Err(bad());
        }
        Ok(FwVersion {
            major: maj.parse().map_err(|_| bad())?,
            minor: min.parse().map_err(|_| bad())?,
        })
    }

    /// Strict: the shape `bcd_decode_version` produces — minor exactly two
    /// digits, major without a leading zero. Everything we ingest from a
    /// camera, a page, or the archive goes through this so `"5.4"` can't be
    /// silently read as 5.04 or 5.40.
    pub fn parse_canonical(s: &str) -> Result<Self, VersionError> {
        let v = Self::parse(s)?;
        let t = s.trim();
        let (maj, min) = t.split_once('.').expect("parse() succeeded");
        if min.len() != 2 || (maj.len() > 1 && maj.starts_with('0')) {
            return Err(VersionError::NotCanonical(s.to_owned()));
        }
        Ok(v)
    }
}

impl fmt::Display for FwVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.major, self.minor)
    }
}

// ─── Source abstraction ──────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source {source_id} has no entry for model {model:?}")]
    UnsupportedModel { source_id: &'static str, model: String },
    #[error("HTTP request for {url} failed: {detail}")]
    Http { url: String, detail: String },
    #[error("could not read {url}: {reason}")]
    Parse { url: String, reason: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Blocking HTTP, abstracted so parsing and orchestration are testable offline.
pub trait HttpGet {
    fn get_text(&self, url: &str) -> Result<String, SourceError>;
    /// Stream `url` into `dest`; returns bytes written.
    fn get_to_file(&self, url: &str, dest: &Path) -> Result<u64, SourceError>;
}

pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent("nikon-fleet/0.1 (firmware check; +local)")
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build();
        UreqHttp { agent }
    }

    fn call(&self, url: &str) -> Result<ureq::Response, SourceError> {
        self.agent.get(url).call().map_err(|e| SourceError::Http {
            url: url.to_owned(),
            detail: match e {
                ureq::Error::Status(code, _) => format!("HTTP {code}"),
                ureq::Error::Transport(t) => t.to_string(),
            },
        })
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpGet for UreqHttp {
    fn get_text(&self, url: &str) -> Result<String, SourceError> {
        self.call(url)?.into_string().map_err(SourceError::from)
    }

    fn get_to_file(&self, url: &str, dest: &Path) -> Result<u64, SourceError> {
        let resp = self.call(url)?;
        let mut reader = resp.into_reader();
        let mut out = fs::File::create(dest)?;
        let n = std::io::copy(&mut reader, &mut out)?;
        out.flush()?;
        Ok(n)
    }
}

/// How the bundle for a release can be obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadAccess {
    /// A human must act first (licence click-through, login…). The tool
    /// must not automate it; `notice` says what the human has to do.
    Gated { notice: String },
    /// Plain URL, with whatever the source publishes to verify it.
    Direct {
        url: String,
        expected_size: Option<u64>,
        expected_sha256: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub model: String,
    pub version: FwVersion,
    /// ISO date `YYYY-MM-DD`, if the source gave a well-formed one.
    pub released: Option<String>,
    pub page_url: String,
    pub download: DownloadAccess,
    /// Non-fatal oddities (layout drift that didn't prevent an answer).
    pub warnings: Vec<String>,
    /// The page the answer came from, kept verbatim for the archive.
    pub raw_page: String,
}

pub trait FirmwareSource {
    fn id(&self) -> &'static str;
    /// Latest published firmware for `model` (canonical model id, e.g. "Z 9",
    /// "Z6_3"). An `Err` means *could not determine* — never "nothing new".
    fn latest(&self, model: &str, http: &dyn HttpGet) -> Result<Release, SourceError>;
}

// ─── Nikon Download Center ───────────────────────────────────────────────────

struct NikonModel {
    model: &'static str,
    /// Product name as the site titles it ("Z6III", not "Z6_3").
    product: &'static str,
    page: &'static str,
}

/// Single source of truth for model → product page. Ids observed 2026-10-27.
const NIKON_MODELS: &[NikonModel] = &[
    NikonModel { model: "Z 9", product: "Z 9", page: "/en/products/589/Z_9.html" },
    NikonModel { model: "Z6_3", product: "Z6III", page: "/en/products/629/Z6III.html" },
    NikonModel { model: "Z 5", product: "Z 5", page: "/en/products/552/Z_5.html" },
];

pub struct NikonDownloadCenter {
    base: String,
}

impl NikonDownloadCenter {
    pub const DEFAULT_BASE: &'static str = "https://downloadcenter.nikonimglib.com";

    pub fn new() -> Self {
        Self::with_base(Self::DEFAULT_BASE)
    }

    pub fn with_base(base: &str) -> Self {
        NikonDownloadCenter { base: base.trim_end_matches('/').to_owned() }
    }

    pub fn supported_models() -> impl Iterator<Item = &'static str> {
        NIKON_MODELS.iter().map(|m| m.model)
    }
}

impl Default for NikonDownloadCenter {
    fn default() -> Self {
        Self::new()
    }
}

/// What the product page's firmware row says.
#[derive(Debug, PartialEq, Eq)]
pub struct ProductRow {
    pub version: FwVersion,
    pub released: Option<String>,
    pub href: String,
    pub warnings: Vec<String>,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

fn text_of(el: scraper::ElementRef<'_>) -> String {
    el.text().collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn perr(url: &str, reason: impl Into<String>) -> SourceError {
    SourceError::Parse { url: url.to_owned(), reason: reason.into() }
}

/// `2026/10/20` → `2026-10-20`; anything else → `None`.
fn iso_date(s: &str) -> Option<String> {
    let mut parts = s.trim().split('/');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let num = |p: &str, len: usize| p.len() == len && p.bytes().all(|b| b.is_ascii_digit());
    if !(num(y, 4) && num(m, 2) && num(d, 2)) {
        return None;
    }
    let (mi, di): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&mi) || !(1..=31).contains(&di) {
        return None;
    }
    Some(format!("{y}-{m}-{d}"))
}

/// Parse a product page. Only rows inside `#firmware` whose title is exactly
/// `"<product> Firmware"` count: the page's `#software` section reuses the
/// same `col title version` markup for NX Studio etc. (`Full Ver. 2.00`), and
/// would otherwise be read as firmware.
pub fn parse_product_page(html: &str, product: &str, url: &str) -> Result<ProductRow, SourceError> {
    let doc = Html::parse_document(html);
    let want_title = format!("{product} Firmware");

    let page_title = doc.select(&sel("title")).next().map(text_of).unwrap_or_default();
    if !page_title.ends_with(&format!("| {product}")) {
        return Err(perr(url, format!("page title {page_title:?} is not for product {product:?}")));
    }

    let (row_sel, title_sel, ver_sel, date_sel, link_sel) = (
        sel("#firmware div.row"),
        sel("strong.col"),
        sel("span.version span.inner"),
        sel("span.date"),
        sel("span.link a"),
    );
    let mut matches = Vec::new();
    for row in doc.select(&row_sel) {
        if row.value().has_class("header", CaseSensitivity::CaseSensitive) {
            continue;
        }
        let title = row.select(&title_sel).next().map(text_of).unwrap_or_default();
        if title == want_title {
            matches.push(row);
        }
    }
    let row = match matches.as_slice() {
        [one] => *one,
        [] => return Err(perr(url, format!("no #firmware row titled {want_title:?}"))),
        many => return Err(perr(url, format!("{} #firmware rows titled {want_title:?}; refusing to guess", many.len()))),
    };

    let ver_text = row.select(&ver_sel).next().map(text_of)
        .ok_or_else(|| perr(url, "firmware row has no version cell"))?;
    let ver_str = ver_text.strip_prefix("C:Ver.").map(str::trim)
        .ok_or_else(|| perr(url, format!("version cell {ver_text:?} does not start with \"C:Ver.\"")))?;
    let version = FwVersion::parse_canonical(ver_str)
        .map_err(|e| perr(url, format!("version cell {ver_text:?}: {e}")))?;

    let mut warnings = Vec::new();
    let released = match row.select(&date_sel).next().map(text_of) {
        Some(d) => iso_date(&d).or_else(|| {
            warnings.push(format!("release date {d:?} not in YYYY/MM/DD form; ignored"));
            None
        }),
        None => {
            warnings.push("firmware row has no release-date cell".into());
            None
        }
    };

    // The row has the link twice (desktop + "forSMP" mobile); they must agree.
    let mut hrefs: Vec<&str> = row.select(&link_sel).filter_map(|a| a.value().attr("href")).collect();
    hrefs.sort_unstable();
    hrefs.dedup();
    let href = match hrefs.as_slice() {
        [h] => *h,
        [] => return Err(perr(url, "firmware row has no download-page link")),
        _ => return Err(perr(url, format!("firmware row links disagree: {hrefs:?}"))),
    };
    let ok_href = href.strip_prefix("/en/download/fw/").and_then(|r| r.strip_suffix(".html"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !ok_href {
        return Err(perr(url, format!("download-page link {href:?} is not /en/download/fw/<n>.html")));
    }

    Ok(ProductRow { version, released, href: href.to_owned(), warnings })
}

/// What the firmware (download) page says it delivers, and whether it still
/// describes the licence gate.
#[derive(Debug, PartialEq, Eq)]
pub struct FirmwarePage {
    pub version: FwVersion,
    pub gate_described: bool,
}

/// Read "...firmware to version X.YY" and the licence sentence off a firmware page.
pub fn parse_firmware_page(html: &str, url: &str) -> Result<FirmwarePage, SourceError> {
    let doc = Html::parse_document(html);
    let body = doc.select(&sel("body")).next().map(text_of)
        .ok_or_else(|| perr(url, "page has no <body>"))?;
    const MARK: &str = "firmware to version ";
    let at = body.find(MARK).ok_or_else(|| perr(url, "no \"firmware to version\" sentence"))?;
    let tail = &body[at + MARK.len()..];
    let ver: String = tail.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let ver = ver.trim_end_matches('.');
    let version = FwVersion::parse_canonical(ver)
        .map_err(|e| perr(url, format!("version after \"firmware to version\": {e}")))?;
    let gate_described = body.contains("acceptance of the agreement");
    Ok(FirmwarePage { version, gate_described })
}

impl FirmwareSource for NikonDownloadCenter {
    fn id(&self) -> &'static str {
        "nikon-download-center"
    }

    fn latest(&self, model: &str, http: &dyn HttpGet) -> Result<Release, SourceError> {
        let m = NIKON_MODELS.iter().find(|m| m.model == model)
            .ok_or_else(|| SourceError::UnsupportedModel { source_id: self.id(), model: model.to_owned() })?;

        let product_url = format!("{}{}", self.base, m.page);
        let product_html = http.get_text(&product_url)?;
        let row = parse_product_page(&product_html, m.product, &product_url)?;

        let page_url = format!("{}{}", self.base, row.href);
        let page_html = http.get_text(&page_url)?;
        let page = parse_firmware_page(&page_html, &page_url)?;
        if page.version != row.version {
            return Err(perr(&page_url, format!(
                "product page lists {} but the download page delivers {}", row.version, page.version
            )));
        }

        let mut warnings = row.warnings;
        if !page.gate_described {
            warnings.push(
                "licence-acceptance wording not found on the download page; layout may have \
                 changed — still treating the download as gated, verify by hand".into(),
            );
        }
        Ok(Release {
            model: model.to_owned(),
            version: row.version,
            released: row.released,
            page_url,
            download: DownloadAccess::Gated {
                notice: "Nikon requires accepting its licence agreement on the download page \
                         (choose OS, tick \"I agree\", \"Begin download\"). This tool does not do that for you."
                    .into(),
            },
            warnings,
            raw_page: page_html,
        })
    }
}

// ─── Source-independent logic ────────────────────────────────────────────────

/// Outcome of comparing a source's latest release with what's archived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// Published, and not in the archive.
    NewAvailable { latest: FwVersion, newest_archived: Option<FwVersion> },
    /// Published and already in the archive.
    AlreadyArchived { latest: FwVersion },
    /// The source could not be read; says nothing about whether anything is new.
    CouldNotDetermine { reason: String },
}

pub fn assess(latest: &Result<Release, SourceError>, archived: &[FwVersion]) -> ModelStatus {
    match latest {
        Err(e) => ModelStatus::CouldNotDetermine { reason: e.to_string() },
        Ok(r) if archived.contains(&r.version) => ModelStatus::AlreadyArchived { latest: r.version },
        Ok(r) => ModelStatus::NewAvailable {
            latest: r.version,
            newest_archived: archived.iter().copied().max(),
        },
    }
}

/// How a camera's reported firmware relates to the latest published one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraRelation {
    Behind,
    Current,
    /// Camera is newer than the source's "latest" (e.g. beta, or stale page).
    Ahead,
    Unreadable(String),
}

pub fn camera_relation(camera_fw: &str, latest: FwVersion) -> CameraRelation {
    match FwVersion::parse_canonical(camera_fw) {
        Err(e) => CameraRelation::Unreadable(e.to_string()),
        Ok(c) => match c.cmp(&latest) {
            Ordering::Less => CameraRelation::Behind,
            Ordering::Equal => CameraRelation::Current,
            Ordering::Greater => CameraRelation::Ahead,
        },
    }
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Archive(#[from] FirmwareError),
    #[error("downloaded {got} bytes but the source published {expected}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("downloaded sha256 {got} but the source published {expected}")]
    HashMismatch { expected: String, got: String },
    #[error("release for {0} is gated; it cannot be fetched automatically")]
    Gated(String),
}

/// Download a `Direct` release into a scratch file, verify what the source
/// publishes, then hand it to `ingest_bundle`. `Gated` is refused outright.
pub fn fetch_direct(
    http: &dyn HttpGet,
    source_id: &str,
    release: &Release,
    data_dir: &Path,
    scratch_dir: &Path,
    retrieved_at: &str,
) -> Result<IngestOutcome, FetchError> {
    let DownloadAccess::Direct { url, expected_size, expected_sha256 } = &release.download else {
        return Err(FetchError::Gated(release.model.clone()));
    };
    let tmp = scratch_dir.join("download.part");
    let got = http.get_to_file(url, &tmp)?;
    if let Some(expected) = *expected_size {
        if expected != got {
            return Err(FetchError::SizeMismatch { expected, got });
        }
    }
    let sha = sha256_file(&tmp).map_err(FirmwareError::from)?;
    if let Some(expected) = expected_sha256 {
        if !expected.eq_ignore_ascii_case(&sha) {
            return Err(FetchError::HashMismatch { expected: expected.clone(), got: sha });
        }
    }
    let prov = Provenance {
        source_id: source_id.to_owned(),
        source_url: url.clone(),
        page_url: release.page_url.clone(),
        released: release.released.clone(),
        retrieved_at: retrieved_at.to_owned(),
        method: RetrievalMethod::DirectDownload,
        published_size: *expected_size,
        published_sha256: expected_sha256.clone(),
        original_filename: None,
    };
    Ok(ingest_bundle(
        data_dir, &release.model, &release.version.to_string(), &tmp, &prov, Some(&release.raw_page),
    )?)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests (offline — fixtures are saved copies of the live pages)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    const P_Z9: &str = include_str!("../tests/fixtures/nikon-download-center/product-589-Z_9.html");
    const P_Z6: &str = include_str!("../tests/fixtures/nikon-download-center/product-629-Z6III.html");
    const P_Z5: &str = include_str!("../tests/fixtures/nikon-download-center/product-552-Z_5.html");
    const FW_Z9: &str = include_str!("../tests/fixtures/nikon-download-center/fw-607-Z_9.html");
    const FW_Z6: &str = include_str!("../tests/fixtures/nikon-download-center/fw-574-Z6III.html");

    fn v(s: &str) -> FwVersion {
        FwVersion::parse_canonical(s).unwrap()
    }

    // ── version parsing / ordering ──────────────────────────────────────────

    #[test]
    fn version_numeric_per_component() {
        assert!(v("5.30") < v("5.31"));
        assert!(v("5.31") == v("5.31"));
        assert!(v("5.33") > v("5.31"));
        assert!(v("10.00") > v("9.99")); // string compare would say "10.00" < "9.99"
        // lenient parse: 5.9 < 5.10 numerically (string compare says the opposite)
        assert!(FwVersion::parse("5.9").unwrap() < FwVersion::parse("5.10").unwrap());
        // 5.4 is minor 4, below 5.30 and 5.31
        assert!(FwVersion::parse("5.4").unwrap() < v("5.30"));
        assert!(FwVersion::parse("5.4").unwrap() < v("5.31"));
    }

    #[test]
    fn version_display_is_two_digit_minor() {
        assert_eq!(FwVersion { major: 2, minor: 0 }.to_string(), "2.00");
        assert_eq!(FwVersion { major: 5, minor: 33 }.to_string(), "5.33");
    }

    #[test]
    fn version_malformed_inputs_are_errors() {
        assert_eq!(FwVersion::parse(""), Err(VersionError::Empty));
        assert_eq!(FwVersion::parse("   "), Err(VersionError::Empty));
        for bad in ["5", "5.", ".31", "5.3.1", "v5.31", "5.31a", "a.b", "-1.00", "5,31", "99999999999.00"] {
            assert!(matches!(FwVersion::parse(bad), Err(VersionError::Malformed(_))), "{bad:?}");
        }
    }

    #[test]
    fn version_canonical_boundaries() {
        assert!(FwVersion::parse_canonical("5.31").is_ok());
        assert!(FwVersion::parse_canonical("5.04").is_ok());
        assert!(FwVersion::parse_canonical("10.00").is_ok());
        assert!(FwVersion::parse_canonical(" 2.00 ").is_ok());
        for bad in ["5.4", "5.310", "05.31", "5.3"] {
            assert!(matches!(FwVersion::parse_canonical(bad), Err(VersionError::NotCanonical(_))), "{bad:?}");
        }
        assert_eq!(FwVersion::parse_canonical(""), Err(VersionError::Empty));
    }

    // ── product-page parsing (real fixtures) ────────────────────────────────

    #[test]
    fn parses_z9_product_page() {
        let r = parse_product_page(P_Z9, "Z 9", "u").unwrap();
        assert_eq!(r.version, v("5.33"));
        assert_eq!(r.released.as_deref(), Some("2026-10-20"));
        assert_eq!(r.href, "/en/download/fw/607.html");
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn parses_z6iii_product_page() {
        let r = parse_product_page(P_Z6, "Z6III", "u").unwrap();
        assert_eq!(r.version, v("2.01"));
        assert_eq!(r.released.as_deref(), Some("2026-10-14"));
        assert_eq!(r.href, "/en/download/fw/574.html");
    }

    #[test]
    fn parses_z5_product_page() {
        let r = parse_product_page(P_Z5, "Z 5", "u").unwrap();
        assert_eq!(r.version, v("1.50"));
        assert_eq!(r.href, "/en/download/fw/562.html");
    }

    #[test]
    fn wrong_product_is_rejected_not_misread() {
        // Z 9 page asked for as Z6III: title check fires.
        let e = parse_product_page(P_Z9, "Z6III", "u").unwrap_err().to_string();
        assert!(e.contains("not for product"), "{e}");
        // "Z 5" must not match "Z 5" inside e.g. a "Z 50" page title (suffix match is exact).
        let fake = P_Z5.replace("| Z 5</title>", "| Z 50</title>");
        assert!(parse_product_page(&fake, "Z 5", "u").is_err());
    }

    #[test]
    fn software_rows_are_never_read_as_firmware() {
        // Rename the firmware row away; the software rows (Full Ver. 2.00 …)
        // share the same CSS classes and must not be picked up instead.
        let mangled = P_Z6.replace("Z6III Firmware</strong>", "Z6III Firmwar</strong>");
        let e = parse_product_page(&mangled, "Z6III", "u").unwrap_err().to_string();
        assert!(e.contains("no #firmware row"), "{e}");
    }

    #[test]
    fn duplicate_firmware_rows_refuse_to_guess() {
        let title_at = P_Z9.find("<strong class=\"col\">Z 9 Firmware</strong>").expect("row in fixture");
        let row_start = P_Z9[..title_at].rfind("<div class=\"row\">").expect("row div");
        let row_end = title_at + P_Z9[title_at..].find("<!-- .pseudoTable -->").expect("table end");
        let row = &P_Z9[row_start..row_end];
        let doubled = P_Z9.replacen(row, &format!("{row}{}", row.replace("C:Ver.5.33", "C:Ver.5.99")), 1);
        let e = parse_product_page(&doubled, "Z 9", "u").unwrap_err().to_string();
        assert!(e.contains("2 #firmware rows"), "{e}");
    }

    #[test]
    fn version_cell_boundaries() {
        let ok = |s: &str| parse_product_page(&P_Z9.replace("C:Ver.5.33", s), "Z 9", "u");
        assert!(ok("C:Ver.5.33").is_ok());
        assert!(ok("C:Ver.5.3").unwrap_err().to_string().contains("canonical"));   // ambiguous minor
        assert!(ok("Ver.5.33").unwrap_err().to_string().contains("C:Ver."));        // wrong stream/prefix
        assert!(ok("C:Ver.").is_err());                                             // empty
        assert!(ok("C:Ver.5.33.1").is_err());                                       // extra component
        assert!(ok("G:Ver.0.17").is_err());                                         // GNSS stream is not "C"
    }

    #[test]
    fn bad_date_is_a_warning_not_a_failure() {
        let r = parse_product_page(&P_Z9.replace("2026/10/20", "20 Oct 2026"), "Z 9", "u").unwrap();
        assert_eq!(r.released, None);
        assert_eq!(r.warnings.len(), 1);
        // boundary: impossible month / day
        assert_eq!(iso_date("2026/13/01"), None);
        assert_eq!(iso_date("2026/00/10"), None);
        assert_eq!(iso_date("2026/12/31").as_deref(), Some("2026-12-31"));
        assert_eq!(iso_date("2026/12/32"), None);
        assert_eq!(iso_date("2026/1/5"), None);
        assert_eq!(iso_date(""), None);
    }

    #[test]
    fn unexpected_link_shape_is_rejected() {
        let e = parse_product_page(&P_Z9.replace("/en/download/fw/607.html", "/en/download/sw/607.html"), "Z 9", "u");
        assert!(e.is_err());
    }

    #[test]
    fn empty_and_junk_pages_are_errors() {
        assert!(parse_product_page("", "Z 9", "u").is_err());
        assert!(parse_product_page("<html><title>Nikon | Download center | Z 9</title></html>", "Z 9", "u").is_err());
    }

    // ── firmware-page parsing ───────────────────────────────────────────────

    #[test]
    fn parses_firmware_pages() {
        let z9 = parse_firmware_page(FW_Z9, "u").unwrap();
        assert_eq!(z9, FirmwarePage { version: v("5.33"), gate_described: true });
        let z6 = parse_firmware_page(FW_Z6, "u").unwrap();
        assert_eq!(z6, FirmwarePage { version: v("2.01"), gate_described: true });
    }

    #[test]
    fn firmware_page_without_sentence_is_error() {
        assert!(parse_firmware_page("<html><body>nothing here</body></html>", "u").is_err());
    }

    // ── Nikon source end to end against canned HTTP ─────────────────────────

    struct Canned {
        pages: HashMap<String, Result<String, String>>,
        asked: Mutex<Vec<String>>,
    }
    impl Canned {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Canned {
                pages: pairs.iter().map(|(u, b)| (u.to_string(), Ok(b.to_string()))).collect(),
                asked: Mutex::new(Vec::new()),
            }
        }
    }
    impl HttpGet for Canned {
        fn get_text(&self, url: &str) -> Result<String, SourceError> {
            self.asked.lock().unwrap().push(url.to_owned());
            match self.pages.get(url) {
                Some(Ok(b)) => Ok(b.clone()),
                Some(Err(d)) => Err(SourceError::Http { url: url.to_owned(), detail: d.clone() }),
                None => Err(SourceError::Http { url: url.to_owned(), detail: "HTTP 404".into() }),
            }
        }
        fn get_to_file(&self, url: &str, dest: &Path) -> Result<u64, SourceError> {
            let body = self.get_text(url)?;
            fs::write(dest, body.as_bytes())?;
            Ok(body.len() as u64)
        }
    }

    const BASE: &str = "https://example.test";
    fn site() -> Canned {
        Canned::new(&[
            ("https://example.test/en/products/589/Z_9.html", P_Z9),
            ("https://example.test/en/download/fw/607.html", FW_Z9),
            ("https://example.test/en/products/629/Z6III.html", P_Z6),
            ("https://example.test/en/download/fw/574.html", FW_Z6),
        ])
    }

    #[test]
    fn nikon_latest_for_fleet_models() {
        let src = NikonDownloadCenter::with_base(BASE);
        let http = site();
        let z9 = src.latest("Z 9", &http).unwrap();
        assert_eq!((z9.version, z9.released.as_deref()), (v("5.33"), Some("2026-10-20")));
        assert_eq!(z9.page_url, "https://example.test/en/download/fw/607.html");
        assert!(matches!(z9.download, DownloadAccess::Gated { .. }), "Nikon downloads must stay gated");
        let z6 = src.latest("Z6_3", &http).unwrap();
        assert_eq!(z6.version, v("2.01"));
    }

    #[test]
    fn nikon_unknown_model_is_unsupported_error() {
        let src = NikonDownloadCenter::with_base(BASE);
        assert!(matches!(src.latest("Z 8", &site()), Err(SourceError::UnsupportedModel { .. })));
        assert!(matches!(src.latest("", &site()), Err(SourceError::UnsupportedModel { .. })));
        // "Z6III" is the site's name, not the canonical model id used here.
        assert!(matches!(src.latest("Z6III", &site()), Err(SourceError::UnsupportedModel { .. })));
    }

    #[test]
    fn nikon_product_page_and_download_page_must_agree() {
        let src = NikonDownloadCenter::with_base(BASE);
        let stale = FW_Z9.replace("firmware to version 5.33", "firmware to version 5.32");
        let http = Canned::new(&[
            ("https://example.test/en/products/589/Z_9.html", P_Z9),
            ("https://example.test/en/download/fw/607.html", &stale),
        ]);
        let e = src.latest("Z 9", &http).unwrap_err().to_string();
        assert!(e.contains("5.33") && e.contains("5.32"), "{e}");
    }

    #[test]
    fn nikon_http_failure_is_an_error_not_empty() {
        let src = NikonDownloadCenter::with_base(BASE);
        let http = Canned::new(&[]);
        assert!(matches!(src.latest("Z 9", &http), Err(SourceError::Http { .. })));
    }

    #[test]
    fn nikon_missing_gate_wording_warns_but_stays_gated() {
        let src = NikonDownloadCenter::with_base(BASE);
        let no_gate = FW_Z9.replace("acceptance of the agreement", "acceptance of nothing");
        let http = Canned::new(&[
            ("https://example.test/en/products/589/Z_9.html", P_Z9),
            ("https://example.test/en/download/fw/607.html", &no_gate),
        ]);
        let r = src.latest("Z 9", &http).unwrap();
        assert!(matches!(r.download, DownloadAccess::Gated { .. }));
        assert!(r.warnings.iter().any(|w| w.contains("licence")), "{:?}", r.warnings);
    }

    // ── assess / camera_relation ────────────────────────────────────────────

    fn release(ver: &str) -> Release {
        Release {
            model: "Z 9".into(), version: v(ver), released: None, page_url: "p".into(),
            download: DownloadAccess::Gated { notice: String::new() }, warnings: vec![], raw_page: String::new(),
        }
    }

    #[test]
    fn assess_new_vs_archived_vs_unknown() {
        let l = Ok(release("5.33"));
        assert_eq!(assess(&l, &[]), ModelStatus::NewAvailable { latest: v("5.33"), newest_archived: None });
        assert_eq!(
            assess(&l, &[v("5.31"), v("5.32")]),
            ModelStatus::NewAvailable { latest: v("5.33"), newest_archived: Some(v("5.32")) }
        );
        assert_eq!(assess(&l, &[v("5.31"), v("5.33")]), ModelStatus::AlreadyArchived { latest: v("5.33") });
        // Archive holds only OLDER versions → still new. Archive holds a NEWER one → latest still not archived.
        assert!(matches!(assess(&l, &[v("5.34")]), ModelStatus::NewAvailable { .. }));
        let bad: Result<Release, SourceError> = Err(SourceError::Http { url: "u".into(), detail: "HTTP 503".into() });
        assert!(matches!(assess(&bad, &[v("5.33")]), ModelStatus::CouldNotDetermine { .. }),
            "a source failure must never read as 'already archived' or 'nothing new'");
    }

    #[test]
    fn camera_relation_boundaries() {
        let latest = v("5.33");
        assert_eq!(camera_relation("5.32", latest), CameraRelation::Behind);
        assert_eq!(camera_relation("5.33", latest), CameraRelation::Current);
        assert_eq!(camera_relation("5.34", latest), CameraRelation::Ahead);
        assert_eq!(camera_relation("5.9", v("5.10")).clone(), CameraRelation::Unreadable(
            VersionError::NotCanonical("5.9".into()).to_string()));
        assert!(matches!(camera_relation("", latest), CameraRelation::Unreadable(_)));
    }

    // ── Direct-download path (mock; Nikon never uses it) ────────────────────

    fn direct_release(url: &str, size: Option<u64>, sha: Option<&str>) -> Release {
        let mut r = release("5.33");
        r.download = DownloadAccess::Direct { url: url.into(), expected_size: size, expected_sha256: sha.map(String::from) };
        r
    }

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn direct_fetch_verifies_and_archives() {
        let (data, scratch) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let http = Canned::new(&[("https://x.test/a.bin", "hello")]);
        let out = fetch_direct(&http, "t", &direct_release("https://x.test/a.bin", Some(5), Some(HELLO_SHA)),
            data.path(), scratch.path(), "2026-10-27T00:00:00Z").unwrap();
        assert!(matches!(out, IngestOutcome::Archived(_)));
        let meta = crate::firmware::load_meta(data.path(), "Z 9", "5.33").unwrap();
        assert_eq!(meta.bin_sha256, HELLO_SHA);
        let p = meta.provenance.unwrap();
        assert_eq!(p.method, RetrievalMethod::DirectDownload);
        assert_eq!(p.published_sha256.as_deref(), Some(HELLO_SHA));
    }

    #[test]
    fn direct_fetch_size_and_hash_mismatch_archive_nothing() {
        let (data, scratch) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let http = Canned::new(&[("https://x.test/a.bin", "hello")]);
        // size just below / just above the real 5 bytes
        for sz in [4u64, 6] {
            let e = fetch_direct(&http, "t", &direct_release("https://x.test/a.bin", Some(sz), None),
                data.path(), scratch.path(), "t").unwrap_err();
            assert!(matches!(e, FetchError::SizeMismatch { .. }), "{e:?}");
        }
        let e = fetch_direct(&http, "t", &direct_release("https://x.test/a.bin", Some(5), Some("00")),
            data.path(), scratch.path(), "t").unwrap_err();
        assert!(matches!(e, FetchError::HashMismatch { .. }), "{e:?}");
        assert!(!crate::firmware::archive_exists(data.path(), "Z 9", "5.33"));
    }

    #[test]
    fn gated_release_is_refused_by_fetch_direct() {
        let (data, scratch) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let e = fetch_direct(&Canned::new(&[]), "t", &release("5.33"), data.path(), scratch.path(), "t").unwrap_err();
        assert!(matches!(e, FetchError::Gated(_)));
    }
}
