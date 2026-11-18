//! `/api/dedupe/*`: the Duplicates module.

use axum::extract::{Path as AxumPath, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dedupe::apply::{self, ApplyProgress, ApplyRequest, HistoryPage};
use crate::dedupe::groups::{self, GroupsFilter, GroupsPage};
use crate::dedupe::settings::{DedupeSettings, DedupeSettingsPayload};
use crate::dedupe::store::{self, UnusedTorrents};
use crate::dedupe::{check_roots, zfs_info, RootsReport, ScanStatus, ZfsInfo};
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
        .route("/api/dedupe/groups", get(list_groups))
        .route("/api/dedupe/jobs", get(latest_job).post(start_job))
        .route("/api/dedupe/jobs/:id", get(get_job))
        .route("/api/dedupe/jobs/:id/cancel", post(cancel_job))
        .route("/api/dedupe/history", get(history))
        .route("/api/dedupe/space", get(space))
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

#[derive(Deserialize)]
struct GroupsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// Exact pair label from the summary ("Films ↔ Movies").
    pair: Option<String>,
    /// Only groups with a path under this folder (inside a dedupe root).
    folder: Option<String>,
    /// Minimum file size in bytes.
    min_size: Option<i64>,
}

async fn list_groups(
    State(state): State<AppState>,
    Query(query): Query<GroupsQuery>,
) -> AppResult<Json<GroupsPage>> {
    let service = state.dedupe.clone();
    let page = blocking(move || -> Result<GroupsPage, AppError> {
        let settings = service.settings();
        let folder = query
            .folder
            .filter(|f| !f.trim().is_empty())
            .map(|f| crate::dedupe::settings::normalize_root(&f));
        // Same guard as browsing: a folder filter must stay inside the roots.
        if let Some(folder) = &folder {
            if !settings.roots.iter().any(|root| folder.starts_with(root)) {
                return Err(AppError::bad_request("Folder is outside the dedupe roots"));
            }
        }
        let filter = GroupsFilter {
            pair: query.pair.filter(|p| !p.is_empty()),
            min_size: query.min_size.unwrap_or(0).max(0),
            folder: folder.map(|f| f.display().to_string()),
            limit: query.limit.unwrap_or(50).clamp(1, 500),
            offset: query.offset.unwrap_or(0).max(0),
        };
        let conn = service.store().lock();
        groups::list_groups(&conn, &filter, settings.preferred_owner).map_err(|e| {
            AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("dedupe.db error: {e}"),
            )
        })
    })
    .await??;
    Ok(Json(page))
}

async fn start_job(
    State(state): State<AppState>,
    Json(request): Json<ApplyRequest>,
) -> AppResult<Json<ApplyProgress>> {
    let service = state.dedupe.clone();
    let job = blocking(move || service.start_apply(request))
        .await?
        .map_err(AppError::conflict)?;
    Ok(Json(job.snapshot()))
}

async fn latest_job(State(state): State<AppState>) -> Json<Option<ApplyProgress>> {
    Json(state.dedupe.latest_apply_job().map(|job| job.snapshot()))
}

async fn get_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<ApplyProgress>> {
    let job = state
        .dedupe
        .apply_job(&id)
        .ok_or_else(|| AppError::not_found("Unknown duplicate job"))?;
    Ok(Json(job.snapshot()))
}

/// Stops between two files, never in the middle of a replacement.
async fn cancel_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let job = state
        .dedupe
        .apply_job(&id)
        .ok_or_else(|| AppError::not_found("Unknown duplicate job"))?;
    job.control.stop();
    Ok(Json(json!({ "status": "stopping" })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// simulate | apply
    mode: Option<String>,
}

async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Json<HistoryPage>> {
    let service = state.dedupe.clone();
    let page = blocking(move || {
        let mode = query.mode.filter(|m| m == "simulate" || m == "apply");
        let conn = service.store().lock();
        apply::history(
            &conn,
            mode.as_deref(),
            query.limit.unwrap_or(200).clamp(1, 2000),
            query.offset.unwrap_or(0).max(0),
        )
    })
    .await?
    .map_err(|e| {
        AppError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("dedupe.db error: {e}"),
        )
    })?;
    Ok(Json(page))
}

async fn space(State(state): State<AppState>) -> AppResult<Json<ZfsInfo>> {
    let service = state.dedupe.clone();
    Ok(Json(
        blocking(move || zfs_info(&service.settings().roots)).await?,
    ))
}
