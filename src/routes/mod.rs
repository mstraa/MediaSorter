use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::assets::static_handler;
use crate::db::ImportRow;
use crate::error::{AppError, AppResult};
use crate::filesystem::{
    canonical_or_normalized, expand_grouped, expand_grouped_audio, expand_source_files,
    is_relative_to, list_directory, resolve_under_root,
};
use crate::formatting::human_file_size;
use crate::importer::{preview_import, ImportRequest};
use crate::models::*;
use crate::parser::{parse_film_filename, ParsedMedia};
use crate::providers::{EpisodeCandidate, ShowCandidate};
use crate::state::{
    is_valid_action, is_valid_conflict_policy, is_valid_media_type, AppState, MEDIA_TYPES,
    PICKER_ROOTS,
};

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/api/settings", get(get_settings).put(save_settings))
        .route("/api/browse", get(browse))
        .route("/api/match", post(match_route))
        .route("/api/preview", post(preview))
        .route(
            "/api/import-jobs",
            get(list_import_jobs).post(start_import_job),
        )
        .route("/api/import-jobs/clear", post(clear_import_jobs))
        .route("/api/import-jobs/:id", get(get_import_job))
        .route("/api/import-jobs/:id/cancel", post(cancel_import_job))
        .route(
            "/api/import-jobs/:id/items/:index/cancel",
            post(cancel_import_item),
        )
        .route("/api/import-jobs/:id/results", get(import_job_results))
        .route("/api/library", get(library))
        .route("/api/library/rescan", post(rescan_library))
        .route("/api/history", get(history))
        .route("/api/search", get(search))
        .route("/api/episodes", get(episodes))
        .route("/api/source-status", post(source_status))
        .route("/api/folders", get(folders))
        .with_state(state);

    api.fallback(static_handler)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

async fn get_settings(State(state): State<AppState>) -> Json<SettingsResponse> {
    let roots: Vec<String> = state
        .db
        .list_input_roots()
        .into_iter()
        .map(|r| r.path)
        .collect();
    let output_roots = state.output_roots();
    let checks = settings_checks(&roots, &output_roots);
    Json(SettingsResponse {
        input_roots: roots,
        tv_output_root: state.db.get_setting("tv_output_root", ""),
        anime_output_root: state.db.get_setting("anime_output_root", ""),
        film_output_root: state.db.get_setting("film_output_root", ""),
        music_output_root: state.db.get_setting("music_output_root", ""),
        copy_rate_limit_mbps: state.db.get_setting("copy_rate_limit_mbps", "15"),
        checks,
    })
}

async fn save_settings(
    State(state): State<AppState>,
    Json(payload): Json<SettingsPayload>,
) -> Json<Value> {
    let roots: Vec<String> = payload
        .input_roots
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| normalize_path(line))
        .collect();
    state.db.replace_input_roots(&roots);
    state.db.set_setting(
        "tv_output_root",
        &normalize_optional(&payload.tv_output_root),
    );
    state.db.set_setting(
        "anime_output_root",
        &normalize_optional(&payload.anime_output_root),
    );
    state.db.set_setting(
        "film_output_root",
        &normalize_optional(&payload.film_output_root),
    );
    state.db.set_setting(
        "music_output_root",
        &normalize_optional(&payload.music_output_root),
    );
    state.db.set_setting(
        "copy_rate_limit_mbps",
        &normalize_copy_rate_limit(&payload.copy_rate_limit_mbps),
    );
    Json(json!({ "status": "ok" }))
}

fn settings_checks(
    input_roots: &[String],
    output_roots: &HashMap<String, PathBuf>,
) -> Vec<PermissionCheck> {
    let mut checks = Vec::new();
    for root in input_roots {
        let path = PathBuf::from(root);
        checks.push(PermissionCheck {
            label: format!("Input: {}", path.display()),
            exists: path.exists(),
            read: path.is_dir() && os_access(&path, false),
            write: None,
        });
    }
    for &media_type in MEDIA_TYPES {
        if let Some(path) = output_roots.get(media_type) {
            checks.push(PermissionCheck {
                label: format!("{} output: {}", title_word(media_type), path.display()),
                exists: path.exists(),
                read: path.is_dir() && os_access(path, false),
                write: Some(path.is_dir() && os_access(path, true)),
            });
        }
    }
    checks
}

fn title_word(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Browse
// ---------------------------------------------------------------------------

async fn browse(
    State(state): State<AppState>,
    Query(query): Query<BrowseQuery>,
) -> Json<BrowseResponse> {
    let roots = state.db.list_input_roots();
    let active_root = match query.root_id {
        Some(id) => state.db.get_input_root(id),
        None => roots.first().cloned(),
    };
    let root_infos: Vec<BrowseRootInfo> = roots
        .iter()
        .map(|r| BrowseRootInfo {
            id: r.id,
            path: r.path.clone(),
        })
        .collect();
    let active_info = active_root.as_ref().map(|r| BrowseRootInfo {
        id: r.id,
        path: r.path.clone(),
    });

    // The directory listing and per-entry stat/canonicalize calls hit the
    // (often network-mounted) filesystem, so run them on a blocking thread
    // instead of stalling a Tokio worker.
    let (entries, error, parent_path) = match active_root {
        Some(root) => {
            let task_state = state.clone();
            let path = query.path.clone();
            tokio::task::spawn_blocking(move || browse_directory(&task_state, &root, &path))
                .await
                .unwrap_or_else(|e| (Vec::new(), Some(e.to_string()), String::new()))
        }
        None => (Vec::new(), None, String::new()),
    };

    Json(BrowseResponse {
        roots: root_infos,
        active_root: active_info,
        current_path: query.path,
        parent_path,
        entries,
        error,
    })
}

/// List one directory of an input root and compute each entry's import status.
/// Returns `(entries, error, parent_path)`. Synchronous (filesystem + SQLite),
/// so callers run it via `spawn_blocking`.
fn browse_directory(
    state: &AppState,
    root: &crate::db::InputRoot,
    rel_path: &str,
) -> (Vec<BrowseEntry>, Option<String>, String) {
    let root_path = PathBuf::from(&root.path);
    let listed = match list_directory(&root_path, rel_path) {
        Ok(listed) => listed,
        Err(e) => return (Vec::new(), Some(e.to_string()), String::new()),
    };

    // A file is only typed "hardlink" when the filesystem reports extra links
    // AND this app recorded a successful hardlink import of that same inode.
    // nlink > 1 alone proves nothing about the library: seeding tools, *arr
    // imports, and dedup jobs also hardlink sources, and those must stay
    // importable.
    let hardlink_idents = if listed.iter().any(|e| e.has_extra_links) {
        state.db.hardlink_source_idents()
    } else {
        HashSet::new()
    };

    // Files imported from a folder and then moved out of the input tree no
    // longer exist to be listed, but the folder should still reflect that they
    // were imported. Fetch every such record under the *current* directory in
    // one query, then bucket them by the child folder they belong to (one scan
    // instead of one per child).
    let moved_by_child = bucket_moved_imports(state, &root_path, rel_path, &listed);

    // Map each entry to the set of source files it covers.
    let mut entry_sources: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut all_sources: Vec<PathBuf> = Vec::new();
    for entry in &listed {
        let sources = if entry.is_dir {
            let mut present =
                expand_source_files(&root_path, std::slice::from_ref(&entry.relative_path))
                    .unwrap_or_default();
            let present_set: std::collections::HashSet<&PathBuf> = present.iter().collect();
            let moved: Vec<PathBuf> = moved_by_child
                .get(&entry.relative_path)
                .map(|paths| {
                    paths
                        .iter()
                        .filter(|p| !present_set.contains(p))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            drop(present_set);
            present.extend(moved);
            present
        } else {
            vec![canonical_or_normalized(&entry.absolute_path)]
        };
        for s in &sources {
            all_sources.push(s.clone());
        }
        entry_sources.insert(entry.relative_path.clone(), sources);
    }
    let imports = state.db.latest_imports_for_sources(&all_sources);
    let overrides = state.db.source_status_overrides(&all_sources);

    let mut entries = Vec::new();
    for entry in listed {
        let sources = entry_sources
            .get(&entry.relative_path)
            .cloned()
            .unwrap_or_default();
        let status = compute_browse_status(&sources, &imports, &overrides);
        entries.push(BrowseEntry {
            name: entry.name,
            relative_path: entry.relative_path,
            is_dir: entry.is_dir,
            is_video: entry.is_video,
            is_hardlink: entry.has_extra_links
                && entry
                    .dev_inode
                    .is_some_and(|ident| hardlink_idents.contains(&ident)),
            size: entry.size,
            size_human: human_file_size(entry.size),
            status: status.status,
            status_key: status.status_key,
            manual_status: status.manual_status,
            latest_import_result: status.latest_import_result,
            source_count: status.source_count,
        });
    }
    (entries, None, parent_relative(rel_path))
}

struct BrowseStatus {
    status: Option<String>,
    status_key: String,
    manual_status: String,
    latest_import_result: Option<String>,
    source_count: usize,
}

fn source_status_for_path(
    source: &Path,
    imports: &HashMap<String, ImportRow>,
    overrides: &HashMap<String, crate::db::SourceStatusOverride>,
) -> (Option<String>, String, Option<String>) {
    let key = canonical_or_normalized(source)
        .to_string_lossy()
        .to_string();
    let override_status = overrides.get(&key);
    let latest = imports.get(&key);
    let status = if let Some(o) = override_status {
        if o.status == "none" {
            None
        } else {
            Some(o.status.clone())
        }
    } else {
        latest.map(|l| l.result.clone())
    };
    let manual_status = override_status
        .map(|o| o.status.clone())
        .unwrap_or_else(|| "auto".to_string());
    let latest_result = latest.map(|l| l.result.clone());
    (status, manual_status, latest_result)
}

fn compute_browse_status(
    sources: &[PathBuf],
    imports: &HashMap<String, ImportRow>,
    overrides: &HashMap<String, crate::db::SourceStatusOverride>,
) -> BrowseStatus {
    let states: Vec<(Option<String>, String, Option<String>)> = sources
        .iter()
        .map(|s| source_status_for_path(s, imports, overrides))
        .collect();
    let status_keys: std::collections::HashSet<String> = states
        .iter()
        .map(|(status, _, _)| status.clone().unwrap_or_else(|| "none".to_string()))
        .collect();

    let (status, status_key) =
        if states.is_empty() || status_keys == std::iter::once("none".to_string()).collect() {
            (None, "none".to_string())
        } else if status_keys.len() == 1 {
            let key = status_keys.into_iter().next().unwrap();
            if key == "none" {
                (None, "none".to_string())
            } else {
                (Some(key.clone()), key)
            }
        } else {
            (Some("mixed".to_string()), "mixed".to_string())
        };

    let latest_import_result = if states.len() == 1 {
        states[0].2.clone()
    } else {
        None
    };
    let manual_statuses: std::collections::HashSet<String> =
        states.iter().map(|(_, m, _)| m.clone()).collect();
    let manual_status = if states.len() == 1 {
        states[0].1.clone()
    } else if manual_statuses.len() > 1 {
        String::new()
    } else {
        states
            .first()
            .map(|(_, m, _)| m.clone())
            .unwrap_or_default()
    };

    BrowseStatus {
        status,
        status_key,
        manual_status,
        latest_import_result,
        source_count: states.len(),
    }
}

/// Group moved-away import source paths by the listed child folder they belong
/// to. One prefix query over the current directory replaces one query per
/// child. Files moved directly out of the current directory (not from a child
/// folder) are intentionally dropped: they are gone and not listed.
fn bucket_moved_imports(
    state: &AppState,
    root_path: &Path,
    rel_path: &str,
    listed: &[crate::filesystem::BrowserEntry],
) -> HashMap<String, Vec<PathBuf>> {
    let mut buckets: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let current_dir = match resolve_under_root(root_path, rel_path) {
        Ok(dir) => dir,
        Err(_) => return buckets,
    };
    let dir_children: Vec<(String, PathBuf)> = listed
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| {
            (
                e.relative_path.clone(),
                canonical_or_normalized(&e.absolute_path),
            )
        })
        .collect();
    if dir_children.is_empty() {
        return buckets;
    }
    for row in state.db.latest_imports_under_prefix(&current_dir) {
        let path = PathBuf::from(row.source_path);
        if let Some((rel, _)) = dir_children
            .iter()
            .find(|(_, prefix)| path.starts_with(prefix))
        {
            buckets.entry(rel.clone()).or_default().push(path);
        }
    }
    buckets
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

async fn match_route(
    State(state): State<AppState>,
    Json(payload): Json<MatchPayload>,
) -> AppResult<Json<MatchResponse>> {
    let root = state
        .db
        .get_input_root(payload.root_id)
        .ok_or_else(|| AppError::not_found("Input root not found"))?;
    if !is_valid_media_type(&payload.media_type) {
        return Err(AppError::bad_request("Invalid media type"));
    }
    let media_type = payload.media_type.clone();
    let root_path = PathBuf::from(&root.path);
    let selected = payload.selected.clone();

    // Music takes a separate path: audio files are grouped by (artist, album)
    // read from ffprobe tags with a filename/folder fallback.
    if media_type == "music" {
        let expand_root = root_path.clone();
        let expand_selected = selected.clone();
        let groups = tokio::task::spawn_blocking(move || {
            expand_grouped_audio(&expand_root, &expand_selected)
        })
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| AppError::bad_request(e.to_string()))?;
        let out_groups = build_music_groups(groups).await;
        return Ok(Json(MatchResponse {
            media_type: media_type.clone(),
            output_root: state
                .output_root_for(&media_type)
                .map(|p| p.to_string_lossy().to_string()),
            groups: out_groups,
        }));
    }

    let groups = tokio::task::spawn_blocking(move || expand_grouped(&root_path, &selected))
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| AppError::bad_request(e.to_string()))?;

    let mut search_cache: HashMap<String, Vec<ShowCandidate>> = HashMap::new();
    let mut episode_cache: HashMap<String, Vec<EpisodeCandidate>> = HashMap::new();
    let mut out_groups: Vec<MatchGroup> = Vec::new();

    for group in groups {
        if media_type == "film" {
            // Films carry their identity in the filename; one group per file.
            for gfile in group.files {
                let mut parsed = parse_film_filename(&gfile.path);
                parsed.quality = probe_quality_fallback(&gfile.path, &parsed.quality).await;
                let enriched = enrich_film(&state, &parsed, &mut search_cache).await;
                out_groups.push(MatchGroup {
                    group_key: gfile.path.to_string_lossy().to_string(),
                    group_name: parsed.source_name.clone(),
                    show_title: enriched.show_title,
                    show_year: enriched.show_year,
                    provider: enriched.provider,
                    provider_show_id: enriched.provider_show_id,
                    candidates: enriched.candidates,
                    metadata_error: enriched.metadata_error,
                    episodes: vec![MatchEpisode {
                        source_path: gfile.path.to_string_lossy().to_string(),
                        source_name: parsed.source_name.clone(),
                        season_number: 0,
                        episode_number: 0,
                        episode_title: "Film".to_string(),
                        quality: parsed.quality.clone(),
                        parsed,
                    }],
                });
            }
        } else {
            out_groups.push(
                enrich_group(
                    &state,
                    &media_type,
                    group,
                    &mut search_cache,
                    &mut episode_cache,
                )
                .await,
            );
        }
    }

    Ok(Json(MatchResponse {
        media_type: media_type.clone(),
        output_root: state
            .output_root_for(&media_type)
            .map(|p| p.to_string_lossy().to_string()),
        groups: out_groups,
    }))
}

/// Build music match groups from expanded audio file groups. Each track's
/// artist/album/year is read from ffprobe `format` tags when available, falling
/// back to the enclosing `<Artist>/<Album>` folder convention. Tracks are then
/// regrouped by (artist, album) so one MatchGroup maps to one album.
///
/// Field mapping into the existing MatchGroup/MatchEpisode shape:
///   MatchGroup.show_title = Artist, show_year = album Year, group_name = Album.
///   MatchEpisode.season_number = 0, episode_number = 0, episode_title = Album.
async fn build_music_groups(groups: Vec<crate::filesystem::FileGroup>) -> Vec<MatchGroup> {
    // Track whether ffprobe ran at all so we can note when tags were skipped.
    let mut ffprobe_seen = false;
    // Preserve first-appearance order of (artist, album) keys.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut by_album: HashMap<(String, String), MusicAlbumAccum> = HashMap::new();

    for group in groups {
        for gfile in &group.files {
            let (parsed, probed) = resolve_music_track(&gfile.path).await;
            if probed {
                ffprobe_seen = true;
            }
            let key = (parsed.artist.clone(), parsed.album.clone());
            let accum = by_album.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                MusicAlbumAccum {
                    artist: parsed.artist.clone(),
                    album: parsed.album.clone(),
                    year: parsed.year,
                    episodes: Vec::new(),
                }
            });
            // First non-empty year wins for the album folder.
            if accum.year.is_none() {
                accum.year = parsed.year;
            }
            accum.episodes.push(MatchEpisode {
                source_path: gfile.path.to_string_lossy().to_string(),
                source_name: parsed.source_name.clone(),
                season_number: 0,
                episode_number: 0,
                episode_title: parsed.album.clone(),
                quality: String::new(),
                parsed: ParsedMedia {
                    source_name: parsed.source_name.clone(),
                    title: parsed.title.clone(),
                    year: parsed.year,
                    season: 0,
                    episode: 0,
                    episode_title: parsed.album.clone(),
                    quality: String::new(),
                },
            });
        }
    }

    let metadata_error = if ffprobe_seen {
        None
    } else {
        Some("ffprobe is unavailable; using filename/folder tags only.".to_string())
    };

    order
        .into_iter()
        .filter_map(|key| by_album.remove(&key))
        .map(|accum| {
            let group_name = crate::naming::show_folder_name(&accum.album, accum.year);
            MatchGroup {
                group_key: format!("{}\u{0}{}", accum.artist, accum.album),
                group_name,
                show_title: accum.artist,
                show_year: accum.year,
                provider: String::new(),
                provider_show_id: String::new(),
                candidates: Vec::new(),
                metadata_error: metadata_error.clone(),
                episodes: accum.episodes,
            }
        })
        .collect()
}

struct MusicAlbumAccum {
    artist: String,
    album: String,
    year: Option<i64>,
    episodes: Vec<MatchEpisode>,
}

/// Resolve a single audio track's artist/album/year/title. Prefers ffprobe
/// `format` tags, then falls back to the `<Artist>/<Album>` folder convention
/// and the filename. Returns the parsed values and whether ffprobe produced any
/// usable tag (used to surface a "ffprobe missing" note).
async fn resolve_music_track(path: &Path) -> (crate::parser::ParsedMusic, bool) {
    let probe_path = path.to_path_buf();
    let tags = tokio::task::spawn_blocking(move || crate::ffprobe::probe_music_tags(&probe_path))
        .await
        .unwrap_or_default();
    let probed = tags.artist.is_some() || tags.album.is_some() || tags.year.is_some();

    // Folder fallback: parent dir is the album, grandparent is the artist.
    let folder_album = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    let folder_artist = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());

    let mut parsed =
        crate::parser::parse_music_file(path, folder_artist.as_deref(), folder_album.as_deref());
    // ffprobe tags take precedence over the folder/filename fallback.
    if let Some(artist) = tags.artist {
        parsed.artist = artist;
    }
    if let Some(album) = tags.album {
        parsed.album = album.clone();
    }
    if let Some(year) = tags.year {
        parsed.year = Some(year);
    }
    (parsed, probed)
}

/// Resolve quality via ffprobe for a single file when the filename lacked a token.
async fn probe_quality_fallback(path: &Path, current: &str) -> String {
    if current != "Unknown" {
        return current.to_string();
    }
    let probe_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || crate::ffprobe::probe_quality(&probe_path)).await {
        Ok(Some(quality)) => quality,
        _ => current.to_string(),
    }
}

#[derive(Clone)]
struct Enriched {
    show_title: String,
    show_year: Option<i64>,
    provider: String,
    provider_show_id: String,
    candidates: Vec<ShowCandidate>,
    metadata_error: Option<String>,
}

/// Match a whole folder to one show: one provider search + one episode fetch,
/// then map every file in the group to a season/episode/title.
async fn enrich_group(
    state: &AppState,
    media_type: &str,
    group: crate::filesystem::FileGroup,
    search_cache: &mut HashMap<String, Vec<ShowCandidate>>,
    episode_cache: &mut HashMap<String, Vec<EpisodeCandidate>>,
) -> MatchGroup {
    let (folder_title, folder_year) = crate::parser::show_title_from_folder(&group.group_name);

    // One search for the whole folder.
    let search_result = match search_cache.get(&folder_title) {
        Some(cached) => Ok(cached.clone()),
        None => match state.providers.search(media_type, &folder_title).await {
            Ok(found) => {
                search_cache.insert(folder_title.clone(), found.clone());
                Ok(found)
            }
            Err(err) => Err(err.user_message()),
        },
    };

    let (candidates, metadata_error) = match search_result {
        Ok(found) => (found, None),
        Err(message) => (Vec::new(), Some(message)),
    };
    // When the folder name carried a year, prefer the candidate that matches it
    // (e.g. a remake vs. the original) instead of blindly taking the first hit.
    let selected = candidates
        .iter()
        .find(|c| folder_year.is_some() && c.year == folder_year)
        .cloned()
        .or_else(|| candidates.first().cloned());

    // One episode fetch for the selected show.
    let episodes = match &selected {
        Some(candidate) => {
            let key = format!("{}:{}", media_type, candidate.provider_id);
            match episode_cache.get(&key) {
                Some(cached) => cached.clone(),
                None => match state
                    .providers
                    .episodes(media_type, &candidate.provider_id)
                    .await
                {
                    Ok(found) => {
                        episode_cache.insert(key, found.clone());
                        found
                    }
                    Err(_) => Vec::new(),
                },
            }
        }
        None => Vec::new(),
    };

    let show_title = selected
        .as_ref()
        .map(|c| c.title.clone())
        .unwrap_or_else(|| folder_title.clone());
    let show_year = selected.as_ref().and_then(|c| c.year).or(folder_year);

    let mut out_episodes = Vec::new();
    for gfile in &group.files {
        let segments: Vec<&str> = gfile.relative_segments.iter().map(|s| s.as_str()).collect();
        let season_hint = crate::parser::season_from_segments(&segments);
        let mut parsed = crate::parser::parse_folder_episode(&gfile.path, &show_title, season_hint);
        parsed.quality = probe_quality_fallback(&gfile.path, &parsed.quality).await;

        let episode_title = episodes
            .iter()
            .find(|e| e.season == parsed.season && e.episode == parsed.episode)
            .or_else(|| {
                if media_type == "anime" {
                    episodes.iter().find(|e| e.episode == parsed.episode)
                } else {
                    None
                }
            })
            .map(|e| e.title.clone())
            .unwrap_or_else(|| parsed.episode_title.clone());

        out_episodes.push(MatchEpisode {
            source_path: gfile.path.to_string_lossy().to_string(),
            source_name: parsed.source_name.clone(),
            season_number: parsed.season,
            episode_number: parsed.episode,
            episode_title,
            quality: parsed.quality.clone(),
            parsed,
        });
    }

    MatchGroup {
        group_key: group.group_key,
        group_name: group.group_name,
        show_title,
        show_year,
        provider: selected
            .as_ref()
            .map(|c| c.provider.clone())
            .unwrap_or_default(),
        provider_show_id: selected.map(|c| c.provider_id).unwrap_or_default(),
        candidates,
        metadata_error,
        episodes: out_episodes,
    }
}

async fn enrich_film(
    state: &AppState,
    parsed: &ParsedMedia,
    search_cache: &mut HashMap<String, Vec<ShowCandidate>>,
) -> Enriched {
    let fallback = Enriched {
        show_title: parsed.title.clone(),
        show_year: parsed.year,
        provider: String::new(),
        provider_show_id: String::new(),
        candidates: Vec::new(),
        metadata_error: None,
    };

    let candidates = match search_cache.get(&parsed.title) {
        Some(cached) => cached.clone(),
        None => match state.providers.search("film", &parsed.title).await {
            Ok(found) => {
                search_cache.insert(parsed.title.clone(), found.clone());
                found
            }
            Err(err) => {
                return Enriched {
                    metadata_error: Some(err.user_message()),
                    ..fallback
                };
            }
        },
    };

    if candidates.is_empty() {
        return fallback;
    }
    let selected = candidates
        .iter()
        .find(|c| parsed.year.is_some() && c.year == parsed.year)
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    Enriched {
        show_title: selected.title,
        show_year: selected.year.or(parsed.year),
        provider: selected.provider,
        provider_show_id: selected.provider_id,
        candidates,
        metadata_error: None,
    }
}

// ---------------------------------------------------------------------------
// Preview & imports
// ---------------------------------------------------------------------------

async fn preview(
    State(state): State<AppState>,
    Json(batch): Json<ImportBatch>,
) -> AppResult<Json<Value>> {
    let requests = build_import_requests(&state, &batch)?;
    let mut results = Vec::new();
    for request in requests {
        let result = preview_import(request);
        results.push(json!({
            "source_path": result.source_path,
            "final_path": result.final_path,
            "result": result.result,
            "error": result.error,
        }));
    }
    Ok(Json(json!({ "results": results })))
}

async fn start_import_job(
    State(state): State<AppState>,
    Json(batch): Json<ImportBatch>,
) -> AppResult<Json<Value>> {
    let requests = build_import_requests(&state, &batch)?;
    let rate = state.copy_rate_limit_mbps();
    let job = state.jobs.start(requests, state.db.clone(), rate);
    Ok(Json(snapshot_json(&job)))
}

fn snapshot_json(job: &crate::jobs::Job) -> Value {
    serde_json::to_value(job.snapshot()).unwrap_or_else(|_| json!({}))
}

async fn list_import_jobs(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let jobs: Vec<Value> = state
        .jobs
        .list()
        .into_iter()
        .map(|job| snapshot_json(&job))
        .collect();
    Ok(Json(json!({ "jobs": jobs })))
}

async fn clear_import_jobs(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let cleared = state.jobs.clear_completed();
    let jobs: Vec<Value> = state
        .jobs
        .list()
        .into_iter()
        .map(|job| snapshot_json(&job))
        .collect();
    Ok(Json(json!({ "cleared": cleared, "jobs": jobs })))
}

async fn get_import_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let job = state
        .jobs
        .get(&id)
        .ok_or_else(|| AppError::not_found("Import job not found"))?;
    Ok(Json(snapshot_json(&job)))
}

async fn cancel_import_item(
    State(state): State<AppState>,
    AxumPath((id, index)): AxumPath<(String, usize)>,
) -> AppResult<Json<Value>> {
    let job = state
        .jobs
        .get(&id)
        .ok_or_else(|| AppError::not_found("Import job not found"))?;
    job.request_cancel_item(index);
    Ok(Json(snapshot_json(&job)))
}

async fn cancel_import_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let job = state
        .jobs
        .get(&id)
        .ok_or_else(|| AppError::not_found("Import job not found"))?;
    job.request_cancel();
    Ok(Json(snapshot_json(&job)))
}

async fn import_job_results(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> AppResult<Json<Value>> {
    let job = state
        .jobs
        .get(&id)
        .ok_or_else(|| AppError::not_found("Import job not found"))?;
    if !job.is_finished() {
        return Err(AppError::conflict("Import job is not done"));
    }
    let results: Vec<Value> = job
        .results()
        .into_iter()
        .map(|r| {
            json!({
                "source_path": r.source_path,
                "final_path": r.final_path,
                "result": r.result,
                "error": r.error,
            })
        })
        .collect();
    Ok(Json(json!({ "results": results })))
}

fn build_import_requests(state: &AppState, batch: &ImportBatch) -> AppResult<Vec<ImportRequest>> {
    if !is_valid_media_type(&batch.media_type) {
        return Err(AppError::bad_request("Invalid media type"));
    }
    if !is_valid_action(&batch.action) {
        return Err(AppError::bad_request("Invalid action"));
    }
    if !is_valid_conflict_policy(&batch.conflict_policy) {
        return Err(AppError::bad_request("Invalid conflict policy"));
    }
    let output_root = state.output_root_for(&batch.media_type).ok_or_else(|| {
        AppError::bad_request(format!("No {} output root configured", batch.media_type))
    })?;

    let input_roots: Vec<PathBuf> = state
        .db
        .list_input_roots()
        .into_iter()
        .map(|r| PathBuf::from(r.path))
        .collect();
    let mut requests = Vec::new();
    for item in &batch.items {
        let source = canonical_or_normalized(&PathBuf::from(&item.source_path));
        assert_source_allowed(&source, &input_roots)?;
        let origin = origin_relative_path(&source, &input_roots);
        requests.push(ImportRequest {
            source_path: source,
            output_root: output_root.clone(),
            media_type: batch.media_type.clone(),
            show_title: item.show_title.clone(),
            show_year: item.show_year,
            season_number: item.season_number,
            episode_number: item.episode_number,
            episode_title: item.episode_title.clone(),
            quality: item.quality.clone(),
            action: batch.action.clone(),
            conflict_policy: batch.conflict_policy.clone(),
            provider: item.provider.clone().filter(|s| !s.is_empty()),
            provider_show_id: item.provider_show_id.clone().filter(|s| !s.is_empty()),
            origin,
        });
    }
    Ok(requests)
}

/// Derive the source's original path relative to whichever input root contains
/// it (folder structure preserved), e.g. "Spiderman/Saison 01/episode-01.mkv".
/// Falls back to just the file name when no root matches or stripping yields an
/// empty path.
fn origin_relative_path(source: &Path, input_roots: &[PathBuf]) -> String {
    let file_name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    for root in input_roots {
        let root = canonical_or_normalized(root);
        if let Ok(rel) = source.strip_prefix(&root) {
            let rel = rel.to_string_lossy().to_string();
            if !rel.is_empty() {
                return rel;
            }
        }
    }
    file_name
}

fn assert_source_allowed(source: &Path, input_roots: &[PathBuf]) -> AppResult<()> {
    if input_roots.iter().any(|root| is_relative_to(source, root)) {
        Ok(())
    } else {
        Err(AppError::bad_request(format!(
            "Source is outside configured input roots: {}",
            source.display()
        )))
    }
}

// ---------------------------------------------------------------------------
// Library & history
// ---------------------------------------------------------------------------

async fn library(State(state): State<AppState>) -> Json<Value> {
    let files: Vec<Value> = state
        .db
        .list_library_files()
        .into_iter()
        .map(|f| {
            json!({
                "media_type": f.media_type,
                "output_path": f.output_path,
                "present": f.present,
                "size": f.size,
                "size_human": human_file_size(f.size),
            })
        })
        .collect();
    let roots: HashMap<String, String> = state
        .output_roots()
        .into_iter()
        .map(|(k, v)| (k, v.to_string_lossy().to_string()))
        .collect();
    Json(json!({ "files": files, "roots": roots }))
}

async fn rescan_library(State(state): State<AppState>) -> Json<Value> {
    let roots = state.output_roots();
    let counts = tokio::task::spawn_blocking(move || {
        let db = state.db.clone();
        crate::library::rescan_outputs(&db, &roots)
    })
    .await
    .unwrap_or_default();
    Json(json!({ "counts": counts }))
}

async fn history(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "imports": state.db.list_imports(100) }))
}

// ---------------------------------------------------------------------------
// Search / episodes
// ---------------------------------------------------------------------------

async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Value>> {
    if !is_valid_media_type(&query.media_type) {
        return Err(AppError::bad_request("Invalid media type"));
    }
    let q = query.q.trim();
    if q.is_empty() {
        return Ok(Json(json!({ "results": [] })));
    }
    let results = state
        .providers
        .search(&query.media_type, q)
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, e.user_message()))?;
    Ok(Json(json!({ "results": results })))
}

async fn episodes(
    State(state): State<AppState>,
    Query(query): Query<EpisodesQuery>,
) -> AppResult<Json<Value>> {
    if !is_valid_media_type(&query.media_type) {
        return Err(AppError::bad_request("Invalid media type"));
    }
    let results = state
        .providers
        .episodes(&query.media_type, &query.provider_show_id)
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, e.user_message()))?;
    Ok(Json(json!({ "results": results })))
}

// ---------------------------------------------------------------------------
// Source status
// ---------------------------------------------------------------------------

async fn source_status(
    State(state): State<AppState>,
    Json(payload): Json<SourceStatusPayload>,
) -> AppResult<Json<Value>> {
    let sources = status_update_sources(&state, &payload)?;
    if sources.is_empty() {
        return Err(AppError::bad_request("No files selected"));
    }
    if payload.status == "auto" {
        state.db.set_source_status_overrides(&sources, None);
        return Ok(Json(json!({ "status": "auto", "updated": sources.len() })));
    }
    if !crate::state::SOURCE_STATUSES.contains(&payload.status.as_str()) {
        return Err(AppError::bad_request("Invalid status"));
    }
    state
        .db
        .set_source_status_overrides(&sources, Some(&payload.status));
    Ok(Json(
        json!({ "status": payload.status, "updated": sources.len() }),
    ))
}

fn status_update_sources(
    state: &AppState,
    payload: &SourceStatusPayload,
) -> AppResult<Vec<PathBuf>> {
    let input_roots: Vec<PathBuf> = state
        .db
        .list_input_roots()
        .into_iter()
        .map(|r| PathBuf::from(r.path))
        .collect();
    if let Some(source_path) = &payload.source_path {
        let source = canonical_or_normalized(&PathBuf::from(source_path));
        assert_source_allowed(&source, &input_roots)?;
        return Ok(vec![source]);
    }
    let root_id = payload
        .root_id
        .ok_or_else(|| AppError::bad_request("Input root is required"))?;
    let root = state
        .db
        .get_input_root(root_id)
        .ok_or_else(|| AppError::not_found("Input root not found"))?;
    expand_source_files(&PathBuf::from(&root.path), &payload.selected)
        .map_err(|e| AppError::bad_request(e.to_string()))
}

// ---------------------------------------------------------------------------
// Folder picker
// ---------------------------------------------------------------------------

async fn folders(Query(query): Query<FoldersQuery>) -> AppResult<Json<FoldersResponse>> {
    // resolve_picker_path + read_dir touch the filesystem; keep them off the
    // async runtime.
    tokio::task::spawn_blocking(move || folders_blocking(&query.path))
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
}

fn folders_blocking(path: &str) -> AppResult<FoldersResponse> {
    let current = resolve_picker_path(path)?;
    let mut children: Vec<PathBuf> = std::fs::read_dir(&current)
        .map_err(|e| AppError::bad_request(e.to_string()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    children.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });

    let folders: Vec<FolderEntry> = children
        .into_iter()
        .filter(|p| std::fs::metadata(p).is_ok())
        .map(|p| FolderEntry {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: p.to_string_lossy().to_string(),
        })
        .collect();

    let parent = current
        .parent()
        .filter(|p| *p != current)
        .map(|p| p.to_string_lossy().to_string());
    let roots: Vec<String> = PICKER_ROOTS
        .iter()
        .filter(|root| Path::new(root).exists())
        .map(|root| root.to_string())
        .collect();

    Ok(FoldersResponse {
        path: current.to_string_lossy().to_string(),
        parent,
        folders,
        roots,
    })
}

fn resolve_picker_path(value: &str) -> AppResult<PathBuf> {
    let path = canonical_or_normalized(&expand_user(if value.is_empty() { "/" } else { value }));
    if !path.exists() {
        return Err(AppError::not_found(format!(
            "Folder does not exist: {}",
            path.display()
        )));
    }
    if !path.is_dir() {
        return Err(AppError::bad_request(format!(
            "Path is not a folder: {}",
            path.display()
        )));
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn expand_user(value: &str) -> PathBuf {
    let trimmed = value.trim();
    if let Some(rest) = trimmed.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                path.push(rest);
            }
            return path;
        }
    }
    PathBuf::from(trimmed)
}

fn normalize_path(value: &str) -> String {
    canonical_or_normalized(&expand_user(value))
        .to_string_lossy()
        .to_string()
}

fn normalize_optional(value: &str) -> String {
    if value.trim().is_empty() {
        String::new()
    } else {
        normalize_path(value)
    }
}

fn normalize_copy_rate_limit(value: &str) -> String {
    let cleaned = value.replace(',', ".");
    let cleaned = cleaned.trim();
    let mut limit: f64 = cleaned.parse().unwrap_or(15.0);
    limit = limit.clamp(0.0, 1000.0);
    if limit.fract() == 0.0 {
        format!("{}", limit as i64)
    } else {
        let formatted = format!("{limit:.2}");
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn parent_relative(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    match Path::new(path).parent() {
        Some(parent) => {
            let s = parent.to_string_lossy().to_string();
            if s == "." {
                String::new()
            } else {
                s
            }
        }
        None => String::new(),
    }
}

#[cfg(unix)]
fn os_access(path: &Path, write: bool) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mode = if write { libc::W_OK } else { libc::R_OK };
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

#[cfg(not(unix))]
fn os_access(_path: &Path, _write: bool) -> bool {
    false
}
