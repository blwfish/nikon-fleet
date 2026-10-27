//! The embedded browser window for `fleet firmware browse` (cargo feature
//! `webview`). A thin shell: it opens the release page in the OS web view
//! (WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux, via `wry`
//! on a `tao` event loop) and forwards the browser's navigation and download
//! callbacks to `firmware_browse::DownloadTracker`. It does not touch the page:
//! no script is injected and nothing is clicked — the human reads the licence,
//! ticks "I agree" and presses Download.
//!
//! This file cannot be unit-tested headlessly. Everything with logic in it
//! lives in `firmware_browse`; what is left here is wiring.
//!
//! The event loop must run on the main thread, which is why the GUI starts
//! this as a subprocess (`nikon-fleet firmware browse`) rather than embedding it.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

use crate::firmware_browse::{BrowseError, BrowserWindow, DownloadTracker, Finish};

pub struct EmbeddedBrowser;

/// Sent from the download-completed callback to stop the event loop.
enum UserEvent {
    Captured,
}

impl BrowserWindow for EmbeddedBrowser {
    fn run(
        &self,
        page_url: &str,
        staging: &Path,
        tracker: Rc<RefCell<DownloadTracker>>,
    ) -> Result<(), BrowseError> {
        let unavailable = |what: &str, e: &dyn std::fmt::Display| {
            BrowseError::WebViewUnavailable(format!("{what}: {e}"))
        };

        let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        let window = WindowBuilder::new()
            .with_title("Nikon firmware download — accept the licence and click Download")
            .with_inner_size(LogicalSize::new(1100.0, 800.0))
            .build(&event_loop)
            .map_err(|e| unavailable("creating the window", &e))?;

        // Ephemeral profile: incognito where the platform supports it (macOS,
        // Linux, WebView2 >= 101), and in any case a data directory inside the
        // staging dir, which is deleted afterwards. wry ignores the context
        // when incognito is on.
        let mut context = WebContext::new(Some(staging.join("webview-data")));

        let nav = tracker.clone();
        let started = tracker.clone();
        let finished = tracker.clone();
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_url(page_url)
            .with_incognito(true)
            .with_navigation_handler(move |url| nav.borrow_mut().on_navigate(&url))
            .with_download_started_handler(move |url, dest| {
                match started.borrow_mut().on_download_started(&url, dest) {
                    Some(staged) => {
                        *dest = staged;
                        true
                    }
                    None => false,
                }
            })
            .with_download_completed_handler(move |url, path, ok| {
                let outcome = finished.borrow_mut().on_download_finished(&url, path.as_deref(), ok);
                if outcome == Finish::Captured {
                    // Ignore a send error: it means the loop already exited.
                    let _ = proxy.send_event(UserEvent::Captured);
                }
            });

        #[cfg(not(target_os = "linux"))]
        let webview = builder.build(&window);
        #[cfg(target_os = "linux")]
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            builder.build_gtk(window.default_vbox().ok_or_else(|| {
                BrowseError::WebViewUnavailable("the window has no GTK container".into())
            })?)
        };
        // On Windows a missing/old WebView2 runtime fails here.
        let webview = webview.map_err(|e| unavailable("creating the web view", &e))?;

        window.set_focus();
        event_loop.run_return(|event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::WindowEvent { event: WindowEvent::CloseRequested, .. }
                | Event::UserEvent(UserEvent::Captured) => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        });

        // The web view must go before the window it lives in.
        drop(webview);
        drop(window);
        Ok(())
    }
}
