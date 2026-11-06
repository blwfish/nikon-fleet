//! The process exit statuses of the `nikon-fleet` CLI, in one place.
//!
//! The CLI ends with these, the `worker` subcommand and the MCP server's supervisor see them through the CLI, and the
//! GUI tests the status of the child it ran instead of reading its text. Each of those used to carry its own copy of
//! the numbers (the GUI had `EXIT_FTP_WRITE_INDETERMINATE = 4` next to the CLI's `EXIT_WRITE_INDETERMINATE = 4`, with
//! nothing tying the two together, so a renumbering on one side would have turned "check the camera" into a plain,
//! blindly retryable failure on the other). Everything now consumes [`ExitKind`]; the mapping from an error to a
//! status is [`exit_code_for`], here, for the CLI, and [`write_kind_of_ptp_usb_error`] is the one classification of
//! the camera's own error type that both it and the tests use.
//!
//! The closed vocabulary (the documented statuses; `--help` of the commands that use them says which do):
//!
//! | status | [`ExitKind`] | meaning |
//! |---|---|---|
//! | 0 | `Done` | done |
//! | 1 | `Failure` | any other error: a script may retry blindly |
//! | 2 | `Usage` | command-line usage error (clap's own) |
//! | 3 | `AlreadyArchived` | refused: that firmware model and version is already archived, nothing changed |
//! | 4 | `WriteIndeterminate` | a camera write was sent and its effect is unknown: look at the camera first |
//! | 5 | `StalePlan` | `--expect-digest` was given and the plan no longer matches: nothing was written |
//! | 6 | `RefusedBeforeWrite` | an apply was refused before it could write anything: nothing was written |

use crate::diag::Outcome;
use crate::firmware::FirmwareError;
use crate::firmware_browse::BrowseError;
use crate::firmware_source::FetchError;
use crate::ptp_usb::PtpUsbError;

/// A documented process exit status. Closed: a new status is a new variant, with a documented meaning, and the
/// exhaustive matches below and in the tests fail to compile until it is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Done,
    Failure,
    Usage,
    AlreadyArchived,
    WriteIndeterminate,
    StalePlan,
    RefusedBeforeWrite,
}

impl ExitKind {
    /// Every status, in numeric order.
    pub const ALL: [ExitKind; 7] = [
        ExitKind::Done,
        ExitKind::Failure,
        ExitKind::Usage,
        ExitKind::AlreadyArchived,
        ExitKind::WriteIndeterminate,
        ExitKind::StalePlan,
        ExitKind::RefusedBeforeWrite,
    ];

    /// The process exit status. The numbers are a contract with scripts, the GUI and the MCP server's callers.
    pub const fn code(self) -> i32 {
        match self {
            ExitKind::Done => 0,
            ExitKind::Failure => 1,
            ExitKind::Usage => 2,
            ExitKind::AlreadyArchived => 3,
            ExitKind::WriteIndeterminate => 4,
            ExitKind::StalePlan => 5,
            ExitKind::RefusedBeforeWrite => 6,
        }
    }

    /// The documented status `code` is, or `None` for any other number (a panic's 101, a shell's 127, a signal's
    /// absence). A caller that gets `None` for a camera write must treat the outcome as unknown.
    pub fn from_code(code: i32) -> Option<ExitKind> {
        ExitKind::ALL.into_iter().find(|k| k.code() == code)
    }
}

/// Exit status 3: "this model and version is already in the archive and nothing was changed": `firmware add` without
/// `--force`, or a fetch/browse whose bytes differ from the archived ones. A script or GUI tests for it instead of
/// reading the message.
pub const EXIT_ALREADY_ARCHIVED: i32 = ExitKind::AlreadyArchived.code();

/// Exit status 4: a camera write that failed after the data had been sent to the camera, so it may or may not have
/// changed (`vendor-write-ftp`, `vendor-write-ftp-blob`, `vendor-set-string` and the IPTC write:
/// [`PtpUsbError::FtpWriteIndeterminate`], [`PtpUsbError::PropWriteIndeterminate`],
/// [`PtpUsbError::IptcWriteIndeterminate`]). Distinct from the plain failure (1) a script is allowed to retry blindly:
/// this one needs a look at the camera first.
pub const EXIT_WRITE_INDETERMINATE: i32 = ExitKind::WriteIndeterminate.code();

/// Exit status 5: `--expect-digest` was given and the plan no longer matches; nothing was written.
pub const EXIT_STALE_PLAN: i32 = ExitKind::StalePlan.code();

/// Exit status 6: an apply (not a dry run) was refused before it could write anything; the camera is as it was. Every
/// other failure of an apply without a report may have written, so a caller treats that status, and no other, as
/// "nothing was written".
pub const EXIT_REFUSED_BEFORE_WRITE: i32 = ExitKind::RefusedBeforeWrite.code();

/// An error that carries the process exit status it should end the run with.
#[derive(Debug)]
pub struct ExitCodeError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ExitCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitCodeError {}

/// What a camera error means for the exit status: [`ExitKind::WriteIndeterminate`] when the camera may have changed
/// (the write's data had been sent when it failed), else [`ExitKind::Failure`]. Exhaustive on purpose, with no `_`
/// arm: a new `PtpUsbError` variant does not compile until someone decides which of the two it is, because a write
/// failure filed under "plain failure" invites the blind retry exit 4 exists to prevent.
pub fn write_kind_of_ptp_usb_error(e: &PtpUsbError) -> ExitKind {
    match e {
        PtpUsbError::FtpWriteIndeterminate { .. }
        | PtpUsbError::PropWriteIndeterminate { .. }
        | PtpUsbError::IptcWriteIndeterminate { .. } => ExitKind::WriteIndeterminate,
        PtpUsbError::NoDevice
        | PtpUsbError::AmbiguousDevice(_)
        | PtpUsbError::SerialNotFound { .. }
        | PtpUsbError::DuplicateSerial { .. }
        | PtpUsbError::BodiesSkipped { .. }
        | PtpUsbError::Usb(_)
        | PtpUsbError::CameraLock(_)
        | PtpUsbError::NoPtpInterface
        | PtpUsbError::ClaimFailed { .. }
        | PtpUsbError::ShortContainer(_)
        | PtpUsbError::TruncatedContainer { .. }
        | PtpUsbError::ShortWrite { .. }
        | PtpUsbError::ContainerTooLarge { .. }
        | PtpUsbError::TooManyCommandParams { .. }
        | PtpUsbError::MisalignedResponseParams(_)
        | PtpUsbError::TransactionTimedOut(_)
        | PtpUsbError::TransactionDataTooLarge { .. }
        | PtpUsbError::SessionPoisoned { .. }
        | PtpUsbError::OpenSessionFailed(_)
        | PtpUsbError::OperationFailed { .. }
        | PtpUsbError::TransactionIdMismatch { .. }
        | PtpUsbError::UnexpectedDataOp { .. }
        | PtpUsbError::NoDataPhase { .. }
        | PtpUsbError::DeviceInfoMalformed { .. }
        | PtpUsbError::FtpBlobMalformed { .. }
        | PtpUsbError::FtpInfoMalformed { .. }
        | PtpUsbError::FtpSlotOutOfRange { .. }
        | PtpUsbError::FtpProbeOpNotAllowed { .. }
        | PtpUsbError::FtpProbeDataTooLong { .. }
        | PtpUsbError::FtpProbeBadParams { .. }
        | PtpUsbError::FtpProbeDestructiveNotConfirmed { .. }
        | PtpUsbError::FtpFieldTooLong { .. }
        | PtpUsbError::FtpFieldNotAscii { .. }
        | PtpUsbError::FtpFieldControlChar { .. }
        | PtpUsbError::FtpWriteRefused { .. }
        | PtpUsbError::UnconfirmedModel(_)
        | PtpUsbError::PropWriteUnsupported { .. }
        | PtpUsbError::PropWriteUnconfirmed { .. }
        | PtpUsbError::PropNotWritable { .. }
        | PtpUsbError::PropValueBad { .. } => ExitKind::Failure,
    }
}

/// The archive error inside `e`, however it was wrapped: raised directly, inside a fetch
/// (`FetchError::Archive`), or inside a browse (`BrowseError::Ingest`).
pub fn firmware_error_in(e: &anyhow::Error) -> Option<&FirmwareError> {
    if let Some(f) = e.downcast_ref::<FirmwareError>() {
        return Some(f);
    }
    match (e.downcast_ref::<FetchError>(), e.downcast_ref::<BrowseError>()) {
        (Some(FetchError::Archive(f)), _) => Some(f),
        (_, Some(BrowseError::Ingest { error, .. })) => Some(error),
        _ => None,
    }
}

/// This model and version is already archived and the command refused to change it: the typed `FirmwareError`s for
/// "already there" (`AlreadyArchived`: `add` without `--force`; `ContentMismatch`: same version, different bytes).
pub fn is_already_archived(e: &anyhow::Error) -> bool {
    matches!(
        firmware_error_in(e),
        Some(FirmwareError::AlreadyArchived { .. } | FirmwareError::ContentMismatch { .. })
    )
}

/// The process exit status for a failed run: what an [`ExitCodeError`] carries (the CLI builds one for a stale plan or a
/// refused apply), else 3 for "already archived" ([`EXIT_ALREADY_ARCHIVED`]), 4 for a camera write that may have taken
/// ([`EXIT_WRITE_INDETERMINATE`], by [`write_kind_of_ptp_usb_error`]), else 1.
pub fn exit_code_for(e: &anyhow::Error) -> i32 {
    if let Some(x) = e.downcast_ref::<ExitCodeError>() {
        x.code
    } else if is_already_archived(e) {
        EXIT_ALREADY_ARCHIVED
    } else if let Some(p) = e.downcast_ref::<PtpUsbError>() {
        write_kind_of_ptp_usb_error(p).code()
    } else {
        ExitKind::Failure.code()
    }
}

/// How a command that ended with `code` is recorded in the diagnostics: [`Outcome::Ok`] for 0, [`Outcome::Indeterminate`] for
/// [`EXIT_WRITE_INDETERMINATE`] (the effect on the camera is unknown, which is what the report must say), else
/// [`Outcome::Failed`]. The one mapping from a status to a span outcome, so the entry points that record a span from a
/// status (the CLI's `main`) do not each decide it.
pub fn diag_outcome_for_exit(code: i32) -> Outcome {
    match ExitKind::from_code(code) {
        Some(ExitKind::Done) => Outcome::Ok,
        Some(ExitKind::WriteIndeterminate) => Outcome::Indeterminate,
        Some(
            ExitKind::Failure | ExitKind::Usage | ExitKind::AlreadyArchived | ExitKind::StalePlan | ExitKind::RefusedBeforeWrite,
        )
        | None => Outcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cause() -> Box<PtpUsbError> {
        Box::new(PtpUsbError::TransactionTimedOut(Duration::from_secs(1)))
    }

    /// One value of every `PtpUsbError` variant. (The production match is exhaustive, so a new variant fails to compile
    /// there first; this list is what the mapping test below walks.)
    fn every_variant() -> Vec<PtpUsbError> {
        use crate::ptp_usb::{DeviceInfoFault, FtpWriteRefusal, PtpResponseCode};
        vec![
            PtpUsbError::NoDevice,
            PtpUsbError::AmbiguousDevice(2),
            PtpUsbError::SerialNotFound { requested: "S".into(), unreadable: vec![] },
            PtpUsbError::DuplicateSerial { requested: "S".into(), count: 2 },
            PtpUsbError::BodiesSkipped { skipped: vec!["x".into()] },
            PtpUsbError::Usb(rusb::Error::Timeout),
            PtpUsbError::CameraLock(crate::camera_lock::LockError::NoPath),
            PtpUsbError::NoPtpInterface,
            PtpUsbError::ClaimFailed { attempts: 3, last_error: "x".into() },
            PtpUsbError::ShortContainer(3),
            PtpUsbError::TruncatedContainer { declared: 9, got: 3 },
            PtpUsbError::ShortWrite { written: 1, expected: 2 },
            PtpUsbError::ContainerTooLarge { payload_len: 1 },
            PtpUsbError::TooManyCommandParams { given: 9, max: 5 },
            PtpUsbError::MisalignedResponseParams(3),
            PtpUsbError::TransactionTimedOut(Duration::from_secs(1)),
            PtpUsbError::TransactionDataTooLarge { got: 2, limit: 1 },
            PtpUsbError::SessionPoisoned { cause: "x".into() },
            PtpUsbError::OpenSessionFailed(PtpResponseCode(0x2002)),
            PtpUsbError::OperationFailed { op: 0x90EE, code: PtpResponseCode(0x2002) },
            PtpUsbError::TransactionIdMismatch { op: 1, expected: 2, got: 3 },
            PtpUsbError::UnexpectedDataOp { op: 1, got: 2 },
            PtpUsbError::NoDataPhase { op: 1 },
            PtpUsbError::DeviceInfoMalformed { field: "f", offset: 0, fault: DeviceInfoFault::Truncated { needed: 4, available: 1 } },
            PtpUsbError::FtpBlobMalformed { what: "w", offset: 0, detail: "d".into() },
            PtpUsbError::FtpInfoMalformed { raw: vec![1, 2], cause: Box::new(PtpUsbError::FtpBlobMalformed { what: "w", offset: 0, detail: "d".into() }) },
            PtpUsbError::FtpSlotOutOfRange { slot: 5, count: 2 },
            PtpUsbError::FtpProbeOpNotAllowed { op: 1 },
            PtpUsbError::FtpProbeDataTooLong { len: 99 },
            PtpUsbError::FtpProbeBadParams { params: vec![u32::MAX] },
            PtpUsbError::FtpFieldTooLong { field: "f", len: 9, max: 1, unit: "bytes" },
            PtpUsbError::FtpFieldNotAscii { field: "f" },
            PtpUsbError::FtpFieldControlChar { field: "f" },
            PtpUsbError::FtpWriteIndeterminate { cause: cause() },
            PtpUsbError::FtpWriteRefused { slot: 9, param2: 1, why: FtpWriteRefusal::AddNotAtCount { slot: 9, count: 2 } },
            PtpUsbError::UnconfirmedModel("Z".into()),
            PtpUsbError::PropWriteUnsupported { propcode: 1, allowed: "a".into() },
            PtpUsbError::PropWriteUnconfirmed { propcode: 0xD072, model: "Z".into() },
            PtpUsbError::PropNotWritable { propcode: 1, get_set: 0 },
            PtpUsbError::PropValueBad { propcode: 1, why: "w".into() },
            PtpUsbError::PropWriteIndeterminate { propcode: 0xD090, cause: cause() },
            PtpUsbError::IptcWriteIndeterminate { cause: cause() },
        ]
    }

    /// Whether `e` is one of the three "data was sent, then it failed" variants, spelled out apart from the production
    /// match so the two can disagree and a test fail.
    fn is_indeterminate_variant(e: &PtpUsbError) -> bool {
        matches!(
            e,
            PtpUsbError::FtpWriteIndeterminate { .. }
                | PtpUsbError::PropWriteIndeterminate { .. }
                | PtpUsbError::IptcWriteIndeterminate { .. }
        )
    }

    #[test]
    fn the_numbers_are_the_documented_ones_and_distinct() {
        let table = [
            (ExitKind::Done, 0),
            (ExitKind::Failure, 1),
            (ExitKind::Usage, 2),
            (ExitKind::AlreadyArchived, 3),
            (ExitKind::WriteIndeterminate, 4),
            (ExitKind::StalePlan, 5),
            (ExitKind::RefusedBeforeWrite, 6),
        ];
        for (kind, code) in table {
            assert_eq!(kind.code(), code, "{kind:?} is documented as {code} in --help; scripts and the GUI test for it");
            assert_eq!(ExitKind::from_code(code), Some(kind), "round trip of {code}");
        }
        assert_eq!(table.len(), ExitKind::ALL.len(), "every status is in the table above");
        let mut codes: Vec<i32> = ExitKind::ALL.iter().map(|k| k.code()).collect();
        codes.dedup();
        assert_eq!(codes.len(), ExitKind::ALL.len(), "no two statuses share a number");
        assert_eq!(
            (EXIT_ALREADY_ARCHIVED, EXIT_WRITE_INDETERMINATE, EXIT_STALE_PLAN, EXIT_REFUSED_BEFORE_WRITE),
            (3, 4, 5, 6)
        );
        // Anything that is not a documented status is not mapped to one: a panic's 101, a shell's 127, a negative.
        for other in [-1, 7, 101, 124, 127, 137] {
            assert_eq!(ExitKind::from_code(other), None, "{other}");
        }
    }

    #[test]
    fn the_diagnostics_outcome_of_a_status_says_indeterminate_only_for_4() {
        assert_eq!(diag_outcome_for_exit(0), Outcome::Ok);
        assert_eq!(diag_outcome_for_exit(EXIT_WRITE_INDETERMINATE), Outcome::Indeterminate);
        for failed in [1, 2, 3, 5, 6, 101, 127, -1] {
            assert_eq!(diag_outcome_for_exit(failed), Outcome::Failed, "{failed}");
        }
    }

    #[test]
    fn the_apply_statuses_the_ops_module_publishes_are_these_numbers() {
        // One number, two names: ops::apply is where the library's apply code reaches them.
        assert_eq!(crate::ops::apply::EXIT_STALE_PLAN, EXIT_STALE_PLAN);
        assert_eq!(crate::ops::apply::EXIT_REFUSED_BEFORE_WRITE, EXIT_REFUSED_BEFORE_WRITE);
    }

    #[test]
    fn exactly_the_three_write_indeterminate_variants_are_status_4_and_every_other_variant_is_1() {
        let all = every_variant();
        assert!(all.len() >= 40, "the list covers every variant ({} listed)", all.len());
        let mut indeterminate = 0;
        for e in all {
            let want = if is_indeterminate_variant(&e) {
                indeterminate += 1;
                EXIT_WRITE_INDETERMINATE
            } else {
                1
            };
            let any = anyhow::Error::new(e);
            assert_eq!(exit_code_for(&any), want, "{any:?}");
        }
        assert_eq!(indeterminate, 3, "the FTP write, the property write and the IPTC write");
    }

    #[test]
    fn the_property_write_is_status_4_even_when_the_error_is_wrapped_in_context() {
        // vendor-set-string: SetDevicePropValue was sent, then the reply was lost. Exit 4, not 1.
        let e = anyhow::Error::new(PtpUsbError::PropWriteIndeterminate { propcode: 0xD090, cause: cause() });
        assert_eq!(exit_code_for(&e), EXIT_WRITE_INDETERMINATE);
        let wrapped = e.context("writing the property");
        assert_eq!(exit_code_for(&wrapped), EXIT_WRITE_INDETERMINATE, "a context layer does not hide the cause");
        let ftp = anyhow::Error::new(PtpUsbError::FtpWriteIndeterminate { cause: cause() }).context("writing the profile");
        assert_eq!(exit_code_for(&ftp), EXIT_WRITE_INDETERMINATE);
        let iptc = anyhow::Error::new(PtpUsbError::IptcWriteIndeterminate { cause: cause() }).context("writing the IPTC slot");
        assert_eq!(exit_code_for(&iptc), EXIT_WRITE_INDETERMINATE);
        // An error that is not the camera's, and a camera error that is a definite refusal, are plain failures.
        assert_eq!(exit_code_for(&anyhow::anyhow!("boom")), 1);
        let refused = anyhow::Error::new(PtpUsbError::PropNotWritable { propcode: 0xD090, get_set: 0 });
        assert_eq!(exit_code_for(&refused), 1, "nothing was sent: a blind retry is allowed");
    }

    #[test]
    fn a_status_an_error_carries_wins_and_an_undocumented_one_is_passed_through() {
        let stale: anyhow::Error = ExitCodeError { code: EXIT_STALE_PLAN, message: "stale".into() }.into();
        assert_eq!(exit_code_for(&stale), 5);
        // A carried status that is not in the vocabulary is still the status the process ends with (the CLI never
        // builds one).
        let odd: anyhow::Error = ExitCodeError { code: 42, message: "odd".into() }.into();
        assert_eq!(exit_code_for(&odd), 42);
        // The carried status outranks a camera error wrapped inside it: the outer layer is the one decided.
        let outer = anyhow::Error::new(PtpUsbError::FtpWriteIndeterminate { cause: cause() });
        let carried: anyhow::Error = ExitCodeError { code: EXIT_REFUSED_BEFORE_WRITE, message: format!("{outer:?}") }.into();
        assert_eq!(exit_code_for(&carried), EXIT_REFUSED_BEFORE_WRITE);
    }
}
