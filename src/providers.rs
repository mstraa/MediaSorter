use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::db::Database;

const USER_AGENT: &str = "TvSorter/0.1 (+https://github.com/mstraa/TvSorterV2)";

static HTML_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
static YEAR_IN_TEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b((?:19|20)\d{2})\b").unwrap());

#[derive(Clone, Debug, Serialize)]
pub struct ShowCandidate {
    pub provider: String,
    pub provider_id: String,
    pub title: String,
    pub year: Option<i64>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EpisodeCandidate {
    pub provider: String,
    pub provider_show_id: String,
    pub season: i64,
    pub episode: i64,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Http(String),
    #[error("provider returned status {0}")]
    Status(u16),
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Human-friendly fallback message shown in the match queue.
    pub fn user_message(&self) -> String {
        match self {
            ProviderError::Status(429) => {
                "Metadata provider is rate-limiting requests. Filename parsing was used for this item."
                    .to_string()
            }
            ProviderError::Status(401) | ProviderError::Status(403) => {
                "Metadata provider refused the request. Filename parsing was used for this item."
                    .to_string()
            }
            other => other.to_string(),
        }
    }
}

/// Longest we will honour a `Retry-After`. Past this the request fails fast
/// rather than holding a connection open on an upstream's say-so.
const MAX_BACKOFF_SECS: f64 = 30.0;

/// Turn an untrusted seconds value into a sane `Duration`.
///
/// `Duration::from_secs_f64` panics on negative, NaN, infinite, or overflowing
/// input, so this never hands it one.
fn clamp_backoff(secs: f64) -> Duration {
    if !secs.is_finite() || secs <= 0.0 {
        return Duration::from_secs(1);
    }
    Duration::from_secs_f64(secs.min(MAX_BACKOFF_SECS))
}

#[derive(Clone)]
pub struct MetadataProviders {
    db: Database,
    client: reqwest::Client,
    last_jikan: Arc<Mutex<Option<Instant>>>,
}

impl MetadataProviders {
    pub fn new(db: Database) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            db,
            client,
            last_jikan: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn search(
        &self,
        media_type: &str,
        query: &str,
    ) -> Result<Vec<ShowCandidate>, ProviderError> {
        match media_type {
            "tv" => self.search_tvmaze(query).await,
            "anime" => self.search_jikan(query).await,
            "film" => self.search_films(query).await,
            other => Err(ProviderError::Other(format!(
                "Unsupported media type: {other}"
            ))),
        }
    }

    pub async fn episodes(
        &self,
        media_type: &str,
        provider_show_id: &str,
    ) -> Result<Vec<EpisodeCandidate>, ProviderError> {
        match media_type {
            "tv" => self.tvmaze_episodes(provider_show_id).await,
            "anime" => self.jikan_episodes(provider_show_id).await,
            "film" => Ok(Vec::new()),
            other => Err(ProviderError::Other(format!(
                "Unsupported media type: {other}"
            ))),
        }
    }

    async fn cached_or_fetch(&self, key: &str, url: &str) -> Result<Value, ProviderError> {
        if let Some(cached) = self.db.get_cache(key) {
            return Ok(cached);
        }
        let value = self.get_json(url).await?;
        self.db.set_cache(key, &value);
        Ok(value)
    }

    async fn get_json(&self, url: &str) -> Result<Value, ProviderError> {
        for attempt in 0..3 {
            let response = self
                .client
                .get(url)
                .header("Accept", "application/json")
                .header("Api-User-Agent", USER_AGENT)
                .send()
                .await
                .map_err(|e| ProviderError::Http(e.to_string()))?;
            let status = response.status();
            if status.as_u16() != 429 {
                if !status.is_success() {
                    return Err(ProviderError::Status(status.as_u16()));
                }
                return response
                    .json::<Value>()
                    .await
                    .map_err(|e| ProviderError::Http(e.to_string()));
            }
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(2.0 * (attempt as f64 + 1.0));
            // `Duration::from_secs_f64` panics on a negative, NaN, infinite, or
            // overflowing value, and this number comes straight off a remote
            // response header. Clamp instead of trusting it — an upstream that
            // asks us to wait an hour should not stall the request either.
            tokio::time::sleep(clamp_backoff(retry_after)).await;
        }
        Err(ProviderError::Status(429))
    }

    // ---- TVMaze ----

    async fn search_tvmaze(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        let key = format!("tvmaze:search:{}", query.to_lowercase());
        let url = format!(
            "https://api.tvmaze.com/search/shows?q={}",
            urlencoding::encode(query)
        );
        let data = self.cached_or_fetch(&key, &url).await?;
        let mut candidates = Vec::new();
        if let Some(items) = data.as_array() {
            for item in items.iter().take(10) {
                let show = &item["show"];
                let title = show["name"].as_str().unwrap_or("Unknown").to_string();
                let year = year_from_date(show["premiered"].as_str());
                let summary = strip_html(show["summary"].as_str().unwrap_or(""));
                candidates.push(ShowCandidate {
                    provider: "tvmaze".to_string(),
                    provider_id: value_to_id(&show["id"]),
                    title,
                    year,
                    summary: truncate(&summary, 240),
                });
            }
        }
        Ok(candidates)
    }

    async fn tvmaze_episodes(&self, show_id: &str) -> Result<Vec<EpisodeCandidate>, ProviderError> {
        let key = format!("tvmaze:episodes:{show_id}");
        let url = format!(
            "https://api.tvmaze.com/shows/{}/episodes",
            urlencoding::encode(show_id)
        );
        let data = self.cached_or_fetch(&key, &url).await?;
        let mut episodes = Vec::new();
        if let Some(items) = data.as_array() {
            for item in items {
                if item["number"].is_null() {
                    continue;
                }
                episodes.push(EpisodeCandidate {
                    provider: "tvmaze".to_string(),
                    provider_show_id: show_id.to_string(),
                    season: item["season"].as_i64().unwrap_or(1),
                    episode: item["number"].as_i64().unwrap_or(1),
                    title: item["name"].as_str().unwrap_or("Episode").to_string(),
                });
            }
        }
        Ok(episodes)
    }

    // ---- Jikan (anime) ----

    async fn throttle_jikan(&self) {
        let mut last = self.last_jikan.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed().as_secs_f64();
            let wait = 1.1 - elapsed;
            if wait > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
        }
        *last = Some(Instant::now());
    }

    async fn search_jikan(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        let key = format!("jikan:search:{}", query.to_lowercase());
        if self.db.get_cache(&key).is_none() {
            self.throttle_jikan().await;
        }
        let url = format!(
            "https://api.jikan.moe/v4/anime?q={}&limit=10",
            urlencoding::encode(query)
        );
        let data = self.cached_or_fetch(&key, &url).await?;
        let mut candidates = Vec::new();
        if let Some(items) = data["data"].as_array() {
            for item in items.iter().take(10) {
                let title = item["title_english"]
                    .as_str()
                    .or_else(|| item["title"].as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let year = item["year"]
                    .as_i64()
                    .or_else(|| year_from_date(item["aired"]["from"].as_str()));
                let summary = item["synopsis"].as_str().unwrap_or("");
                candidates.push(ShowCandidate {
                    provider: "jikan".to_string(),
                    provider_id: value_to_id(&item["mal_id"]),
                    title,
                    year,
                    summary: truncate(summary, 240),
                });
            }
        }
        Ok(candidates)
    }

    async fn jikan_episodes(&self, show_id: &str) -> Result<Vec<EpisodeCandidate>, ProviderError> {
        let key = format!("jikan:episodes:{show_id}");
        if self.db.get_cache(&key).is_none() {
            self.throttle_jikan().await;
        }
        let url = format!(
            "https://api.jikan.moe/v4/anime/{}/episodes",
            urlencoding::encode(show_id)
        );
        let data = self.cached_or_fetch(&key, &url).await?;
        let mut episodes = Vec::new();
        if let Some(items) = data["data"].as_array() {
            // Jikan returns one MAL entry's episodes (one "season"). Use the
            // returned episode number, falling back to sequential ordering.
            for (index, item) in items.iter().enumerate() {
                let number = item["mal_id"].as_i64().unwrap_or(index as i64 + 1);
                episodes.push(EpisodeCandidate {
                    provider: "jikan".to_string(),
                    provider_show_id: show_id.to_string(),
                    season: 1,
                    episode: number,
                    title: item["title"].as_str().unwrap_or("Episode").to_string(),
                });
            }
        }
        Ok(episodes)
    }

    // ---- Film ----

    async fn search_films(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        let mut first_error: Option<ProviderError> = None;
        match self.search_imdb_suggestions(query).await {
            Ok(candidates) if !candidates.is_empty() => return Ok(candidates),
            Ok(_) => {}
            Err(err) => first_error = Some(err),
        }
        match self.search_wikidata_films(query).await {
            Ok(candidates) if !candidates.is_empty() => return Ok(candidates),
            Ok(_) => {}
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(Vec::new()),
        }
    }

    async fn search_imdb_suggestions(
        &self,
        query: &str,
    ) -> Result<Vec<ShowCandidate>, ProviderError> {
        let key = format!("imdb:suggest:film:{}", query.to_lowercase());
        let first_char = first_query_letter(query);
        let url = format!(
            "https://v3.sg.media-imdb.com/suggestion/{}/{}.json",
            first_char,
            urlencoding::encode(&query.to_lowercase())
        );
        let data = self.cached_or_fetch(&key, &url).await?;
        let mut candidates = Vec::new();
        if let Some(items) = data["d"].as_array() {
            for item in items {
                let qid = item["qid"].as_str().unwrap_or("");
                if qid != "movie" && qid != "tvMovie" {
                    continue;
                }
                let title = match item["l"].as_str() {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => continue,
                };
                let provider_id = match item["id"].as_str() {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => continue,
                };
                let mut summary_parts = Vec::new();
                if let Some(q) = item["q"].as_str() {
                    summary_parts.push(q.to_string());
                }
                if let Some(s) = item["s"].as_str() {
                    summary_parts.push(s.to_string());
                }
                candidates.push(ShowCandidate {
                    provider: "imdb".to_string(),
                    provider_id,
                    title,
                    year: optional_year(&item["y"]),
                    summary: truncate(&summary_parts.join(" - "), 240),
                });
                if candidates.len() >= 10 {
                    break;
                }
            }
        }
        Ok(candidates)
    }

    async fn search_wikidata_films(
        &self,
        query: &str,
    ) -> Result<Vec<ShowCandidate>, ProviderError> {
        let key = format!("wikidata:film:search:{}", query.to_lowercase());
        let cached = if let Some(cached) = self.db.get_cache(&key) {
            cached
        } else {
            let search_url = format!(
                "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&type=item&limit=10&format=json",
                urlencoding::encode(query)
            );
            let search_payload = self.get_json(&search_url).await?;
            let ids: Vec<String> = search_payload["search"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item["id"].as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let entities = if ids.is_empty() {
                serde_json::json!({})
            } else {
                let ids_param = urlencoding::encode(&ids.join("|")).into_owned();
                let entities_url = format!(
                    "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={ids_param}&props=labels|descriptions|claims&languages=en&format=json"
                );
                let payload = self.get_json(&entities_url).await?;
                payload["entities"].clone()
            };
            let combined = serde_json::json!({
                "search": search_payload["search"].clone(),
                "entities": entities,
            });
            self.db.set_cache(&key, &combined);
            combined
        };

        let mut candidates = Vec::new();
        if let Some(items) = cached["search"].as_array() {
            for item in items {
                let entity_id = item["id"].as_str().unwrap_or("");
                let entity = &cached["entities"][entity_id];
                let label = entity["labels"]["en"]["value"]
                    .as_str()
                    .or_else(|| item["label"].as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let description = entity["descriptions"]["en"]["value"]
                    .as_str()
                    .or_else(|| item["description"].as_str())
                    .unwrap_or("")
                    .to_string();
                if !looks_like_film(entity, &description) {
                    continue;
                }
                candidates.push(ShowCandidate {
                    provider: "wikidata".to_string(),
                    provider_id: entity_id.to_string(),
                    title: label,
                    year: wikidata_release_year(entity)
                        .or_else(|| year_from_description(&description)),
                    summary: truncate(&description, 240),
                });
                if candidates.len() >= 10 {
                    break;
                }
            }
        }
        Ok(candidates)
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn value_to_id(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn year_from_date(value: Option<&str>) -> Option<i64> {
    // `get(..4)` returns None on a short string or a non-char-boundary split,
    // so this never panics on multibyte provider data.
    value?.get(..4)?.parse().ok()
}

fn strip_html(value: &str) -> String {
    HTML_TAG_RE.replace_all(value, "").trim().to_string()
}

fn first_query_letter(query: &str) -> String {
    query
        .to_lowercase()
        .chars()
        .find(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "x".to_string())
}

fn optional_year(value: &Value) -> Option<i64> {
    let year = match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }?;
    if (1800..=2100).contains(&year) {
        Some(year)
    } else {
        None
    }
}

fn looks_like_film(entity: &Value, description: &str) -> bool {
    let lower = description.to_lowercase();
    if lower.contains("film") || lower.contains("movie") {
        return true;
    }
    if let Some(claims) = entity["claims"]["P31"].as_array() {
        for claim in claims {
            if let Some(id) = claim["mainsnak"]["datavalue"]["value"]["id"].as_str() {
                if matches!(id, "Q11424" | "Q24862" | "Q506240") {
                    return true;
                }
            }
        }
    }
    false
}

fn wikidata_release_year(entity: &Value) -> Option<i64> {
    let claims = entity["claims"]["P577"].as_array()?;
    for claim in claims {
        if let Some(time) = claim["mainsnak"]["datavalue"]["value"]["time"].as_str() {
            if let Some(year) = year_from_date(Some(time.trim_start_matches('+'))) {
                return Some(year);
            }
        }
    }
    None
}

fn year_from_description(description: &str) -> Option<i64> {
    YEAR_IN_TEXT_RE
        .captures(description)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}
