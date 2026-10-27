//! Argv for the CLI's raw-USB-PTP vendor commands (`vendor-read`,
//! `vendor-write-ftp`), for a GUI that runs them as a subprocess.
//!
//! Why a subprocess at all (finding nikon-fleet-20260909-7b2e#12): the
//! vendor ops claim the camera's PTP interface by repeatedly SIGKILLing
//! macOS's `ptpcamerad`/`icdd` and re-asserting the USB configuration
//! (`ptp_usb::claim_with_retry`). Run in-process from the egui GUI's worker
//! thread, that is the same shape as the USB-reset crash fixed in commit
//! 48704d5 (USB device events reaching the AppKit run loop mid-frame), and
//! whether it triggers the same crash was never verified. The Python GUI
//! already shells out to the CLI for these; the egui GUI now does too, so
//! the churn happens in a separate process that cannot corrupt the GUI's
//! drawing surface, and a crash there surfaces as an error message.
//!
//! These builders live in the library (not the GUI crate) so the CLI's own
//! integration tests can feed their exact output to the real binary — see
//! `tests/vendor_argv_cli.rs`.
//!
//! Every value flag uses the `--flag=value` form: clap would otherwise
//! reject a value that starts with `-` (a profile name or SSID like
//! `-guest`) as an unknown flag.

use crate::ptp_usb::FtpProfile;

/// `vendor-read 0xNNNN [--serial=S]`.
pub fn vendor_read_argv(propcode: u16, serial: Option<&str>) -> Vec<String> {
    let mut argv = vec!["vendor-read".to_string(), format!("0x{propcode:04x}")];
    if let Some(s) = serial {
        argv.push(format!("--serial={s}"));
    }
    argv
}

/// `vendor-write-ftp ... --password-stdin [--serial=S]`.
///
/// The password is deliberately NOT in the argv (it would be visible to
/// other local users via `ps` while the CLI runs): the caller must write
/// `profile.password` to the child's stdin and close it.
pub fn vendor_write_ftp_argv(profile: &FtpProfile, serial: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "vendor-write-ftp".to_string(),
        format!("--profile-name={}", profile.profile_name),
        format!("--ssid-24ghz={}", profile.ssid_24ghz),
        format!("--ssid-5ghz={}", profile.ssid_5ghz),
        format!("--host={}", profile.host),
        format!("--port={}", profile.port),
        format!("--username={}", profile.username),
        "--password-stdin".to_string(),
    ];
    if let Some(s) = serial {
        argv.push(format!("--serial={s}"));
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> FtpProfile {
        FtpProfile {
            profile_name: "CLAUDE".to_string(),
            ssid_24ghz: "net24".to_string(),
            ssid_5ghz: "net5".to_string(),
            host: "ftp.example.com".to_string(),
            port: 2121,
            username: "user".to_string(),
            password: "s3cret-pw".to_string(),
        }
    }

    #[test]
    fn read_argv_zero_pads_the_propcode_to_four_hex_digits() {
        assert_eq!(vendor_read_argv(0x0053, None), vec!["vendor-read", "0x0053"]);
        assert_eq!(vendor_read_argv(0xD053, None), vec!["vendor-read", "0xd053"]);
    }

    #[test]
    fn read_argv_includes_serial_only_when_given() {
        assert_eq!(vendor_read_argv(0xD053, Some("123")), vec!["vendor-read", "0xd053", "--serial=123"]);
        assert_eq!(vendor_read_argv(0xD053, None).len(), 2);
    }

    #[test]
    fn write_argv_never_contains_the_password() {
        let argv = vendor_write_ftp_argv(&profile(), Some("serial"));
        assert!(argv.iter().all(|a| !a.contains("s3cret-pw")), "{argv:?}");
        assert!(argv.iter().any(|a| a == "--password-stdin"));
        assert!(!argv.iter().any(|a| a.starts_with("--password=") || a == "--password"));
    }

    #[test]
    fn write_argv_carries_every_field_in_equals_form() {
        let argv = vendor_write_ftp_argv(&profile(), None);
        assert_eq!(
            argv,
            vec![
                "vendor-write-ftp",
                "--profile-name=CLAUDE",
                "--ssid-24ghz=net24",
                "--ssid-5ghz=net5",
                "--host=ftp.example.com",
                "--port=2121",
                "--username=user",
                "--password-stdin",
            ]
        );
    }

    #[test]
    fn write_argv_keeps_a_leading_dash_value_attached_to_its_flag() {
        let mut p = profile();
        p.profile_name = "-guest".to_string();
        let argv = vendor_write_ftp_argv(&p, None);
        assert!(argv.contains(&"--profile-name=-guest".to_string()));
    }

    #[test]
    fn write_argv_serial_goes_last() {
        let argv = vendor_write_ftp_argv(&profile(), Some("S1"));
        assert_eq!(argv.last().map(String::as_str), Some("--serial=S1"));
    }
}
