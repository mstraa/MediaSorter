//! MediaSorter server binary (LXC / systemd deployment).
//!
//! Domain logic and the HTTP app live in the `tvsorter` library crate
//! (`src/lib.rs`) so the desktop build (`src-tauri`) can reuse them.

use tvsorter::config::load_config;

#[tokio::main]
async fn main() {
    tvsorter::init_tracing();
    let config = load_config();
    tvsorter::serve(config).await;
}
