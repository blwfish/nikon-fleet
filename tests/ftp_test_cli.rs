//! `nikon-fleet ftp-test` through the real binary, against a small FTP server run here. (The whole transfer, the SFTP
//! side and the failure kinds are tested against fuller servers in `src/ftp_test.rs`; this is the command line:
//! arguments, the password path, exit status, the report, and testing a profile stored in a snapshot.)

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Output, Stdio};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_nikon-fleet");

/// An FTP server that knows login, PWD and CWD (no transfers: the tests use `--no-file-check`).
fn ftp_server(user: &'static str, pass: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let say = |w: &mut std::net::TcpStream, t: &str| {
                    let _ = w.write_all(format!("{t}\r\n").as_bytes());
                };
                say(&mut w, "220 cli test server");
                let (mut user_ok, mut line) = (false, String::new());
                loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let c = line.trim_end();
                    let (v, a) = c.split_once(' ').unwrap_or((c, ""));
                    match v.to_uppercase().as_str() {
                        "USER" => {
                            user_ok = a == user;
                            say(&mut w, "331 password");
                        }
                        "PASS" => say(&mut w, if user_ok && a == pass { "230 ok" } else { "530 Login incorrect." }),
                        "TYPE" => say(&mut w, "200 ok"),
                        "PWD" => say(&mut w, "257 \"/\" current"),
                        "CWD" => say(&mut w, if a == "incoming" { "250 ok" } else { "550 No such file or directory." }),
                        "QUIT" => {
                            say(&mut w, "221 bye");
                            return;
                        }
                        _ => say(&mut w, "502 no"),
                    }
                }
            });
        }
    });
    port
}

fn run(data_dir: &Path, args: &[&str]) -> Output {
    common::cli(BIN).arg("--data-dir").arg(data_dir).args(args).output().expect("spawn CLI")
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_server_that_accepts_the_login_and_the_folder_passes_and_the_password_is_never_printed() {
    let port = ftp_server("fleet", "s3cret!");
    let d = tempfile::tempdir().unwrap();
    let p = port.to_string();
    let o = run(d.path(), &["ftp-test", "--host", "127.0.0.1", "--port", &p, "--user", "fleet", "--password", "s3cret!", "--folder", "incoming", "--no-file-check"]);
    assert!(o.status.success(), "{}\n{}", out(&o), err(&o));
    assert!(out(&o).contains("result: the server accepted this profile") && out(&o).contains("skipped  put a test file: not asked for"), "{}", out(&o));
    assert!(!out(&o).contains("s3cret!") && !err(&o).contains("s3cret!"));
    // JSON
    let o = run(d.path(), &["ftp-test", "--host", "127.0.0.1", "--port", &p, "--user", "fleet", "--password", "s3cret!", "--no-file-check", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out(&o)).expect("JSON on stdout");
    assert_eq!((v["ok"].clone(), v["steps"][2]["step"].clone(), v["steps"][2]["outcome"].clone()), (true.into(), "login".into(), "ok".into()));
    assert!(!out(&o).contains("s3cret!"), "no password in the JSON");
}

#[test]
fn the_password_can_come_from_stdin() {
    let port = ftp_server("fleet", "from stdin");
    let d = tempfile::tempdir().unwrap();
    let mut child = common::cli(BIN)
        .arg("--data-dir")
        .arg(d.path())
        .args(["ftp-test", "--host", "127.0.0.1", "--port", &port.to_string(), "--user", "fleet", "--password-stdin", "--no-file-check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"from stdin\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}\n{}", out(&o), err(&o));
}

#[test]
fn a_refused_login_exits_non_zero_names_the_step_and_shows_the_servers_words() {
    let port = ftp_server("fleet", "right");
    let d = tempfile::tempdir().unwrap();
    let o = run(d.path(), &["ftp-test", "--host", "127.0.0.1", "--port", &port.to_string(), "--user", "fleet", "--password", "wrong", "--no-file-check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(out(&o).contains("FAILED   log in") && out(&o).contains("530") && out(&o).contains("Login incorrect") && out(&o).contains("did not pass"), "{}", out(&o));
    let o = run(d.path(), &["ftp-test", "--host", "127.0.0.1", "--port", &port.to_string(), "--user", "fleet", "--password", "right", "--folder", "nope", "--no-file-check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(out(&o).contains("FAILED   go to the folder") && out(&o).contains("550"), "{}", out(&o));
}

#[test]
fn nothing_listening_is_a_failed_connect_with_the_address_tried() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let d = tempfile::tempdir().unwrap();
    let o = run(d.path(), &["ftp-test", "--host", "127.0.0.1", "--port", &port.to_string(), "--user", "u", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!((v["ok"].clone(), v["steps"][1]["kind"].clone()), (false.into(), "connection_refused".into()), "{}", out(&o));
    assert!(v["steps"][1]["message"].as_str().unwrap().contains(&format!("127.0.0.1:{port}")));
}

#[test]
fn a_profile_stored_in_a_snapshot_is_tested_with_its_own_settings() {
    use nikon_fleet::extended::{Extended, FtpProfileEntry, FtpState, FtpStore, IptcState, ReadFrom};
    use nikon_fleet::ftp_edit::{ProfileFields, ftp_add};
    use nikon_fleet::snapshot::{Camera, Snapshot, Transport};
    let port = ftp_server("fleet", "s3cret!");
    let d = tempfile::tempdir().unwrap();
    let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
    s.extended = Some(Extended {
        read_from: ReadFrom { model: "Z 9".into(), usb_serial: "S1".into() },
        props: Default::default(),
        ftp: FtpState::Read(FtpStore { count: 0, limit: 20, active_slot: None, device_name: "Z 9_S1".into(), truncated: false, profiles: Vec::<FtpProfileEntry>::new() }),
        iptc: IptcState::NotOnModel,
    });
    let mut copy = nikon_fleet::snapshot_edit::copy_for_edit(&s, "s.json", None, "2026-11-06T12:00:00Z");
    let f = ProfileFields {
        name: Some("local".into()),
        host: Some("127.0.0.1".into()),
        username: Some("fleet".into()),
        password: Some("s3cret!".into()),
        folder: Some("incoming".into()),
        port: Some(port),
        ..Default::default()
    };
    ftp_add(&mut copy, &f, None, "2026-11-06T12:00:00Z").unwrap();
    std::fs::create_dir_all(d.path().join("snapshots")).unwrap();
    copy.save_to_file(d.path().join("snapshots").join("copy.json")).unwrap();
    let o = run(d.path(), &["ftp-test", "--snapshot", "copy.json", "--profile", "0", "--no-file-check"]);
    assert!(o.status.success(), "{}\n{}", out(&o), err(&o));
    assert!(out(&o).contains(&format!("FTP 127.0.0.1:{port}  folder incoming")) && !out(&o).contains("s3cret!"), "{}", out(&o));
    let o = run(d.path(), &["ftp-test", "--snapshot", "copy.json", "--profile", "5"]);
    assert!(!o.status.success() && err(&o).contains("ftp_index"), "{}", err(&o));
}

#[test]
fn the_arguments_that_make_no_sense_are_usage_errors() {
    let d = tempfile::tempdir().unwrap();
    for args in [
        vec!["ftp-test"],
        vec!["ftp-test", "--host", "h"],
        vec!["ftp-test", "--user", "u"],
        vec!["ftp-test", "--snapshot", "x.json"],
        vec!["ftp-test", "--profile", "0"],
        vec!["ftp-test", "--snapshot", "x.json", "--profile", "0", "--host", "h"],
        vec!["ftp-test", "--host", "h", "--user", "u", "--timeout", "0"],
        vec!["ftp-test", "--host", "h", "--user", "u", "--timeout", "301"],
        vec!["ftp-test", "--host", "h", "--user", "u", "--protocol", "ftps"],
        vec!["ftp-test", "--host", "h", "--user", "u", "--password", "x", "--password-stdin"],
    ] {
        assert_eq!(run(d.path(), &args).status.code(), Some(2), "{args:?}");
    }
}
