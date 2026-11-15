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
            ProviderError::Status(code) if (500..600).contains(code) => {
                "Metadata provider is temporarily unavailable. Filename parsing was used for this item."
                    .to_string()
            }
            other => other.to_string(),
        }
    }
}

/// Statuses worth another attempt: the upstream is busy or its own backend
/// timed out, neither of which says anything about the request itself.
/// Jikan in particular answers 504 whenever MyAnimeList is unreachable.
fn is_retriable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
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

/// Describe a reqwest failure by its root cause instead of its outer layer.
///
/// reqwest's `Display` stops at "error sending request for url (...)", which
/// hides whether it was a timeout, a DNS failure, a TLS error or a reset
/// connection, and leaks the full query URL into the match queue. Report the
/// host and the innermost causes instead.
fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let host = err
        .url()
        .and_then(|u| u.host_str())
        .unwrap_or("metadata provider")
        .to_string();
    if err.is_timeout() {
        return format!("{host} did not answer in time (timed out)");
    }
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !causes.contains(&text) {
            causes.push(text);
        }
        source = cause.source();
    }
    let what = if err.is_connect() {
        "could not connect to"
    } else if err.is_decode() {
        "unreadable response from"
    } else {
        "request failed for"
    };
    if causes.is_empty() {
        format!("{what} {host}")
    } else {
        format!("{what} {host}: {}", causes.join(": "))
    }
}

#[derive(Clone)]
pub struct MetadataProviders {
    db: Database,
    client: reqwest::Client,
    last_jikan: Arc<Mutex<Option<Instant>>>,
    /// Set when Jikan timed out or failed outright: until then, anime lookups
    /// skip it and go straight to AniList instead of waiting on it again for
    /// every folder in the batch.
    jikan_down_until: Arc<Mutex<Option<Instant>>>,
}

/// How long to stop asking Jikan after it failed to answer.
const JIKAN_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Per-request cap for Jikan. It is only the preferred anime source, not the
/// only one, so it should fail fast and let AniList answer.
const JIKAN_TIMEOUT: Duration = Duration::from_secs(10);

const ANILIST_SEARCH_QUERY: &str = "query ($search: String) { Page(perPage: 10) { media(search: $search, type: ANIME, sort: SEARCH_MATCH) { id title { romaji english } seasonYear startDate { year } description(asHtml: false) } } }";

impl MetadataProviders {
    pub fn new(db: Database) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Jikan's cold searches can take longer than 15 s while it queries
            // MyAnimeList; give it room before giving up.
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            db,
            client,
            last_jikan: Arc::new(Mutex::new(None)),
            jikan_down_until: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn search(
        &self,
        media_type: &str,
        query: &str,
    ) -> Result<Vec<ShowCandidate>, ProviderError> {
        match media_type {
            "tv" => self.search_tvmaze(query).await,
            "anime" => self.search_anime(query).await,
            "film" => self.search_films(query).await,
            other => Err(ProviderError::Other(format!(
                "Unsupported media type: {other}"
            ))),
        }
    }

    /// Fetch episodes from the provider the show was actually matched against.
    ///
    /// An anime folder can be matched on TVMaze when Jikan is down, so the
    /// media type alone is not enough to pick the right episode endpoint.
    pub async fn episodes_for(
        &self,
        provider: &str,
        media_type: &str,
        provider_show_id: &str,
    ) -> Result<Vec<EpisodeCandidate>, ProviderError> {
        match provider {
            "tvmaze" => self.tvmaze_episodes(provider_show_id).await,
            "jikan" => self.jikan_episodes(provider_show_id).await,
            // AniList has no per-episode titles; filename parsing fills them.
            "anilist" | "imdb" | "wikidata" => Ok(Vec::new()),
            _ => self.episodes(media_type, provider_show_id).await,
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

    /// Send the request built by `build` (rebuilt for each attempt) and parse
    /// a JSON answer, retrying busy upstreams and one dropped connection.
    async fn get_json(&self, url: &str) -> Result<Value, ProviderError> {
        self.send_json(|| self.client.get(url), true).await
    }

    async fn send_json<F>(&self, build: F, retry_dropped: bool) -> Result<Value, ProviderError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_status = 429u16;
        for attempt in 0..3 {
            let response = match build()
                .header("Accept", "application/json")
                .header("Api-User-Agent", USER_AGENT)
                .send()
                .await
            {
                Ok(response) => response,
                // Jikan regularly drops or stalls a connection under load
                // (it proxies MyAnimeList live). A timeout or failed connect
                // says nothing about the request, so retry it once.
                Err(err)
                    if retry_dropped && attempt == 0 && (err.is_timeout() || err.is_connect()) =>
                {
                    tracing::warn!(error = %describe_reqwest_error(&err), "provider request failed, retrying");
                    tokio::time::sleep(clamp_backoff(2.0 * (attempt as f64 + 1.0))).await;
                    continue;
                }
                Err(err) => return Err(ProviderError::Http(describe_reqwest_error(&err))),
            };
            let status = response.status();
            let code = status.as_u16();
            if !is_retriable(code) {
                if !status.is_success() {
                    return Err(ProviderError::Status(code));
                }
                return response
                    .json::<Value>()
                    .await
                    .map_err(|e| ProviderError::Http(describe_reqwest_error(&e)));
            }
            last_status = code;
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
        Err(ProviderError::Status(last_status))
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

    /// Jikan is the preferred anime source (its MAL ids also give episode
    /// titles), but it proxies MyAnimeList live and regularly times out or
    /// answers 504. AniList is the fallback for anime, then TVMaze, which lists
    /// most anime series too. The first error is reported only when nothing
    /// answered with candidates.
    async fn search_anime(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        let mut outcomes = Vec::new();
        let mut errors = 0;
        for (source, label) in [
            ("jikan", "Jikan"),
            ("anilist", "AniList"),
            ("tvmaze", "TVMaze"),
        ] {
            let result = match source {
                "jikan" => self.search_jikan(query).await,
                "anilist" => self.search_anilist(query).await,
                _ => self.search_tvmaze(query).await,
            };
            match result {
                Ok(candidates) if !candidates.is_empty() => return Ok(candidates),
                Ok(_) => outcomes.push(format!("{label}: no match")),
                Err(err) => {
                    tracing::warn!(source, error = %err, "anime search failed, trying next provider");
                    errors += 1;
                    outcomes.push(format!("{label}: {err}"));
                }
            }
        }
        if errors == 0 {
            return Ok(Vec::new());
        }
        // Name every provider tried, so a failure shows whether the fallbacks
        // ran and what each one answered.
        Err(ProviderError::Other(format!(
            "no anime provider found \"{query}\" ({})",
            outcomes.join("; ")
        )))
    }

    async fn jikan_available(&self) -> bool {
        let mut down = self.jikan_down_until.lock().await;
        match *down {
            Some(until) if Instant::now() < until => false,
            Some(_) => {
                *down = None;
                true
            }
            None => true,
        }
    }

    /// Fetch from Jikan with its own short timeout, and put Jikan on cooldown
    /// when it does not answer or answers with a server error.
    async fn jikan_fetch(&self, key: &str, url: &str) -> Result<Value, ProviderError> {
        if let Some(cached) = self.db.get_cache(key) {
            return Ok(cached);
        }
        if !self.jikan_available().await {
            return Err(ProviderError::Other(
                "Jikan (MyAnimeList) is not responding; skipped for a few minutes.".to_string(),
            ));
        }
        self.throttle_jikan().await;
        match self
            // No retry on a stalled search: the same query stalls again, and
            // AniList is next in line.
            .send_json(|| self.client.get(url).timeout(JIKAN_TIMEOUT), false)
            .await
        {
            Ok(value) => {
                self.db.set_cache(key, &value);
                Ok(value)
            }
            Err(err) => {
                // A timeout can be specific to one slow search, so it does not
                // bench Jikan for everyone; a refused connection or a server
                // error does.
                let down = match &err {
                    ProviderError::Http(message) => !message.contains("timed out"),
                    ProviderError::Status(code) => *code == 429 || *code >= 500,
                    ProviderError::Other(_) => false,
                };
                if down {
                    *self.jikan_down_until.lock().await = Some(Instant::now() + JIKAN_COOLDOWN);
                }
                Err(err)
            }
        }
    }

    async fn search_jikan(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        // Jikan's full-text search stalls on some non-ASCII queries ("Rozé"
        // timed out where "Lelouch" answered at once), and MAL titles are
        // romanised anyway, so send it the accent-free form.
        let query = fold_accents(query);
        let key = format!("jikan:search:{}", query.to_lowercase());
        let url = format!(
            "https://api.jikan.moe/v4/anime?q={}&limit=10",
            urlencoding::encode(&query)
        );
        let data = self.jikan_fetch(&key, &url).await?;
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

    // ---- AniList (anime fallback) ----

    async fn search_anilist(&self, query: &str) -> Result<Vec<ShowCandidate>, ProviderError> {
        let key = format!("anilist:search:{}", query.to_lowercase());
        let data = match self.db.get_cache(&key) {
            Some(cached) => cached,
            None => {
                let body = serde_json::json!({
                    "query": ANILIST_SEARCH_QUERY,
                    "variables": { "search": query },
                });
                let value = self
                    .send_json(
                        || self.client.post("https://graphql.anilist.co").json(&body),
                        true,
                    )
                    .await?;
                if let Some(message) = value["errors"][0]["message"].as_str() {
                    return Err(ProviderError::Other(format!("AniList: {message}")));
                }
                self.db.set_cache(&key, &value);
                value
            }
        };
        Ok(anilist_candidates(&data))
    }

    async fn jikan_episodes(&self, show_id: &str) -> Result<Vec<EpisodeCandidate>, ProviderError> {
        let key = format!("jikan:episodes:{show_id}");
        let url = format!(
            "https://api.jikan.moe/v4/anime/{}/episodes",
            urlencoding::encode(show_id)
        );
        let data = self.jikan_fetch(&key, &url).await?;
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

/// Map AniList `Page.media` results to candidates.
fn anilist_candidates(data: &Value) -> Vec<ShowCandidate> {
    let Some(items) = data["data"]["Page"]["media"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .take(10)
        .map(|item| {
            let title = item["title"]["english"]
                .as_str()
                .or_else(|| item["title"]["romaji"].as_str())
                .unwrap_or("Unknown")
                .to_string();
            let year = item["seasonYear"]
                .as_i64()
                .or_else(|| item["startDate"]["year"].as_i64());
            let summary = strip_html(item["description"].as_str().unwrap_or(""));
            ShowCandidate {
                provider: "anilist".to_string(),
                provider_id: value_to_id(&item["id"]),
                title,
                year,
                summary: truncate(&summary, 240),
            }
        })
        .collect()
}

/// Strip Latin diacritics: "Rozé" -> "Roze". Other characters are kept.
fn fold_accents(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' => 'A',
            'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' => 'E',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ī' => 'I',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ø' => 'o',
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ō' | 'Ø' => 'O',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' => 'u',
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ū' => 'U',
            'ç' => 'c',
            'Ç' => 'C',
            'ñ' => 'n',
            'Ñ' => 'N',
            'ý' | 'ÿ' => 'y',
            'Ý' => 'Y',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reqwest_error_names_host_and_root_cause() {
        // Nothing listens on port 1: the connect is refused straight away.
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/v4/anime?q=secret%20query")
            .send()
            .await
            .unwrap_err();
        let message = describe_reqwest_error(&err);
        assert!(
            message.starts_with("could not connect to 127.0.0.1"),
            "{message}"
        );
        assert!(!message.contains("secret"), "{message}");
        assert!(
            message.len() > "could not connect to 127.0.0.1".len(),
            "{message}"
        );
    }

    #[test]
    fn folds_accents_for_jikan() {
        assert_eq!(
            fold_accents("Code Geass Rozé Of The Recapture"),
            "Code Geass Roze Of The Recapture"
        );
        assert_eq!(fold_accents("Pokémon Ça Ñ 東京"), "Pokemon Ca N 東京");
    }

    #[test]
    fn maps_anilist_results() {
        let data = serde_json::json!({"data": {"Page": {"media": [
            {"id": 139609, "title": {"romaji": "Code Geass: Dakkan no Rozé", "english": "Code Geass: Rozé of the Recapture"},
             "seasonYear": null, "startDate": {"year": 2024}, "description": "<i>Neo</i> Britannia."},
            {"id": 1, "title": {"romaji": "Only Romaji", "english": null}, "seasonYear": 2001, "startDate": {"year": 2000}, "description": null}
        ]}}});
        let found = anilist_candidates(&data);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].provider, "anilist");
        assert_eq!(found[0].provider_id, "139609");
        assert_eq!(found[0].title, "Code Geass: Rozé of the Recapture");
        assert_eq!(found[0].year, Some(2024));
        assert_eq!(found[0].summary, "Neo Britannia.");
        assert_eq!(found[1].title, "Only Romaji");
        assert_eq!(found[1].year, Some(2001));
    }

    /// Hits the real APIs: `cargo test live_anime -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_anime_lookup() {
        let dir = std::env::temp_dir().join(format!("tvsorter-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let providers = MetadataProviders::new(Database::open(&dir.join("test.db")).unwrap());
        for query in ["Code Geass Rozé Of The Recapture", "Code Geass Lelouch"] {
            let started = std::time::Instant::now();
            let jikan = providers.search_jikan(query).await.map(|c| c.len());
            println!("jikan   {query:?}: {jikan:?} in {:?}", started.elapsed());
            let started = std::time::Instant::now();
            let anilist = providers.search_anilist(query).await;
            println!(
                "anilist {query:?}: {:?} in {:?}",
                anilist.as_ref().map(|c| c.first().map(|c| c.title.clone())),
                started.elapsed()
            );
            assert!(!providers.search_anime(query).await.unwrap().is_empty());
        }
    }
}
