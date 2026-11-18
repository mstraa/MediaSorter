//! `/api/dedupe/*`: the Duplicates module.

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dedupe::settings::{DedupeSettings, DedupeSettingsPayload};
use crate::dedupe::store::{self, UnusedTorrents};
use crate::dedupe::{check_roots, RootsReport, ScanStatus};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/dedupe/settings", get(get_settings).put(save_settings))
        .route("/api/dedupe/scan", post(start_scan))
        .route("/api/dedupe/scan/status", get(scan_status))
        .route("/api/dedupe/scan/pause", post(pause_scan))
        .route("/api/dedupe/scan/resume", post(resume_scan))
        .route("/api/dedupe/scan/cancel", post(cancel_scan))
        .route("/api/dedupe/scan/clear", post(clear_scan))
        .route("/api/dedupe/unused-torrents", get(unused_torrents))
}

/// Run blocking work (filesystem checks, `dedupe.db` reads) off the async
/// workers.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> AppResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Serialize)]
struct SettingsView {
    settings: DedupeSettingsPayload,
    report: RootsReport,
}

fn settings_view(settings: &DedupeSettings) -> SettingsView {
    SettingsView {
        settings: settings.to_payload(),
        report: check_roots(&settings.roots),
    }
}

async fn get_settings(State(state): State<AppState>) -> AppResult<Json<SettingsView>> {
    let service = state.dedupe.clone();
    let view = blocking(move || settings_view(&service.settings())).await?;
    Ok(Json(view))
}

async fn save_settings(
    State(state): State<AppState>,
    Json(payload): Json<DedupeSettingsPayload>,
) -> AppResult<Json<SettingsView>> {
    let service = state.dedupe.clone();
    let saved = blocking(move || DedupeSettings::save(service.db(), &payload))
        .await?
        .map_err(AppError::bad_request)?;
    let view = blocking(move || settings_view(&saved)).await?;
    Ok(Json(view))
}

#[derive(Deserialize, Default)]
struct StartScanBody {
    /// Discard an unfinished run and walk everything again.
    #[serde(default)]
    fresh: bool,
}

async fn start_scan(
    State(state): State<AppState>,
    body: Option<Json<StartScanBody>>,
) -> AppResult<Json<ScanStatus>> {
    let fresh = body.map(|b| b.0.fresh).unwrap_or(false);
    let service = state.dedupe.clone();
    blocking(move || service.start_scan(fresh))
        .await?
        .map_err(AppError::conflict)?;
    scan_status(State(state)).await
}

async fn scan_status(State(state): State<AppState>) -> AppResult<Json<ScanStatus>> {
    let service = state.dedupe.clone();
    Ok(Json(blocking(move || service.scan_status()).await?))
}

fn no_scan() -> AppError {
    AppError::conflict("No scan is running")
}

async fn pause_scan(State(state): State<AppState>) -> AppResult<Json<Value>> {
    if !state.dedupe.pause_scan() {
        return Err(no_scan());
    }
    Ok(Json(json!({ "status": "pausing" })))
}

async fn resume_scan(State(state): State<AppState>) -> AppResult<Json<Value>> {
    if !state.dedupe.resume_scan() {
        return Err(no_scan());
    }
    Ok(Json(json!({ "status": "running" })))
}

async fn cancel_scan(State(state): State<AppState>) -> AppResult<Json<Value>> {
    if !state.dedupe.cancel_scan() {
        return Err(no_scan());
    }
    Ok(Json(json!({ "status": "stopping" })))
}

async fn clear_scan(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let service = state.dedupe.clone();
    blocking(move || service.clear_scan_cache())
        .await?
        .map_err(AppError::conflict)?;
    Ok(Json(json!({ "status": "cleared" })))
}

#[derive(Deserialize)]
struct PageQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Serialize)]
struct UnusedTorrentsView {
    torrent_roots: Vec<String>,
    /// When the data was gathered: `nlink` is as of the last scan.
    last_scan_finished_at: Option<String>,
    #[serde(flatten)]
    result: UnusedTorrents,
}

async fn unused_torrents(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<UnusedTorrentsView>> {
    let limit = query.limit.unwrap_or(200).clamp(1, 2000);
    let offset = query.offset.unwrap_or(0).max(0);
    let service = state.dedupe.clone();
    let view = blocking(move || -> Result<UnusedTorrentsView, String> {
        let settings = service.settings();
        let conn = service.store().lock();
        let result = store::unused_torrents(&conn, &settings.torrent_roots, limit, offset)
            .map_err(|e| format!("dedupe.db error: {e}"))?;
        let last = store::latest_completed_run(&conn)
            .ok()
            .flatten()
            .and_then(|r| r.finished_at);
        Ok(UnusedTorrentsView {
            torrent_roots: settings
                .torrent_roots
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            last_scan_finished_at: last,
            result,
        })
    })
    .await?
    .map_err(|e| AppError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(view))
}
