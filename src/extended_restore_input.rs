//! The input `restore-extended` assembles from a loaded snapshot, shared by its two front ends: the CLI's
//! `cmd_restore_extended` and the MCP worker's `op_restore_extended`. Both used to build it independently
//! (target identity, `--only` parsing, which serial opens the session), so a rule changed in one would
//! have silently diverged from the other. They differ only in how they LOAD the snapshot (the worker
//! confines the name to `snapshots/`) and in how they report a refusal; everything between is here.

use std::collections::BTreeSet;
use std::path::Path;

use nikon_fleet::ops::restore_extended::{Category, ExtendedRestoreInput, Refusal};
use nikon_fleet::snapshot::{Camera, Snapshot};

/// Why the input could not be assembled. Both are refusals made before any camera is touched.
#[derive(Debug)]
pub enum PrepareError {
    /// `--only` named something that is not a category.
    UnknownCategory(String),
    /// The snapshot holds no extended section, so there is nothing to restore. The library's own refusal (its tag and
    /// words), raised here so it comes before the camera lock and the raw session are taken instead of after them.
    NoExtendedSection,
}

impl PrepareError {
    pub const UNKNOWN_CATEGORY_TAG: &'static str = "unknown_category";

    pub fn tag(&self) -> &'static str {
        match self {
            PrepareError::UnknownCategory(_) => Self::UNKNOWN_CATEGORY_TAG,
            PrepareError::NoExtendedSection => Refusal::SnapshotHasNoExtended.tag(),
        }
    }
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::UnknownCategory(why) => f.write_str(why),
            PrepareError::NoExtendedSection => std::fmt::Display::fmt(&Refusal::SnapshotHasNoExtended, f),
        }
    }
}

impl std::error::Error for PrepareError {}

/// Everything `ExtendedRestoreInput` borrows, owned, plus the serial the raw session is opened for.
#[derive(Debug)]
pub struct PreparedExtendedRestore {
    pub snapshot: Snapshot,
    pub bytes: Vec<u8>,
    /// The snapshot file's own name (the plan's provenance), not the path.
    pub name: String,
    /// The snapshot's camera identity, with the serial replaced by the requested one when there is one. A
    /// serial that is not the live body's is refused against it by the library (`not_this_body`).
    pub target: Camera,
    /// `None` = every category.
    pub only: Option<BTreeSet<Category>>,
    /// The serial to open the session for: the requested one, else the snapshot's; `None` when neither names one.
    pub session_serial: Option<String>,
}

impl PreparedExtendedRestore {
    pub fn input(&self) -> ExtendedRestoreInput<'_> {
        ExtendedRestoreInput {
            source: &self.snapshot,
            source_bytes: &self.bytes,
            source_name: &self.name,
            target: &self.target,
            only: self.only.as_ref(),
        }
    }
}

/// Assemble the input from an already-loaded snapshot. `serial` is the one the caller was asked to act on
/// (`--serial`; the worker always has one). Nothing here touches a camera.
pub fn prepare(
    snapshot: Snapshot,
    bytes: Vec<u8>,
    source_path: &Path,
    serial: Option<&str>,
    only_names: &[String],
) -> Result<PreparedExtendedRestore, PrepareError> {
    let only = if only_names.is_empty() {
        None
    } else {
        Some(
            only_names
                .iter()
                .map(|c| Category::parse(c))
                .collect::<Result<BTreeSet<_>, _>>()
                .map_err(PrepareError::UnknownCategory)?,
        )
    };
    if snapshot.extended.is_none() {
        return Err(PrepareError::NoExtendedSection);
    }
    let name = source_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut target = snapshot.camera.clone();
    if let Some(s) = serial {
        target.serial = s.to_string();
    }
    let session_serial = Some(target.serial.clone()).filter(|s| !s.is_empty());
    Ok(PreparedExtendedRestore { snapshot, bytes, name, target, only, session_serial })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nikon_fleet::snapshot::Transport;
    use std::path::PathBuf;

    fn bare(serial: &str) -> Snapshot {
        Snapshot::new(
            Camera { model: "Z 9".into(), serial: serial.into(), firmware: "5.31".into() },
            Transport::Usb,
            "2026-11-06T00:00:00Z".into(),
        )
    }

    fn snap(serial: &str) -> Snapshot {
        use nikon_fleet::extended::{Extended, FtpState, ReadFrom};
        let mut s = bare(serial);
        s.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: serial.into() },
            props: Default::default(),
            iptc: Default::default(),
            ftp: FtpState::NotOnModel,
        });
        s
    }

    fn prep(serial_in_snapshot: &str, requested: Option<&str>, only: &[&str]) -> Result<PreparedExtendedRestore, PrepareError> {
        let only: Vec<String> = only.iter().map(|s| s.to_string()).collect();
        prepare(snap(serial_in_snapshot), b"{}".to_vec(), &PathBuf::from("/d/snapshots/a.json"), requested, &only)
    }

    #[test]
    fn a_snapshot_without_an_extended_section_is_refused_with_the_librarys_tag_and_words_before_any_camera() {
        let e = prepare(bare("S"), b"{}".to_vec(), &PathBuf::from("a.json"), Some("S"), &[]).unwrap_err();
        assert!(matches!(e, PrepareError::NoExtendedSection));
        assert_eq!(e.tag(), "snapshot_has_no_extended");
        assert_eq!(e.to_string(), Refusal::SnapshotHasNoExtended.to_string(), "one text, the library's");
        // A bad category is reported first: it is the cheaper thing to correct.
        let e = prepare(bare("S"), b"{}".to_vec(), &PathBuf::from("a.json"), None, &["nope".to_string()]).unwrap_err();
        assert_eq!(e.tag(), "unknown_category");
    }

    #[test]
    fn the_snapshot_serial_is_the_default_target_and_a_requested_serial_replaces_it() {
        let p = prep("S1", None, &[]).unwrap();
        assert_eq!((p.target.serial.as_str(), p.session_serial.as_deref()), ("S1", Some("S1")));
        assert_eq!(p.target.model, "Z 9", "the model always stays the snapshot's");
        let p = prep("S1", Some("S2"), &[]).unwrap();
        assert_eq!((p.target.serial.as_str(), p.session_serial.as_deref()), ("S2", Some("S2")));
    }

    #[test]
    fn no_serial_anywhere_opens_the_session_for_whichever_body_is_alone() {
        assert_eq!(prep("", None, &[]).unwrap().session_serial, None);
        // An empty requested serial is a request for nothing: it is not "a different body".
        let p = prep("S1", Some(""), &[]).unwrap();
        assert_eq!(p.session_serial, None, "an empty --serial names no body");
    }

    #[test]
    fn only_is_all_categories_when_absent_and_a_set_when_given() {
        assert_eq!(prep("S", None, &[]).unwrap().only, None);
        let p = prep("S", None, &["ftp", "Strings ", "ftp"]).unwrap();
        assert_eq!(p.only, Some([Category::Ftp, Category::Strings].into_iter().collect()));
    }

    #[test]
    fn one_unknown_category_refuses_the_whole_selection_and_names_it() {
        for bad in ["nope", "", "all", "ftp,iptc", "string"] {
            let e = prep("S", None, &["strings", bad]).unwrap_err();
            assert!(e.to_string().contains("is not a category"), "{bad:?}: {e}");
        }
        assert_eq!(PrepareError::UnknownCategory(String::new()).tag(), "unknown_category");
    }

    #[test]
    fn the_name_is_the_file_name_and_the_input_borrows_the_prepared_values() {
        let p = prep("S1", Some("S2"), &["iptc"]).unwrap();
        assert_eq!(p.name, "a.json");
        let i = p.input();
        assert_eq!((i.source_name, i.target.serial.as_str(), i.source_bytes), ("a.json", "S2", &b"{}"[..]));
        assert_eq!(i.only.map(|s| s.len()), Some(1));
    }
}
