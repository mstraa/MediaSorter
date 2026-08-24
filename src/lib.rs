//! MediaSorter core library.
//!
//! The HTTP server and all domain logic live here so that multiple front-ends
//! can reuse them:
//! - `src/main.rs` runs the long-lived server (LXC / systemd deployment).
//! - `src-tauri` embeds the server on a loopback port and wraps it in a native
//!   desktop window (`spawn_embedded`).

pub mod assets;
pub mod config;
pub mod db;
pub mod error;
pub mod ffprobe;
pub mod filesystem;
pub mod formatting;
pub mod importer;
pub mod jobs;
pub mod library;
pub mod models;
pub mod naming;
pub mod parser;
pub mod providers;
pub mod routes;
pub mod state;

use std::net::SocketAddr;

use axum::http::HeaderValue;
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config::AppConfig;
use crate::db::Database;
use crate::jobs::JobManager;
use crate::providers::MetadataProviders;
use crate::state::AppState;

/// Initialize tracing/logging. Safe to call once at process start.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tvsorter=info,tower_http=warn".into()),
        )
        .init();
}

/// CORS policy for the API.
///
/// In production the SPA is embedded and served from this same origin, so no
/// cross-origin access is needed at all. Only the Vite dev server (a different
/// port) needs it, so the allow-list is limited to loopback dev origins.
///
/// This matters because the API is unauthenticated: with `allow_origin(Any)`,
/// any page the user happens to visit could call `/api/folders` to enumerate
/// the server's filesystem, or `/api/import-jobs` to move their media around.
/// Set `TVSORTER_CORS_ORIGINS` (comma-separated) to allow additional origins,
/// e.g. a Vite dev server running on another machine.
fn cors_layer() -> CorsLayer {
    let mut origins: Vec<HeaderValue> = ["http://127.0.0.1:5173", "http://localhost:5173"]
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();

    if let Ok(extra) = std::env::var("TVSORTER_CORS_ORIGINS") {
        origins.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|o| !o.is_empty())
                .filter_map(|o| o.parse::<HeaderValue>().ok()),
        );
    }

    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// Build the fully-wired axum application (state, routes, CORS, tracing layer).
pub fn build_app(config: AppConfig) -> Router {
    let db = Database::open(&config.database_path).expect("failed to open database");
    let providers = MetadataProviders::new(db.clone());
    let state = AppState {
        config,
        db,
        providers,
        jobs: JobManager::new(),
    };

    routes::build_router(state)
        .layer(cors_layer())
        .layer(TraceLayer::new_for_http())
}

/// Run the server until shutdown, binding to `config.host:config.port`.
/// Used by the standalone binary (LXC / systemd deployment).
pub async fn serve(config: AppConfig) {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("invalid host/port");
    let app = build_app(config);
    tracing::info!("MediaSorter listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind address");
    axum::serve(listener, app).await.expect("server error");
}

/// Bind an ephemeral loopback port, serve the app on a dedicated background
/// thread (with its own Tokio runtime), and return the bound address.
///
/// Used by the desktop (Tauri) build: the native webview then navigates to the
/// returned `http://127.0.0.1:<port>` address. The socket is bound before this
/// function returns, so connections from the webview queue until the server
/// starts accepting — no race on first paint.
pub fn spawn_embedded() -> SocketAddr {
    let mut config = config::load_config();

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind embedded server");
    let addr = listener
        .local_addr()
        .expect("failed to read embedded server address");
    config.host = addr.ip().to_string();
    config.port = addr.port();
    tracing::info!("MediaSorter embedded server on http://{addr}");

    let app = build_app(config);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build embedded Tokio runtime");
        runtime.block_on(async move {
            listener
                .set_nonblocking(true)
                .expect("failed to set listener non-blocking");
            let listener =
                tokio::net::TcpListener::from_std(listener).expect("failed to adopt std listener");
            axum::serve(listener, app)
                .await
                .expect("embedded server error");
        });
    });

    addr
}
