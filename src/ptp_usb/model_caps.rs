//! The one table of what is known, per camera model, about the raw-PTP features this crate writes: FTP profiles
//! (adds, replaces and single deletes), IPTC presets, the three owner strings and the two one-byte switches.
//!
//! Every per-model gate reads this table and none keeps a list of its own: the session's write gates
//! ([`ftp_write_confirmed`], [`ftp_delete_confirmed`], [`iptc_write_confirmed`], [`string_write_gate`],
//! [`byte_write_confirmed`]), the restore planner (which calls those same functions), and the extended capture's
//! "this body lacks it" skips ([`ftp_absent`], [`iptc_absent`]). The model registry's name list
//! (`ops::cameras::KNOWN_MODELS`) is derived from it too, so a model has exactly one spelling, the SDK's (`"Z 9"`,
//! `"Z6_3"`, `"Z 5"`; the Z6III's USB product string is `NIKON DSC Z6_3`, not "Z6III").
//!
//! A model that is not in the table is NOT an error to be guessed around: every write gate refuses it before
//! anything is sent ([`PtpUsbError::UnconfirmedModel`], whose text says the model is not recognized), and the capture
//! asks it for everything, so an unknown body loses no data. The research-only `force_unconfirmed_model` flags
//! bypass the write gates and nothing else.
//!
//! Nothing in the table is a claim about firmware: the confirmations were made on one firmware per body and which
//! one was not recorded for every write, so the unit is "this model".

use super::{PROP_ARTIST, PROP_COPYRIGHT, PROP_ENABLE_COPYRIGHT, PROP_IMAGE_COMMENT, PROP_IPTC_PRESET, PtpUsbError};

/// What is known about a body's support for one feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// The body is known NOT to have the feature (owner, 2026-11-06, for the Z 5's FTP profiles and IPTC presets).
    /// The extended capture does not ask it; no write is ever confirmed on it.
    Absent,
    /// The body is surveyed but a write of this feature has never been confirmed on it, so a write is refused
    /// (unless `force_unconfirmed_model`). Reads still ask it: it may well have the feature.
    Unconfirmed,
    /// The feature's writes were run on a body of this model and read back.
    Confirmed,
}

/// One model's row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCaps {
    /// The SDK model name, which is also the USB product string minus `"NIKON DSC "`.
    pub model: &'static str,
    /// FTP profiles: `Confirmed` means `0x90EE` adds and replaces were run and read back on this model.
    pub ftp: Support,
    /// `0x90EF(n)`, deleting ONE stored profile, was run on this model. Only meaningful with `ftp == Confirmed`
    /// (a test pins that). There is no delete-all in the product path.
    pub ftp_delete: bool,
    /// IPTC presets: `Confirmed` means a `0x9413` slot write was read back on this model.
    pub iptc: Support,
    /// The string properties (Artist, Copyright, Image comment) whose USB write was run and restored byte for byte
    /// on this model.
    pub string_writes: &'static [u16],
    /// The one-byte properties (EnableCopyright, IPTC preset select) whose USB write was run on this model.
    pub byte_writes: &'static [u16],
}

const ALL_STRINGS: &[u16] = &[PROP_ARTIST, PROP_COPYRIGHT, PROP_IMAGE_COMMENT];

/// The table. Evidence per cell (all 2026-11-05/06 unless noted):
/// - Z 5: no FTP profiles and no IPTC presets (owner; its `0x90E8` times out, its DeviceInfo lists no `0x9413`).
///   Strings: Artist, Copyright and Image comment written and restored. `0xD053` EnableCopyright written 0 then 1 and
///   read back.
/// - Z6III (`"Z6_3"`): FTP adds, replaces and `0x90EF(n)` deletes read back; IPTC write confirmed (replay 2026-09-06, a
///   new preset into an empty slot read back identical); strings written and restored; `0xD060` set to 1 OK.
/// - Z 9: FTP adds, replaces and `0x90EF(n)` deletes read back (the single delete also 2026-07-10); IPTC write confirmed;
///   strings written and restored. `0xD060` answered `AccessDenied` while all its presets were empty, and `0xD053` was
///   never run on it, so no byte property is confirmed.
///
/// Registry order (`KNOWN_MODELS` is this order; `FTP_WRITE_CONFIRMED_MODELS` keeps the order the Python parity fixture lists).
pub const MODEL_CAPS: [ModelCaps; 3] = [
    ModelCaps {
        model: "Z 5",
        ftp: Support::Absent,
        ftp_delete: false,
        iptc: Support::Absent,
        string_writes: ALL_STRINGS,
        byte_writes: &[PROP_ENABLE_COPYRIGHT],
    },
    ModelCaps {
        model: "Z 9",
        ftp: Support::Confirmed,
        ftp_delete: true,
        iptc: Support::Confirmed,
        string_writes: ALL_STRINGS,
        byte_writes: &[],
    },
    ModelCaps {
        model: "Z6_3",
        ftp: Support::Confirmed,
        ftp_delete: true,
        iptc: Support::Confirmed,
        string_writes: ALL_STRINGS,
        byte_writes: &[PROP_IPTC_PRESET],
    },
];

/// The model names of `table`, in order. `const` so the registry's list is built from the table at compile time.
pub const fn model_names<const N: usize>(table: &[ModelCaps; N]) -> [&'static str; N] {
    let mut names = [""; N];
    let mut i = 0;
    while i < N {
        names[i] = table[i].model;
        i += 1;
    }
    names
}

const fn count_ftp_confirmed(table: &[ModelCaps]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < table.len() {
        if matches!(table[i].ftp, Support::Confirmed) {
            n += 1;
        }
        i += 1;
    }
    n
}

const FTP_CONFIRMED_COUNT: usize = count_ftp_confirmed(&MODEL_CAPS);

/// The models with `ftp == Confirmed`, in table order: a VIEW of the table kept because the Python parity test
/// (`tests/ftp_profile_python_parity.rs`) holds its own list to it. Nothing in the crate gates on this array; the gates
/// call [`ftp_write_confirmed`].
pub const FTP_WRITE_CONFIRMED_MODELS: [&str; FTP_CONFIRMED_COUNT] = {
    let mut out = [""; FTP_CONFIRMED_COUNT];
    let mut i = 0;
    let mut j = 0;
    while i < MODEL_CAPS.len() {
        if matches!(MODEL_CAPS[i].ftp, Support::Confirmed) {
            out[j] = MODEL_CAPS[i].model;
            j += 1;
        }
        i += 1;
    }
    out
};

/// The row for `model` in `table`: an exact match (no case folding, no trimming), `None` for a model not in it.
/// Every public predicate is this lookup over [`MODEL_CAPS`]; the `_in` forms exist so a test can run the same
/// mechanism over a table whose rows differ.
pub fn caps_in<'t>(table: &'t [ModelCaps], model: &str) -> Option<&'t ModelCaps> {
    table.iter().find(|c| c.model == model)
}

/// The row for `model`, or `None` for a model this code does not recognize.
pub fn caps_of(model: &str) -> Option<&'static ModelCaps> {
    caps_in(&MODEL_CAPS, model)
}

impl ModelCaps {
    /// The body is known to have no FTP profiles.
    pub fn lacks_ftp(&self) -> bool {
        self.ftp == Support::Absent
    }

    /// The body is known to have no IPTC presets.
    pub fn lacks_iptc(&self) -> bool {
        self.iptc == Support::Absent
    }

    /// `0x90EE` adds and replaces are confirmed.
    pub fn ftp_write_ok(&self) -> bool {
        self.ftp == Support::Confirmed
    }

    /// `0x90EF(n)` is confirmed AND FTP writes are (a delete is a write).
    pub fn ftp_delete_ok(&self) -> bool {
        self.ftp_delete && self.ftp == Support::Confirmed
    }

    /// A `0x9413` slot write is confirmed.
    pub fn iptc_write_ok(&self) -> bool {
        self.iptc == Support::Confirmed
    }

    /// The byte property `propcode` was written on this model.
    pub fn byte_write_ok(&self, propcode: u16) -> bool {
        self.byte_writes.contains(&propcode)
    }
}

/// The body is known to have no FTP profiles. An unrecognized model is NOT absent (it is asked).
pub fn ftp_absent(model: &str) -> bool {
    caps_of(model).is_some_and(ModelCaps::lacks_ftp)
}

/// The body is known to have no IPTC presets. An unrecognized model is NOT absent (it is asked).
pub fn iptc_absent(model: &str) -> bool {
    caps_of(model).is_some_and(ModelCaps::lacks_iptc)
}

/// FTP profile writes (`0x90EE` adds and replaces) are confirmed on `model`.
pub fn ftp_write_confirmed(model: &str) -> bool {
    caps_of(model).is_some_and(ModelCaps::ftp_write_ok)
}

/// Deleting one stored FTP profile (`0x90EF(n)`) is confirmed on `model`. The gate of
/// `PtpUsbSession::delete_ftp_profile`; a restore's FTP rebuild is planned only where this and
/// [`ftp_write_confirmed`] both hold.
pub fn ftp_delete_confirmed(model: &str) -> bool {
    caps_of(model).is_some_and(ModelCaps::ftp_delete_ok)
}

/// An IPTC slot write (`0x9413`) is confirmed on `model`.
pub fn iptc_write_confirmed(model: &str) -> bool {
    caps_of(model).is_some_and(ModelCaps::iptc_write_ok)
}

/// Whether the byte property `propcode` has been written on `model`.
pub fn byte_write_confirmed(model: &str, propcode: u16) -> bool {
    caps_of(model).is_some_and(|c| c.byte_write_ok(propcode))
}

/// The models on which the string property `propcode` is confirmed, for refusal text.
pub fn string_write_models(propcode: u16) -> Vec<&'static str> {
    MODEL_CAPS.iter().filter(|c| c.string_writes.contains(&propcode)).map(|c| c.model).collect()
}

/// The write gate of a string property over `table`: a model with no confirmed string property (or none in the table) is
/// [`PtpUsbError::UnconfirmedModel`] (checked first, so the error names the real cause); a recognized model that has
/// some but not this one is [`PtpUsbError::PropWriteUnconfirmed`].
pub fn string_write_gate_in(table: &[ModelCaps], model: &str, propcode: u16) -> Result<(), PtpUsbError> {
    let Some(caps) = caps_in(table, model).filter(|c| !c.string_writes.is_empty()) else {
        return Err(PtpUsbError::UnconfirmedModel(model.to_string()));
    };
    if !caps.string_writes.contains(&propcode) {
        return Err(PtpUsbError::PropWriteUnconfirmed { propcode, model: model.to_string() });
    }
    Ok(())
}

/// [`string_write_gate_in`] over [`MODEL_CAPS`].
pub fn string_write_gate(model: &str, propcode: u16) -> Result<(), PtpUsbError> {
    string_write_gate_in(&MODEL_CAPS, model, propcode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::cameras::{KNOWN_MODELS, sdk_model_name};
    use crate::ptp_usb::{BYTE_PROPS, STRING_PROPS};

    #[test]
    fn the_table_is_pinned_cell_by_cell() {
        // Each cell is a claim about what may be sent to a camera. Changing one is a decision, so it is spelled out.
        let row = |m: &str| *caps_of(m).unwrap_or_else(|| panic!("{m}"));
        let all: &[u16] = &[0xD072, 0xD073, 0xD090];
        assert_eq!(
            row("Z 5"),
            ModelCaps { model: "Z 5", ftp: Support::Absent, ftp_delete: false, iptc: Support::Absent, string_writes: all, byte_writes: &[0xD053] }
        );
        assert_eq!(
            row("Z6_3"),
            ModelCaps { model: "Z6_3", ftp: Support::Confirmed, ftp_delete: true, iptc: Support::Confirmed, string_writes: all, byte_writes: &[0xD060] }
        );
        assert_eq!(
            row("Z 9"),
            ModelCaps { model: "Z 9", ftp: Support::Confirmed, ftp_delete: true, iptc: Support::Confirmed, string_writes: all, byte_writes: &[] }
        );
        assert_eq!(MODEL_CAPS.len(), 3, "a new row needs its own pinned cells above");
        assert_eq!(FTP_WRITE_CONFIRMED_MODELS, ["Z 9", "Z6_3"], "the order tests/fixtures/ftp_profile_vectors.json lists");
    }

    #[test]
    fn the_registry_is_the_table_and_the_names_are_unique() {
        assert_eq!(KNOWN_MODELS, model_names(&MODEL_CAPS), "ops::cameras::KNOWN_MODELS is built from the table");
        for (i, c) in MODEL_CAPS.iter().enumerate() {
            assert!(sdk_model_name(c.model).is_some(), "{:?}", c.model);
            assert!(MODEL_CAPS[i + 1..].iter().all(|o| o.model != c.model), "{:?} appears twice", c.model);
        }
    }

    #[test]
    fn every_row_is_internally_consistent() {
        for c in &MODEL_CAPS {
            // A single delete is a write: it cannot be confirmed where adds are not.
            assert!(!c.ftp_delete || c.ftp == Support::Confirmed, "{}: delete without confirmed FTP", c.model);
            // A body that lacks the feature has no confirmed write of it.
            assert!(c.ftp != Support::Absent || !c.ftp_delete, "{}", c.model);
            // Only properties this crate writes can be listed, and each at most once.
            for code in c.string_writes {
                assert!(STRING_PROPS.iter().any(|(p, _)| p == code), "{}: {code:#x} is not a string property", c.model);
            }
            for code in c.byte_writes {
                assert!(BYTE_PROPS.iter().any(|(p, ..)| p == code), "{}: {code:#x} is not a byte property", c.model);
            }
            assert!((0..c.string_writes.len()).all(|i| !c.string_writes[i + 1..].contains(&c.string_writes[i])), "{}", c.model);
            assert!((0..c.byte_writes.len()).all(|i| !c.byte_writes[i + 1..].contains(&c.byte_writes[i])), "{}", c.model);
        }
    }

    #[test]
    fn every_predicate_reads_the_row_and_an_unknown_model_is_false_for_all() {
        for c in &MODEL_CAPS {
            assert_eq!(ftp_absent(c.model), c.ftp == Support::Absent, "{}", c.model);
            assert_eq!(iptc_absent(c.model), c.iptc == Support::Absent, "{}", c.model);
            assert_eq!(ftp_write_confirmed(c.model), c.ftp == Support::Confirmed, "{}", c.model);
            assert_eq!(ftp_delete_confirmed(c.model), c.ftp_delete, "{}", c.model);
            assert_eq!(iptc_write_confirmed(c.model), c.iptc == Support::Confirmed, "{}", c.model);
            for (code, _) in STRING_PROPS {
                assert_eq!(string_write_gate(c.model, code).is_ok(), c.string_writes.contains(&code), "{} {code:#x}", c.model);
            }
            for (code, ..) in BYTE_PROPS {
                assert_eq!(byte_write_confirmed(c.model, code), c.byte_writes.contains(&code), "{} {code:#x}", c.model);
            }
        }
        // Not recognized: nothing is absent (so the capture asks it and loses no data) and nothing is confirmed.
        // Near-misses of real names are not those names.
        for m in ["Z 8", "", "z 9", "Z 9 ", "Z6III", "Z 6_3", "Z6_3 ", "Z 50", "Z"] {
            assert!(caps_of(m).is_none(), "{m:?}");
            assert!(!ftp_absent(m) && !iptc_absent(m), "{m:?}: unknown is asked, not skipped");
            assert!(!ftp_write_confirmed(m) && !ftp_delete_confirmed(m) && !iptc_write_confirmed(m), "{m:?}");
            assert!(matches!(string_write_gate(m, 0xD072), Err(PtpUsbError::UnconfirmedModel(x)) if x == m), "{m:?}");
            assert!(!byte_write_confirmed(m, 0xD053) && !byte_write_confirmed(m, 0xD060), "{m:?}");
        }
    }

    /// The mechanisms over tables whose rows differ from the real one (every real row now confirms all three
    /// strings, so the real table cannot show the second refusal).
    #[test]
    fn the_string_gate_checks_the_model_first_then_the_property() {
        const ONLY_COMMENT: &[u16] = &[PROP_IMAGE_COMMENT];
        let base = ModelCaps { model: "A", ftp: Support::Unconfirmed, ftp_delete: false, iptc: Support::Unconfirmed, string_writes: ONLY_COMMENT, byte_writes: &[] };
        let table = [base, ModelCaps { model: "B", string_writes: &[], ..base }];
        assert!(string_write_gate_in(&table, "A", PROP_IMAGE_COMMENT).is_ok());
        for code in [PROP_ARTIST, PROP_COPYRIGHT] {
            let err = string_write_gate_in(&table, "A", code).unwrap_err();
            assert!(matches!(&err, PtpUsbError::PropWriteUnconfirmed { propcode, model } if *propcode == code && model == "A"), "{err:?}");
            let text = err.to_string();
            assert!(text.contains("has not been confirmed") && text.contains("--force-unconfirmed-model"), "{text}");
        }
        // A recognized model with no confirmed string, and an unrecognized one, fail on the model even for a property
        // the other model has: the error names the real cause.
        for m in ["B", "C"] {
            for code in [PROP_IMAGE_COMMENT, PROP_ARTIST] {
                assert!(matches!(string_write_gate_in(&table, m, code), Err(PtpUsbError::UnconfirmedModel(x)) if x == m), "{m} {code:#x}");
            }
        }
        assert!(string_write_gate_in(&[], "A", PROP_IMAGE_COMMENT).is_err(), "an empty table confirms nothing");
    }

    /// Every real row has the delete confirmed wherever the write is, so the real table cannot show the columns apart.
    /// Rows that differ in exactly one cell show that each predicate reads its own column and nothing else.
    #[test]
    fn each_predicate_reads_its_own_column_and_no_other() {
        let base = ModelCaps { model: "S", ftp: Support::Unconfirmed, ftp_delete: false, iptc: Support::Unconfirmed, string_writes: &[], byte_writes: &[] };
        // Unconfirmed (and the all-default row): nothing is absent and nothing is confirmed.
        assert!(!base.lacks_ftp() && !base.lacks_iptc() && !base.ftp_write_ok() && !base.ftp_delete_ok() && !base.iptc_write_ok() && !base.byte_write_ok(PROP_ENABLE_COPYRIGHT));
        // ftp column only.
        let ftp_ok = ModelCaps { ftp: Support::Confirmed, ..base };
        assert!(ftp_ok.ftp_write_ok() && !ftp_ok.ftp_delete_ok() && !ftp_ok.lacks_ftp() && !ftp_ok.iptc_write_ok() && !ftp_ok.lacks_iptc());
        let ftp_absent_row = ModelCaps { ftp: Support::Absent, ..base };
        assert!(ftp_absent_row.lacks_ftp() && !ftp_absent_row.ftp_write_ok() && !ftp_absent_row.lacks_iptc());
        // delete column only: a delete flag on a body whose FTP write is not confirmed confirms nothing.
        assert!(!ModelCaps { ftp_delete: true, ..base }.ftp_delete_ok());
        assert!(!ModelCaps { ftp_delete: true, ftp: Support::Absent, ..base }.ftp_delete_ok());
        assert!(ModelCaps { ftp_delete: true, ftp: Support::Confirmed, ..base }.ftp_delete_ok());
        // iptc column only.
        let iptc_ok = ModelCaps { iptc: Support::Confirmed, ..base };
        assert!(iptc_ok.iptc_write_ok() && !iptc_ok.lacks_iptc() && !iptc_ok.ftp_write_ok() && !iptc_ok.lacks_ftp());
        assert!(ModelCaps { iptc: Support::Absent, ..base }.lacks_iptc());
        // byte column: exactly the listed codes.
        let byte = ModelCaps { byte_writes: &[PROP_ENABLE_COPYRIGHT], ..base };
        assert!(byte.byte_write_ok(PROP_ENABLE_COPYRIGHT) && !byte.byte_write_ok(PROP_IPTC_PRESET) && !byte.byte_write_ok(0));
    }

    #[test]
    fn the_model_lookup_is_exact_and_first_match_is_the_only_match() {
        let c = ModelCaps { model: "X", ftp: Support::Unconfirmed, ftp_delete: false, iptc: Support::Unconfirmed, string_writes: &[], byte_writes: &[] };
        assert_eq!(caps_in(&[c], "X").map(|r| r.model), Some("X"));
        for m in ["x", "X ", " X", "XX", ""] {
            assert!(caps_in(&[c], m).is_none(), "{m:?}");
        }
        // An Unconfirmed FTP row is neither absent nor confirmed: asked at capture, refused for a write.
        assert!(c.ftp != Support::Absent && c.ftp != Support::Confirmed);
    }
}
