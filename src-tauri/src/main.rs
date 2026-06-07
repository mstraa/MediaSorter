//! MediaSorter desktop entry point.
//!
//! Strategy: "embedded server". We start the exact same axum server used for
//! the LXC deployment on a loopback port, then point a native Tauri webview at
//! it. No HTTP handlers are rewritten as Tauri commands — the desktop app and
//! the server share one codebase (`tvsorter` library crate).

// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{WebviewUrl, WebviewWindowBuilder};

fn main() {
    tvsorter::init_tracing();

    // Bind + serve the embedded server; returns the loopback address to load.
    let addr = tvsorter::spawn_embedded();
    let url = format!("http://{addr}");
    let webview_url = WebviewUrl::External(url.parse().expect("invalid embedded server URL"));

    tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", webview_url.clone())
                .title("MediaSorter")
                .inner_size(1200.0, 800.0)
                .min_inner_size(900.0, 600.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running MediaSorter desktop");
}
