//! Smoke test for the embedded browser plumbing, run by hand on a machine
//! with a display: `cargo run --example webview_smoke --features webview`.
//!
//! It serves a page from 127.0.0.1 that starts a download of a small file by
//! itself, and checks that the real window (a) opened, (b) let the navigation
//! through, (c) routed the download into the staging directory we chose, and
//! (d) closed itself once the download completed. This is OUR page, not
//! Nikon's: nothing here touches the licence gate.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::rc::Rc;
use std::thread;

use nikon_fleet::firmware_browse::{BrowserWindow, DownloadTracker};
use nikon_fleet::firmware_webview::EmbeddedBrowser;
use url::Url;

const PAYLOAD: &[u8] = b"smoke-test-firmware-bytes";

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let page_url = format!("http://127.0.0.1:{port}/page.html");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (head, body): (String, Vec<u8>) = if req.starts_with("GET /page.html") {
                let html = b"<html><body>smoke page<script>setTimeout(function(){location.href='/F-Z9-V0533W.exe'},500)</script></body></html>".to_vec();
                ("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n".into(), html)
            } else if req.starts_with("GET /F-Z9-V0533W.exe") {
                ("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"F-Z9-V0533W.exe\"\r\n".into(), PAYLOAD.to_vec())
            } else {
                ("HTTP/1.1 404 Not Found\r\n".into(), vec![])
            };
            let _ = write!(s, "{head}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });

    let staging = tempfile::tempdir().unwrap();
    let tracker = Rc::new(RefCell::new(DownloadTracker::new(
        &Url::parse(&page_url).unwrap(),
        staging.path(),
        |_| false,
    )));
    EmbeddedBrowser.run(&page_url, staging.path(), tracker.clone()).expect("window ran");

    let t = tracker.borrow();
    let c = t.captured().expect("no download was captured (window closed first?)");
    assert!(c.path.starts_with(staging.path()), "download escaped staging: {}", c.path.display());
    assert_eq!(c.original_name, "F-Z9-V0533W.exe");
    assert_eq!(std::fs::read(&c.path).expect("staged file"), PAYLOAD);
    println!("OK: window opened, download routed to {} and window closed itself", c.path.display());
}
