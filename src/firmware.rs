//! Firmware archive: store and retrieve Nikon firmware binaries keyed by
//! (model, version). Each entry lives at:
//!
//!   <data-dir>/firmware/{model_slug}/{version}/
//!     ├── firmware.bin
//!     └── metadata.json

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const FIRMWARE_META_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirmwareMeta {
    pub format_version: u32,
    pub model: String,
    pub firmware_version: String,
    pub archived_at: String,
    pub archived_by: String,
    pub bin_sha256: String,
    pub bin_size_bytes: u64,
    pub source_path: String,
    pub canonical_snapshot_path: Option<String>,
    pub notes: Option<String>,
    /// Where the bundle came from. Absent on entries made by `fleet firmware
    /// add` before provenance existed (additive: `format_version` stays 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// How the bytes reached the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrievalMethod {
    /// The user accepted the source's licence/gate themselves, downloaded the
    /// file, and handed it to `fleet firmware fetch --from-file`.
    ManualAfterGate,
    /// The tool downloaded it from an ungated URL.
    DirectDownload,
    /// The user accepted the gate and clicked Download themselves inside the
    /// embedded browser (`fleet firmware browse`); the tool only chose where
    /// the file landed, so `source_url` is the real file URL.
    BrowserAfterGate,
}

/// Provenance recorded for bundles that came from a firmware source.
/// `bin_sha256`/`bin_size_bytes` in `FirmwareMeta` are always *computed* from
/// the stored bytes; `published_*` are what the source claimed (if anything).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source_id: String,
    /// URL the bytes were fetched from (equals `page_url` for manual retrieval,
    /// where the tool never sees the file URL).
    pub source_url: String,
    pub page_url: String,
    /// ISO date the source gave for the release, if any.
    pub released: Option<String>,
    pub retrieved_at: String,
    pub method: RetrievalMethod,
    pub published_size: Option<u64>,
    pub published_sha256: Option<String>,
    /// File name the source gave the download (e.g. an installer name), when
    /// the tool saw it. The archive stores the bytes as `firmware.bin`, so
    /// this is the only record of what the file was called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_filename: Option<String>,
}

#[derive(Debug, Error)]
pub enum FirmwareError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported format_version {found}; expected {expected}")]
    UnsupportedVersion { found: u32, expected: u32 },
    #[error("firmware archive already exists for {model} {version} at {path}")]
    AlreadyArchived { model: String, version: String, path: String },
    #[error("no firmware archive found for {model} {version}")]
    NotFound { model: String, version: String },
    #[error("SHA-256 mismatch: stored={stored}, computed={computed}")]
    ChecksumMismatch { stored: String, computed: String },
    #[error(
        "{model} {version} is already archived with different bytes (archived sha256 {existing}, \
         new {new}); refusing to overwrite — the same version number should be the same file"
    )]
    ContentMismatch { model: String, version: String, existing: String, new: String },
    #[error("{path} holds a firmware.bin without a metadata.json (interrupted archive?); inspect and remove it by hand")]
    IncompleteArchive { path: String },
    #[error("{what} {value:?} is not a single plain path component")]
    UnsafePathComponent { what: &'static str, value: String },
}

// ─── Identity helpers ────────────────────────────────────────────────────────

/// Convert a model identifier to a filesystem-safe slug: spaces → underscores.
pub fn model_slug(model: &str) -> String {
    model.replace(' ', "_")
}

/// Strip the `"NIKON DSC "` prefix from a USB product string.
pub fn model_from_usb_product(usb_product: &str) -> &str {
    usb_product.strip_prefix("NIKON DSC ").unwrap_or(usb_product)
}

/// Re-export so callers don't need to touch sdk directly.
pub use crate::sdk::bcd_decode_version;

// ─── Layout ─────────────────────────────────────────────────────────────────

/// `<data-dir>/firmware/`
pub fn firmware_root(data_dir: &Path) -> PathBuf {
    data_dir.join("firmware")
}

/// `<data-dir>/firmware/{model_slug}/{version}/`
pub fn archive_dir(data_dir: &Path, model: &str, version: &str) -> PathBuf {
    firmware_root(data_dir).join(model_slug(model)).join(version)
}

// ─── Metadata I/O ───────────────────────────────────────────────────────────

pub fn load_meta(data_dir: &Path, model: &str, version: &str) -> Result<FirmwareMeta, FirmwareError> {
    let path = archive_dir(data_dir, model, version).join("metadata.json");
    if !path.exists() {
        return Err(FirmwareError::NotFound {
            model: model.to_owned(),
            version: version.to_owned(),
        });
    }
    let text = fs::read_to_string(&path)?;
    let meta: FirmwareMeta = serde_json::from_str(&text)?;
    if meta.format_version != FIRMWARE_META_FORMAT_VERSION {
        return Err(FirmwareError::UnsupportedVersion {
            found: meta.format_version,
            expected: FIRMWARE_META_FORMAT_VERSION,
        });
    }
    Ok(meta)
}

/// Write metadata.json into `dir` (the archive directory, already created).
pub fn save_meta(meta: &FirmwareMeta, dir: &Path) -> Result<(), FirmwareError> {
    let path = dir.join("metadata.json");
    fs::write(path, serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

// ─── Utilities ───────────────────────────────────────────────────────────────

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn format_size(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ─── Ingest (source-independent archiving) ───────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum IngestOutcome {
    Archived(FirmwareMeta),
    /// Same version, same bytes already archived; nothing written.
    AlreadyPresent(FirmwareMeta),
}

fn safe_component(what: &'static str, value: &str) -> Result<(), FirmwareError> {
    // The library's one definition of a plain path component (ops::paths); the NUL check
    // this module's own copy lacked comes with it.
    if crate::ops::paths::is_safe_path_component(value) {
        Ok(())
    } else {
        Err(FirmwareError::UnsafePathComponent { what, value: value.to_owned() })
    }
}

/// Archive `src` as `(model, version)` with provenance. Never replaces an
/// existing bundle: identical bytes are a no-op, different bytes are
/// `ContentMismatch`. `bin` is written to a temp name, re-hashed, and renamed
/// into place, and `metadata.json` (the entry's existence marker, see
/// `archive_exists`) is written last, so a crash never leaves a half entry
/// that looks complete.
pub fn ingest_bundle(
    data_dir: &Path,
    model: &str,
    version: &str,
    src: &Path,
    provenance: &Provenance,
    source_page: Option<&str>,
) -> Result<IngestOutcome, FirmwareError> {
    safe_component("model", &model_slug(model))?;
    safe_component("version", version)?;

    if fs::metadata(src)?.len() == 0 {
        return Err(FirmwareError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput, format!("{} is empty; not archiving", src.display()),
        )));
    }
    let new_sha = sha256_file(src)?;
    let dir = archive_dir(data_dir, model, version);
    let bin = dir.join("firmware.bin");

    if dir.join("metadata.json").exists() {
        let meta = load_meta(data_dir, model, version)?;
        let existing = sha256_file(&bin)?;
        if existing != meta.bin_sha256 {
            return Err(FirmwareError::ChecksumMismatch { stored: meta.bin_sha256, computed: existing });
        }
        return if existing == new_sha {
            Ok(IngestOutcome::AlreadyPresent(meta))
        } else {
            Err(FirmwareError::ContentMismatch {
                model: model.to_owned(), version: version.to_owned(), existing, new: new_sha,
            })
        };
    }
    if bin.exists() {
        return Err(FirmwareError::IncompleteArchive { path: dir.display().to_string() });
    }

    fs::create_dir_all(&dir)?;
    let part = dir.join("firmware.bin.part");
    fs::copy(src, &part)?;
    let copied_sha = sha256_file(&part)?;
    if copied_sha != new_sha {
        let _ = fs::remove_file(&part); // best effort; the mismatch below is the real error
        return Err(FirmwareError::ChecksumMismatch { stored: new_sha, computed: copied_sha });
    }
    let size = fs::metadata(&part)?.len();
    fs::rename(&part, &bin)?;
    if let Some(html) = source_page {
        fs::write(dir.join("source-page.html"), html)?;
    }

    let meta = FirmwareMeta {
        format_version: FIRMWARE_META_FORMAT_VERSION,
        model: model.to_owned(),
        firmware_version: version.to_owned(),
        archived_at: provenance.retrieved_at.clone(),
        archived_by: "fleet firmware fetch".into(),
        bin_sha256: new_sha,
        bin_size_bytes: size,
        source_path: src.display().to_string(),
        canonical_snapshot_path: None,
        notes: None,
        provenance: Some(provenance.clone()),
    };
    save_meta(&meta, &dir)?;
    Ok(IngestOutcome::Archived(meta))
}

// ─── Listing ─────────────────────────────────────────────────────────────────

/// One valid archive entry: what `metadata.json` says, and where it lives.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveEntry {
    pub model: String,
    pub version: String,
    pub meta: FirmwareMeta,
    /// The entry's directory (`<data-dir>/firmware/{model_slug}/{version}/`, as found on disk).
    pub directory: PathBuf,
    /// `firmware.bin` exists in `directory`. `metadata.json` is the entry's existence marker, so
    /// an entry whose binary was deleted by hand is still listed, and says so here.
    pub bin_present: bool,
}

/// Why a directory under `firmware/` that looks like an entry is not listed as one.
/// Closed vocabulary for structured output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveSkipKind {
    /// `metadata.json` exists but could not be read.
    MetadataUnreadable,
    /// `metadata.json` is not valid metadata JSON.
    MetadataCorrupt,
    /// `metadata.json` has a `format_version` this build does not read.
    UnsupportedVersion,
    /// A `firmware.bin` (or its `.part` temp) with no `metadata.json`: an interrupted archive.
    IncompleteArchive,
}

/// A directory that was not listed, and why. Never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSkip {
    pub directory: PathBuf,
    pub kind: ArchiveSkipKind,
    pub error: String,
    /// The `{model_slug}` directory the skipped entry sits in, for the model filter.
    model_dir: String,
}

/// Everything one walk of `firmware/` found.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveScan {
    /// `<data-dir>/firmware/`
    pub root: PathBuf,
    pub root_exists: bool,
    /// Valid entries, sorted by model then version.
    pub entries: Vec<ArchiveEntry>,
    /// Entries that could not be listed, sorted by directory.
    pub skipped: Vec<ArchiveSkip>,
    /// Directory entries the OS failed to yield. Counted, never silently dropped.
    pub skipped_dir_entries: usize,
}

impl ArchiveScan {
    /// Say each skipped entry on stderr, in the form the CLI has always used.
    pub fn warn_skipped(&self) {
        for skip in &self.skipped {
            eprintln!("warning: skipping {}: {}", skip.directory.display(), skip.error);
        }
        if self.skipped_dir_entries > 0 {
            eprintln!(
                "warning: {} director{} under {} could not be read and {} not listed",
                self.skipped_dir_entries,
                if self.skipped_dir_entries == 1 { "y" } else { "ies" },
                self.root.display(),
                if self.skipped_dir_entries == 1 { "was" } else { "were" },
            );
        }
    }
}

/// Walk `firmware/`: every valid entry (model and version are read from each
/// `metadata.json`, not the directory names, so the display name uses
/// canonical spacing) and every directory that looks like an entry but could
/// not be listed. `model_filter` keeps only that model's entries, and the
/// skipped ones in that model's directory. This is the one validation rule for
/// the archive: [`list_archives`] and `fleet firmware ls --json` both use it.
pub fn scan_archives(data_dir: &Path, model_filter: Option<&str>) -> ArchiveScan {
    let root = firmware_root(data_dir);
    let mut scan = ArchiveScan {
        root_exists: root.exists(),
        root,
        entries: Vec::new(),
        skipped: Vec::new(),
        skipped_dir_entries: 0,
    };
    if !scan.root_exists {
        return scan;
    }
    // A root that exists but cannot be read is an empty walk with one skipped directory entry,
    // not an empty archive.
    let Ok(model_dirs) = fs::read_dir(&scan.root) else {
        scan.skipped_dir_entries = 1;
        return scan;
    };
    let filter_slug = model_filter.map(model_slug);

    for model_dir in model_dirs {
        let Ok(model_dir) = model_dir else {
            scan.skipped_dir_entries += 1;
            continue;
        };
        if !model_dir.path().is_dir() {
            continue;
        }
        let model_dir_name = model_dir.file_name().to_string_lossy().into_owned();
        let Ok(version_dirs) = fs::read_dir(model_dir.path()) else {
            scan.skipped_dir_entries += 1;
            continue;
        };

        for version_dir in version_dirs {
            let Ok(version_dir) = version_dir else {
                scan.skipped_dir_entries += 1;
                continue;
            };
            let directory = version_dir.path();
            if !directory.is_dir() {
                continue;
            }
            let meta_path = directory.join("metadata.json");
            let mut skip = |kind: ArchiveSkipKind, error: String| {
                scan.skipped.push(ArchiveSkip {
                    directory: directory.clone(),
                    kind,
                    error,
                    model_dir: model_dir_name.clone(),
                });
            };
            if !meta_path.exists() {
                if directory.join("firmware.bin").exists() || directory.join("firmware.bin.part").exists() {
                    skip(
                        ArchiveSkipKind::IncompleteArchive,
                        "a firmware binary without a metadata.json (interrupted archive?)".to_string(),
                    );
                }
                continue;
            }
            let text = match fs::read_to_string(&meta_path) {
                Ok(t) => t,
                Err(e) => {
                    skip(ArchiveSkipKind::MetadataUnreadable, format!("could not read {}: {e}", meta_path.display()));
                    continue;
                }
            };
            let meta = match serde_json::from_str::<FirmwareMeta>(&text) {
                Ok(m) => m,
                Err(e) => {
                    skip(ArchiveSkipKind::MetadataCorrupt, format!("corrupt metadata at {}: {e}", meta_path.display()));
                    continue;
                }
            };
            if meta.format_version != FIRMWARE_META_FORMAT_VERSION {
                skip(
                    ArchiveSkipKind::UnsupportedVersion,
                    format!(
                        "{} has format_version={}, expected {}",
                        meta_path.display(), meta.format_version, FIRMWARE_META_FORMAT_VERSION
                    ),
                );
                continue;
            }
            if let Some(f) = model_filter {
                if meta.model != f {
                    continue;
                }
            }
            scan.entries.push(ArchiveEntry {
                model: meta.model.clone(),
                version: meta.firmware_version.clone(),
                bin_present: directory.join("firmware.bin").is_file(),
                directory,
                meta,
            });
        }
    }

    if let Some(slug) = &filter_slug {
        scan.skipped.retain(|s| &s.model_dir == slug);
    }
    scan.skipped.sort_by(|a, b| a.directory.cmp(&b.directory));
    scan.entries.sort_by(|a, b| {
        a.model.cmp(&b.model).then_with(|| version_sort_key(&a.version).cmp(&version_sort_key(&b.version)))
    });
    scan
}

/// `fleet firmware ls --json`: the same envelope as `ls --json` (`dir`, `dir_exists`, `entries`,
/// `skipped_dir_entries`), built from one [`ArchiveScan`]. Entries that could not be listed are in
/// `entries` too, with `status: "unreadable"` and a closed `error_kind`, as in `ls --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArchiveListing {
    pub dir: String,
    pub dir_exists: bool,
    /// Valid entries first (model, then version), then the unreadable ones (by directory).
    pub entries: Vec<ArchiveListEntry>,
    pub skipped_dir_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ArchiveListEntry {
    Ok {
        model: String,
        version: String,
        directory: String,
        /// `firmware.bin` exists in `directory`.
        bin_present: bool,
        /// As recorded when the bundle was archived; nothing here re-hashes the file.
        bin_sha256: String,
        bin_size_bytes: u64,
        archived_at: String,
        archived_by: String,
        canonical_snapshot_path: Option<String>,
        notes: Option<String>,
        provenance: Option<Provenance>,
    },
    Unreadable {
        directory: String,
        error_kind: ArchiveSkipKind,
        error: String,
    },
}

impl ArchiveListing {
    pub fn from_scan(scan: &ArchiveScan) -> Self {
        let ok = scan.entries.iter().map(|e| ArchiveListEntry::Ok {
            model: e.model.clone(),
            version: e.version.clone(),
            directory: e.directory.display().to_string(),
            bin_present: e.bin_present,
            bin_sha256: e.meta.bin_sha256.clone(),
            bin_size_bytes: e.meta.bin_size_bytes,
            archived_at: e.meta.archived_at.clone(),
            archived_by: e.meta.archived_by.clone(),
            canonical_snapshot_path: e.meta.canonical_snapshot_path.clone(),
            notes: e.meta.notes.clone(),
            provenance: e.meta.provenance.clone(),
        });
        let bad = scan.skipped.iter().map(|s| ArchiveListEntry::Unreadable {
            directory: s.directory.display().to_string(),
            error_kind: s.kind,
            error: s.error.clone(),
        });
        ArchiveListing {
            dir: scan.root.display().to_string(),
            dir_exists: scan.root_exists,
            entries: ok.chain(bad).collect(),
            skipped_dir_entries: scan.skipped_dir_entries,
        }
    }
}

/// The valid entries of [`scan_archives`], sorted by model then version, with a stderr warning
/// for each skipped one. Callers that must account for the skipped ones use `scan_archives`.
pub fn list_archives(data_dir: &Path, model_filter: Option<&str>) -> Vec<ArchiveEntry> {
    let scan = scan_archives(data_dir, model_filter);
    scan.warn_skipped();
    scan.entries
}

/// Sort key for firmware version strings like `"5.31"` — numeric (major,
/// minor), so `"10.00"` sorts after `"5.31"` (a plain string compare would
/// put `"10.00"` first, since `'1' < '5'`). `firmware_version` is a
/// free-typed CLI argument, not schema-validated, so anything that doesn't
/// parse as `major.minor` sorts after every numeric version instead of
/// erroring — deterministic, not a crash on unexpected input.
fn version_sort_key(version: &str) -> (bool, u32, u32, &str) {
    let mut parts = version.splitn(2, '.');
    let major = parts.next().and_then(|s| s.parse::<u32>().ok());
    let minor = parts.next().and_then(|s| s.parse::<u32>().ok());
    match (major, minor) {
        (Some(maj), Some(min)) => (false, maj, min, version),
        _ => (true, 0, 0, version),
    }
}

/// Check whether an archive entry exists for (model, version).
pub fn archive_exists(data_dir: &Path, model: &str, version: &str) -> bool {
    archive_dir(data_dir, model, version).join("metadata.json").exists()
}

// ─── Schema helpers (used by main.rs for diff annotation) ────────────────────

/// Extract the integer major version from a firmware version string.
/// `"5.31"` → `Some(5)`, `"common"` → `None`.
pub fn schema_major(firmware_version: &str) -> Option<u32> {
    firmware_version.split('.').next()?.parse().ok()
}

/// Build a `name → allowed_ops` map for all schema capabilities available at
/// a given firmware major version, including "common" sections.
pub fn capability_map_for_fw(
    schema: &crate::maid_layer::MaidLayerConfig,
    model: &str,
    fw_major: u32,
) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    for section in schema.sections_for_model(model) {
        let include = match schema_major(&section.version) {
            Some(m) => m <= fw_major,
            None => true,  // "common" and other non-numeric versions always included
        };
        if include {
            for cap in &section.capabilities {
                map.insert(cap.name.clone(), cap.allowed_ops);
            }
        }
    }
    map
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // ── model_slug ──────────────────────────────────────────────────────────

    #[test]
    fn slug_replaces_spaces() {
        assert_eq!(model_slug("Z 9"),   "Z_9");
        assert_eq!(model_slug("Z6_3"),  "Z6_3");
        assert_eq!(model_slug("Z 5"),   "Z_5");
        assert_eq!(model_slug("Z 30"),  "Z_30");
    }

    #[test]
    fn slug_collision_pinned() {
        // "Z 9" and a hypothetical raw "Z_9" model string slug to the same
        // value — pinning this as documented current behavior. Rests on
        // every real Nikon USB product string in this codebase using
        // spaces, never underscores; re-check if that domain assumption
        // ever loosens (synthetic/injected model names, third-party bodies).
        assert_eq!(model_slug("Z 9"), model_slug("Z_9"));
    }

    // ── model_from_usb_product ──────────────────────────────────────────────

    #[test]
    fn strips_nikon_dsc_prefix() {
        assert_eq!(model_from_usb_product("NIKON DSC Z 9"), "Z 9");
        assert_eq!(model_from_usb_product("NIKON DSC Z6_3"), "Z6_3");
        assert_eq!(model_from_usb_product("NIKON DSC Z 5"), "Z 5");
    }

    #[test]
    fn no_prefix_returns_input() {
        assert_eq!(model_from_usb_product("Z 9"), "Z 9");
        assert_eq!(model_from_usb_product(""), "");
    }

    // ── schema_major ────────────────────────────────────────────────────────

    #[test]
    fn schema_major_extracts_integer() {
        assert_eq!(schema_major("5.31"), Some(5));
        assert_eq!(schema_major("2.00"), Some(2));
        assert_eq!(schema_major("1.43"), Some(1));
    }

    #[test]
    fn schema_major_non_numeric_returns_none() {
        assert_eq!(schema_major("common"), None);
        assert_eq!(schema_major(""), None);
    }

    // ── format_size ─────────────────────────────────────────────────────────

    #[test]
    fn format_size_mib() {
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(100 * 1024 * 1024 + 400 * 1024), "100.4 MiB");
    }

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(0),   "0 B");
    }

    #[test]
    fn format_size_just_below_mib() {
        // Surviving mutant: `>=` → `>` would make 1 MiB-1 fall into MiB branch.
        assert_eq!(format_size(1024 * 1024 - 1), "1048575 B");
    }

    // ── sha256_file ─────────────────────────────────────────────────────────

    #[test]
    fn sha256_of_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.bin");
        fs::File::create(&path).unwrap();
        let hash = sha256_file(&path).unwrap();
        // SHA-256 of empty input is well-known
        assert_eq!(hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn sha256_of_known_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.bin");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"hello").unwrap();
        let hash = sha256_file(&path).unwrap();
        assert_eq!(hash, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    // ── load_meta / save_meta round-trip ────────────────────────────────────

    #[test]
    fn meta_round_trip() {
        // data_dir = TempDir root; archive lives at data_dir/firmware/Z_9/5.31/
        let data_dir = TempDir::new().unwrap();
        let adir = archive_dir(data_dir.path(), "Z 9", "5.31");
        fs::create_dir_all(&adir).unwrap();

        let meta = FirmwareMeta {
            format_version: FIRMWARE_META_FORMAT_VERSION,
            model: "Z 9".into(),
            firmware_version: "5.31".into(),
            archived_at: "2026-06-03T00:00:00Z".into(),
            archived_by: "fleet firmware add".into(),
            bin_sha256: "abc123".into(),
            bin_size_bytes: 104857600,
            source_path: "/tmp/Z9FW0531.BIN".into(),
            canonical_snapshot_path: None,
            notes: None,
            provenance: None,
        };
        save_meta(&meta, &adir).unwrap();
        let loaded = load_meta(data_dir.path(), "Z 9", "5.31").unwrap();
        assert_eq!(meta, loaded);
    }

    #[test]
    fn load_meta_missing_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let err = load_meta(dir.path(), "Z 9", "5.31").unwrap_err();
        assert!(matches!(err, FirmwareError::NotFound { .. }));
    }

    // ── archive_exists ───────────────────────────────────────────────────────

    #[test]
    fn archive_exists_detects_presence() {
        let dir = TempDir::new().unwrap();
        assert!(!archive_exists(dir.path(), "Z 9", "5.31"));
        let adir = archive_dir(dir.path(), "Z 9", "5.31");
        fs::create_dir_all(&adir).unwrap();
        fs::write(adir.join("metadata.json"), "{}").unwrap();
        assert!(archive_exists(dir.path(), "Z 9", "5.31"));
    }

    // ── load_meta version guard ──────────────────────────────────────────

    #[test]
    fn load_meta_wrong_version_returns_error() {
        let data_dir = TempDir::new().unwrap();
        let adir = archive_dir(data_dir.path(), "Z 9", "5.31");
        fs::create_dir_all(&adir).unwrap();
        let bad = FirmwareMeta {
            format_version: 99,  // unsupported
            model: "Z 9".into(),
            firmware_version: "5.31".into(),
            archived_at: "2026-06-03T00:00:00Z".into(),
            archived_by: "fleet firmware add".into(),
            bin_sha256: "abc".into(),
            bin_size_bytes: 0,
            source_path: "".into(),
            canonical_snapshot_path: None,
            notes: None,
            provenance: None,
        };
        fs::write(adir.join("metadata.json"), serde_json::to_string(&bad).unwrap()).unwrap();
        let err = load_meta(data_dir.path(), "Z 9", "5.31").unwrap_err();
        assert!(
            matches!(err, FirmwareError::UnsupportedVersion { found: 99, expected: 1 }),
            "expected UnsupportedVersion, got {err:?}"
        );
    }

    // ── list_archives filtering and corrupt-entry handling ───────────────

    fn write_meta(data_dir: &Path, model: &str, version: &str) {
        let adir = archive_dir(data_dir, model, version);
        fs::create_dir_all(&adir).unwrap();
        let meta = FirmwareMeta {
            format_version: FIRMWARE_META_FORMAT_VERSION,
            model: model.to_owned(),
            firmware_version: version.to_owned(),
            archived_at: "2026-06-03T00:00:00Z".into(),
            archived_by: "fleet firmware add".into(),
            bin_sha256: "abc".into(),
            bin_size_bytes: 0,
            source_path: "".into(),
            canonical_snapshot_path: None,
            notes: None,
            provenance: None,
        };
        fs::write(adir.join("metadata.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    }

    #[test]
    fn list_archives_model_filter() {
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "5.31");
        write_meta(dir.path(), "Z 9", "4.00");
        write_meta(dir.path(), "Z6_3", "2.00");

        let all = list_archives(dir.path(), None);
        assert_eq!(all.len(), 3);

        let z9_only = list_archives(dir.path(), Some("Z 9"));
        assert_eq!(z9_only.len(), 2);
        assert!(z9_only.iter().all(|e| e.model == "Z 9"));

        let z6_only = list_archives(dir.path(), Some("Z6_3"));
        assert_eq!(z6_only.len(), 1);
        assert_eq!(z6_only[0].version, "2.00");
    }

    #[test]
    fn list_archives_skips_corrupt_metadata() {
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "5.31");
        // Add a directory with invalid JSON.
        let bad_dir = archive_dir(dir.path(), "Z 9", "4.00");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("metadata.json"), b"not valid json").unwrap();

        let entries = list_archives(dir.path(), None);
        assert_eq!(entries.len(), 1, "corrupt entry should be silently skipped");
        assert_eq!(entries[0].version, "5.31");
    }

    #[test]
    fn list_archives_sorted_by_model_then_version() {
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "5.31");
        write_meta(dir.path(), "Z 9", "4.00");
        write_meta(dir.path(), "Z6_3", "2.00");

        let entries = list_archives(dir.path(), None);
        let keys: Vec<(&str, &str)> = entries.iter().map(|e| (e.model.as_str(), e.version.as_str())).collect();
        assert_eq!(keys, vec![("Z 9", "4.00"), ("Z 9", "5.31"), ("Z6_3", "2.00")]);
    }

    #[test]
    fn list_archives_sorts_double_digit_major_numerically() {
        // Regression: a plain string sort put "10.00" before "5.31"
        // ('1' < '5' lexically); once any body reaches a double-digit
        // firmware major this must sort as the newer version.
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "10.00");
        write_meta(dir.path(), "Z 9", "5.31");
        write_meta(dir.path(), "Z 9", "9.00");

        let entries = list_archives(dir.path(), None);
        let versions: Vec<&str> = entries.iter().map(|e| e.version.as_str()).collect();
        assert_eq!(versions, vec!["5.31", "9.00", "10.00"]);
    }

    #[test]
    fn version_sort_key_unparseable_sorts_after_numeric() {
        // firmware_version is free-typed, not schema-validated — an
        // unparseable value must sort deterministically after every
        // numeric version, not panic or silently misorder.
        let mut keys = vec![
            version_sort_key("5.31"),
            version_sort_key("common"),
            version_sort_key("10.00"),
        ];
        keys.sort();
        assert_eq!(keys, vec![
            version_sort_key("5.31"),
            version_sort_key("10.00"),
            version_sort_key("common"),
        ]);
    }

    // ── scan_archives / the --json listing ───────────────────────────────

    fn kinds(scan: &ArchiveScan) -> Vec<(String, ArchiveSkipKind)> {
        scan.skipped
            .iter()
            .map(|s| (s.directory.file_name().unwrap().to_string_lossy().into_owned(), s.kind))
            .collect()
    }

    #[test]
    fn scan_reports_every_kind_of_unlistable_entry_with_a_closed_kind() {
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "5.31");
        let bad = |v: &str| {
            let d = archive_dir(dir.path(), "Z 9", v);
            fs::create_dir_all(&d).unwrap();
            d
        };
        fs::write(bad("1.corrupt").join("metadata.json"), b"not json").unwrap();
        let future = bad("2.future");
        fs::write(
            future.join("metadata.json"),
            serde_json::to_string(&serde_json::json!({
                "format_version": 99, "model": "Z 9", "firmware_version": "2.future", "archived_at": "t",
                "archived_by": "x", "bin_sha256": "x", "bin_size_bytes": 1, "source_path": "p",
                "canonical_snapshot_path": null, "notes": null
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(bad("3.orphan").join("firmware.bin"), b"orphan").unwrap();
        fs::write(bad("4.part").join("firmware.bin.part"), b"half").unwrap();
        // A metadata.json that is a directory exists but cannot be read.
        fs::create_dir_all(bad("5.unreadable").join("metadata.json")).unwrap();
        // Boundary: an empty version directory is not an entry and not an error.
        bad("6.empty");

        let scan = scan_archives(dir.path(), None);
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(
            kinds(&scan),
            [
                ("1.corrupt".to_string(), ArchiveSkipKind::MetadataCorrupt),
                ("2.future".to_string(), ArchiveSkipKind::UnsupportedVersion),
                ("3.orphan".to_string(), ArchiveSkipKind::IncompleteArchive),
                ("4.part".to_string(), ArchiveSkipKind::IncompleteArchive),
                ("5.unreadable".to_string(), ArchiveSkipKind::MetadataUnreadable),
            ]
        );
        assert!(scan.skipped.iter().all(|s| !s.error.is_empty()));
    }

    #[test]
    fn scan_model_filter_keeps_only_that_models_skipped_entries() {
        let dir = TempDir::new().unwrap();
        for model in ["Z 9", "Z6_3"] {
            let d = archive_dir(dir.path(), model, "1.00");
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("metadata.json"), b"{").unwrap();
        }
        assert_eq!(scan_archives(dir.path(), None).skipped.len(), 2);
        let z9 = scan_archives(dir.path(), Some("Z 9"));
        assert_eq!(z9.skipped.len(), 1);
        assert!(z9.skipped[0].directory.to_string_lossy().contains("Z_9"));
        assert!(scan_archives(dir.path(), Some("Z 5")).skipped.is_empty());
    }

    #[test]
    fn list_archives_is_exactly_the_scans_entries() {
        // One validation rule: the text listing and --json cannot disagree about what is valid.
        let dir = TempDir::new().unwrap();
        write_meta(dir.path(), "Z 9", "5.31");
        write_meta(dir.path(), "Z6_3", "2.00");
        let bad = archive_dir(dir.path(), "Z 9", "4.00");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("metadata.json"), b"{").unwrap();
        let scan = scan_archives(dir.path(), None);
        let listed = list_archives(dir.path(), None);
        assert_eq!(listed, scan.entries);
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn a_missing_firmware_root_is_an_empty_scan_not_an_error() {
        let dir = TempDir::new().unwrap();
        let scan = scan_archives(dir.path(), None);
        assert!(!scan.root_exists && scan.entries.is_empty() && scan.skipped.is_empty());
        fs::create_dir_all(firmware_root(dir.path())).unwrap();
        let scan = scan_archives(dir.path(), None);
        assert!(scan.root_exists && scan.entries.is_empty());
    }

    #[test]
    fn entries_say_whether_their_binary_is_present() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let f = file_with(src.path(), "in.bin", b"hello");
        ingest_bundle(data.path(), "Z 9", "5.33", &f, &prov(), None).unwrap();
        write_meta(data.path(), "Z 9", "5.40"); // metadata only: its binary was never there
        let by_version: Vec<(String, bool)> = scan_archives(data.path(), None)
            .entries
            .into_iter()
            .map(|e| (e.version, e.bin_present))
            .collect();
        assert_eq!(by_version, [("5.33".to_string(), true), ("5.40".to_string(), false)]);
        // A directory with that name is not a binary.
        let dir = archive_dir(data.path(), "Z 9", "5.40");
        fs::create_dir_all(dir.join("firmware.bin")).unwrap();
        assert!(!scan_archives(data.path(), None).entries[1].bin_present);
    }

    #[test]
    fn listing_json_has_the_ls_envelope_and_one_row_per_entry() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let f = file_with(src.path(), "in.bin", b"hello");
        ingest_bundle(data.path(), "Z 9", "5.33", &f, &prov(), None).unwrap();
        let bad = archive_dir(data.path(), "Z 9", "5.10");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("metadata.json"), b"{").unwrap();

        let v = serde_json::to_value(ArchiveListing::from_scan(&scan_archives(data.path(), None))).unwrap();
        assert_eq!(v["dir_exists"], serde_json::json!(true));
        assert_eq!(v["skipped_dir_entries"], serde_json::json!(0));
        assert!(v["dir"].as_str().unwrap().ends_with("firmware"));
        let (ok, unreadable) = (&v["entries"][0], &v["entries"][1]);
        assert_eq!(v["entries"].as_array().unwrap().len(), 2);
        assert_eq!(ok["status"], "ok");
        assert_eq!((ok["model"].as_str(), ok["version"].as_str()), (Some("Z 9"), Some("5.33")));
        assert_eq!(ok["bin_sha256"], "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(ok["bin_size_bytes"], 5);
        assert_eq!(ok["bin_present"], true);
        assert!(ok["directory"].as_str().unwrap().ends_with("5.33"));
        assert_eq!(ok["canonical_snapshot_path"], serde_json::Value::Null);
        assert_eq!(ok["provenance"]["source_id"], "test");
        assert_eq!(unreadable["status"], "unreadable");
        assert_eq!(unreadable["error_kind"], "metadata_corrupt");
        assert!(unreadable.get("model").is_none());
        // A missing root is `dir_exists: false`, not an error.
        let v = serde_json::to_value(ArchiveListing::from_scan(&scan_archives(src.path(), None))).unwrap();
        assert_eq!((v["dir_exists"].as_bool(), v["entries"].as_array().map(Vec::len)), (Some(false), Some(0)));
    }

    #[test]
    fn ingest_names_agree_with_the_shared_plain_component_table() {
        // `ingest_bundle` is this module's caller of the predicate; the shared table is what
        // ops::list and ops::paths run too.
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let f = file_with(src.path(), "in.bin", b"hello");
        for (value, plain) in crate::ops::paths::PLAIN_COMPONENT_TABLE {
            let r = ingest_bundle(data.path(), "Z 9", value, &f, &prov(), None);
            let refused = matches!(r, Err(FirmwareError::UnsafePathComponent { .. }));
            assert_eq!(!refused, *plain, "version {value:?}: {r:?}");
        }
        // NUL in particular: the copy this module used to carry did not refuse it.
        assert!(matches!(
            ingest_bundle(data.path(), "Z 9", "5\0.31", &f, &prov(), None),
            Err(FirmwareError::UnsafePathComponent { .. })
        ));
    }

    // ── capability_map_for_fw boundary ───────────────────────────────────

    #[test]
    fn capability_map_fw_boundary() {
        use crate::maid_layer::MaidLayerConfig;
        let schema_text = r#"
<model:Z 9>
  <version>4.0</version>
  <caplist>
    <capability:kNkMAIDCapability_OldMode-100>
      <description>0,0,"OldMode"</description>
      <allowedoperation:14></allowedoperation>
    </capability>
  </caplist>
</model>
<model:Z 9>
  <version>5.0</version>
  <caplist>
    <capability:kNkMAIDCapability_NewMode-200>
      <description>0,0,"NewMode"</description>
      <allowedoperation:14></allowedoperation>
    </capability>
  </caplist>
</model>
"#;
        let schema = MaidLayerConfig::parse(schema_text).unwrap();

        // At exactly fw_major=4: should include section 4.0 but NOT 5.0
        let caps4 = capability_map_for_fw(&schema, "Z 9", 4);
        assert!(caps4.contains_key("kNkMAIDCapability_OldMode"),  "4.0 section must be included at fw_major=4");
        assert!(!caps4.contains_key("kNkMAIDCapability_NewMode"), "5.0 section must not be included at fw_major=4");

        // At fw_major=5: both sections included
        let caps5 = capability_map_for_fw(&schema, "Z 9", 5);
        assert!(caps5.contains_key("kNkMAIDCapability_OldMode"),  "4.0 section must be included at fw_major=5");
        assert!(caps5.contains_key("kNkMAIDCapability_NewMode"),  "5.0 section must be included at fw_major=5");

        // At fw_major=3: neither section included (both above the boundary)
        let caps3 = capability_map_for_fw(&schema, "Z 9", 3);
        assert!(caps3.is_empty(), "no sections should be included at fw_major=3");
    }

    // ── ingest_bundle ────────────────────────────────────────────────────────

    fn prov() -> Provenance {
        Provenance {
            source_id: "test".into(),
            source_url: "https://example.test/page".into(),
            page_url: "https://example.test/page".into(),
            released: Some("2026-10-20".into()),
            retrieved_at: "2026-10-27T00:00:00Z".into(),
            method: RetrievalMethod::ManualAfterGate,
            published_size: None,
            published_sha256: None,
            original_filename: None,
        }
    }

    fn file_with(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn ingest_archives_with_provenance_and_page() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let f = file_with(src.path(), "in.bin", b"hello");
        let out = ingest_bundle(data.path(), "Z 9", "5.33", &f, &prov(), Some("<html>page</html>")).unwrap();
        let IngestOutcome::Archived(meta) = out else { panic!("expected Archived") };
        assert_eq!(meta.bin_sha256, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(meta.bin_size_bytes, 5);
        let dir = archive_dir(data.path(), "Z 9", "5.33");
        assert_eq!(fs::read(dir.join("firmware.bin")).unwrap(), b"hello");
        assert_eq!(fs::read_to_string(dir.join("source-page.html")).unwrap(), "<html>page</html>");
        assert!(!dir.join("firmware.bin.part").exists(), "temp file must not linger");
        // round-trips, and is visible to the existing listing code
        assert_eq!(load_meta(data.path(), "Z 9", "5.33").unwrap().provenance, Some(prov()));
        assert_eq!(list_archives(data.path(), None).len(), 1);
    }

    #[test]
    fn ingest_same_bytes_is_noop_different_bytes_never_overwrites() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let a = file_with(src.path(), "a.bin", b"hello");
        let b = file_with(src.path(), "b.bin", b"hellO"); // same length, one byte differs
        ingest_bundle(data.path(), "Z 9", "5.33", &a, &prov(), None).unwrap();
        assert!(matches!(
            ingest_bundle(data.path(), "Z 9", "5.33", &a, &prov(), None).unwrap(),
            IngestOutcome::AlreadyPresent(_)
        ));
        let err = ingest_bundle(data.path(), "Z 9", "5.33", &b, &prov(), None).unwrap_err();
        assert!(matches!(err, FirmwareError::ContentMismatch { .. }), "{err:?}");
        let bin = archive_dir(data.path(), "Z 9", "5.33").join("firmware.bin");
        assert_eq!(fs::read(bin).unwrap(), b"hello", "original bytes must survive the refused overwrite");
    }

    #[test]
    fn ingest_detects_corrupted_existing_archive() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let a = file_with(src.path(), "a.bin", b"hello");
        ingest_bundle(data.path(), "Z 9", "5.33", &a, &prov(), None).unwrap();
        fs::write(archive_dir(data.path(), "Z 9", "5.33").join("firmware.bin"), b"rotted").unwrap();
        let err = ingest_bundle(data.path(), "Z 9", "5.33", &a, &prov(), None).unwrap_err();
        assert!(matches!(err, FirmwareError::ChecksumMismatch { .. }), "{err:?}");
    }

    #[test]
    fn ingest_refuses_bin_without_metadata_and_empty_input_and_unsafe_names() {
        let (data, src) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let a = file_with(src.path(), "a.bin", b"hello");
        let dir = archive_dir(data.path(), "Z 9", "5.33");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("firmware.bin"), b"orphan").unwrap();
        assert!(matches!(
            ingest_bundle(data.path(), "Z 9", "5.33", &a, &prov(), None).unwrap_err(),
            FirmwareError::IncompleteArchive { .. }
        ));
        assert_eq!(fs::read(dir.join("firmware.bin")).unwrap(), b"orphan");

        let empty = file_with(src.path(), "empty.bin", b"");
        assert!(matches!(
            ingest_bundle(data.path(), "Z 9", "5.34", &empty, &prov(), None).unwrap_err(),
            FirmwareError::Io(_)
        ));
        assert!(!archive_exists(data.path(), "Z 9", "5.34"));

        for bad in ["", ".", "..", "../x", "a/b", "a\\b"] {
            assert!(matches!(
                ingest_bundle(data.path(), "Z 9", bad, &a, &prov(), None).unwrap_err(),
                FirmwareError::UnsafePathComponent { .. }
            ), "version {bad:?}");
        }
        assert!(ingest_bundle(data.path(), "../evil", "5.33", &a, &prov(), None).is_err());
    }

    #[test]
    fn ingest_missing_source_file_is_error() {
        let data = TempDir::new().unwrap();
        let err = ingest_bundle(data.path(), "Z 9", "5.33", Path::new("/nonexistent/x.bin"), &prov(), None).unwrap_err();
        assert!(matches!(err, FirmwareError::Io(_)));
    }

    #[test]
    fn legacy_metadata_without_provenance_still_loads() {
        // Entries written by `fleet firmware add` before provenance existed.
        let data = TempDir::new().unwrap();
        let adir = archive_dir(data.path(), "Z 9", "5.31");
        fs::create_dir_all(&adir).unwrap();
        fs::write(adir.join("metadata.json"), r#"{"format_version":1,"model":"Z 9","firmware_version":"5.31",
            "archived_at":"t","archived_by":"fleet firmware add","bin_sha256":"x","bin_size_bytes":1,
            "source_path":"p","canonical_snapshot_path":null,"notes":null}"#).unwrap();
        assert_eq!(load_meta(data.path(), "Z 9", "5.31").unwrap().provenance, None);
    }
}
