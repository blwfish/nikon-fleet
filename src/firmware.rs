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

// ─── Listing ─────────────────────────────────────────────────────────────────

pub struct ArchiveEntry {
    pub model: String,
    pub version: String,
    pub meta: FirmwareMeta,
}

/// Walk `firmware/` and return all valid entries sorted by model then version.
/// Reads `model` and `firmware_version` from each `metadata.json` (not the
/// directory names) so the display name uses canonical spacing.
pub fn list_archives(data_dir: &Path, model_filter: Option<&str>) -> Vec<ArchiveEntry> {
    let root = firmware_root(data_dir);
    if !root.exists() {
        return Vec::new();
    }
    let mut entries: Vec<ArchiveEntry> = Vec::new();

    let Ok(model_dirs) = fs::read_dir(&root) else { return Vec::new(); };

    for model_dir in model_dirs.filter_map(|e| e.ok()) {
        if !model_dir.path().is_dir() { continue; }
        let Ok(version_dirs) = fs::read_dir(model_dir.path()) else { continue; };

        for version_dir in version_dirs.filter_map(|e| e.ok()) {
            if !version_dir.path().is_dir() { continue; }
            let meta_path = version_dir.path().join("metadata.json");
            if !meta_path.exists() { continue; }
            let Ok(text) = fs::read_to_string(&meta_path) else { continue; };
            let Ok(meta) = serde_json::from_str::<FirmwareMeta>(&text) else { continue; };
            if let Some(f) = model_filter {
                if meta.model != f { continue; }
            }
            entries.push(ArchiveEntry {
                model: meta.model.clone(),
                version: meta.firmware_version.clone(),
                meta,
            });
        }
    }

    entries.sort_by(|a, b| a.model.cmp(&b.model).then(a.version.cmp(&b.version)));
    entries
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
}
